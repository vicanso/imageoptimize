use avif_decode::Decoder;
use image::codecs::avif;
use image::codecs::gif;
use image::{AnimationDecoder, DynamicImage, ImageEncoder, ImageFormat, RgbaImage};
use lodepng::Bitmap;
use rayon::prelude::*;
use rgb::{ComponentBytes, FromSlice, RGBA8};
use snafu::{ResultExt, Snafu};
use std::borrow::Cow;
use std::{
    ffi::OsStr,
    io::{BufRead, Seek},
};
use webp::Encoder;

#[derive(Debug, Snafu)]
pub enum ImageError {
    #[snafu(display("Handle image fail, category:{category}, message:{source}"))]
    Image {
        category: String,
        source: image::ImageError,
    },
    #[snafu(display("Handle image fail, category:{category}, message:{source}"))]
    ImageQuant {
        category: String,
        source: imagequant::Error,
    },
    #[snafu(display("Handle image fail, category:{category}, message:{source}"))]
    AvifDecode {
        category: String,
        source: avif_decode::Error,
    },
    #[snafu(display("Handle image fail, category:{category}, message:{source}"))]
    LodePNG {
        category: String,
        source: lodepng::Error,
    },
    #[snafu(display("Io fail, {source}"))]
    Io { source: std::io::Error },
    #[snafu(display("{message}"))]
    Unsupported { message: String },
    #[snafu(display("Handle image fail, category:{category}, message:{message}"))]
    Encode { category: String, message: String },
    #[snafu(display("Handle image fail"))]
    Unknown,
}

type Result<T, E = ImageError> = std::result::Result<T, E>;

/// Holds a decoded image ready for encoding. The channel layout of the backing
/// `DynamicImage` is preserved (opaque inputs stay RGB8), so encoders can borrow
/// the raw bytes without re-adding an alpha plane. `opaque` is computed once at
/// construction so per-encode calls don't rescan the pixels.
pub struct ImageInfo {
    pub image: DynamicImage,
    opaque: bool,
}

impl ImageInfo {
    fn from_dynamic(image: DynamicImage) -> Self {
        match image {
            // A fully opaque RGBA8 image (e.g. after padding or flattening) is dropped to
            // RGB8 once here, so the JPEG/WebP/AVIF encoders — called many times during an
            // auto-quality search — borrow the bytes instead of re-converting on every call.
            DynamicImage::ImageRgba8(img) if img.as_raw().par_chunks(4).all(|p| p[3] == 255) => {
                ImageInfo {
                    image: DynamicImage::ImageRgb8(DynamicImage::ImageRgba8(img).to_rgb8()),
                    opaque: true,
                }
            }
            other => {
                let opaque = !other.color().has_alpha();
                ImageInfo {
                    image: other,
                    opaque,
                }
            }
        }
    }

    pub fn width(&self) -> usize {
        self.image.width() as usize
    }
    pub fn height(&self) -> usize {
        self.image.height() as usize
    }

    /// True when any pixel carries transparency, so an alpha-capable format is required.
    pub fn has_alpha(&self) -> bool {
        !self.opaque
    }

    /// Borrow the image as RGBA8 bytes, converting only when it is not already RGBA8.
    fn rgba_bytes(&self) -> Cow<'_, [u8]> {
        match &self.image {
            DynamicImage::ImageRgba8(img) => Cow::Borrowed(img.as_raw()),
            other => Cow::Owned(other.to_rgba8().into_raw()),
        }
    }

    /// Borrow the image as RGB8 bytes (alpha dropped), converting only when it is
    /// not already RGB8.
    fn rgb_bytes(&self) -> Cow<'_, [u8]> {
        match &self.image {
            DynamicImage::ImageRgb8(img) => Cow::Borrowed(img.as_raw()),
            other => Cow::Owned(other.to_rgb8().into_raw()),
        }
    }
}

impl From<Bitmap<RGBA8>> for ImageInfo {
    fn from(info: Bitmap<RGBA8>) -> Self {
        let raw = info.buffer.as_bytes().to_vec();
        let image =
            RgbaImage::from_raw(info.width as u32, info.height as u32, raw).unwrap_or_default();
        ImageInfo::from_dynamic(DynamicImage::ImageRgba8(image))
    }
}

impl From<RgbaImage> for ImageInfo {
    fn from(image: RgbaImage) -> Self {
        ImageInfo::from_dynamic(DynamicImage::ImageRgba8(image))
    }
}

impl From<DynamicImage> for ImageInfo {
    fn from(image: DynamicImage) -> Self {
        ImageInfo::from_dynamic(image)
    }
}

/// Decode data from avif format, it supports rgb8,
/// rgba8, rgb16 and rgba16.
pub fn avif_decode(data: &[u8]) -> Result<DynamicImage> {
    let avif_result = Decoder::from_avif(data)
        .context(AvifDecodeSnafu {
            category: "decode".to_string(),
        })?
        .to_image()
        .context(AvifDecodeSnafu {
            category: "decode".to_string(),
        })?;
    match avif_result {
        avif_decode::Image::Rgb8(img) => {
            let (width, height) = (img.width() as u32, img.height() as u32);
            let pixels = img.buf();
            let mut buf = Vec::with_capacity(pixels.len() * 3);
            buf.extend(pixels.iter().flat_map(|p| [p.r, p.g, p.b]));
            image::RgbImage::from_raw(width, height, buf)
                .ok_or(ImageError::Unknown)
                .map(DynamicImage::ImageRgb8)
        }
        avif_decode::Image::Rgba8(img) => {
            let (width, height) = (img.width() as u32, img.height() as u32);
            let pixels = img.buf();
            let mut buf = Vec::with_capacity(pixels.len() * 4);
            buf.extend(pixels.iter().flat_map(|p| [p.r, p.g, p.b, p.a]));
            image::RgbaImage::from_raw(width, height, buf)
                .ok_or(ImageError::Unknown)
                .map(DynamicImage::ImageRgba8)
        }
        avif_decode::Image::Rgba16(img) => {
            let (width, height) = (img.width() as u32, img.height() as u32);
            let pixels = img.buf();
            let mut buf = Vec::with_capacity(pixels.len() * 4);
            buf.extend(pixels.iter().flat_map(|p| {
                [
                    (p.r / 257) as u8,
                    (p.g / 257) as u8,
                    (p.b / 257) as u8,
                    (p.a / 257) as u8,
                ]
            }));
            image::RgbaImage::from_raw(width, height, buf)
                .ok_or(ImageError::Unknown)
                .map(DynamicImage::ImageRgba8)
        }
        avif_decode::Image::Rgb16(img) => {
            let (width, height) = (img.width() as u32, img.height() as u32);
            let pixels = img.buf();
            let mut buf = Vec::with_capacity(pixels.len() * 3);
            buf.extend(
                pixels
                    .iter()
                    .flat_map(|p| [(p.r / 257) as u8, (p.g / 257) as u8, (p.b / 257) as u8]),
            );
            image::RgbImage::from_raw(width, height, buf)
                .ok_or(ImageError::Unknown)
                .map(DynamicImage::ImageRgb8)
        }
        _ => Err(ImageError::Unknown),
    }
}

/// Decode data from JXL format using jpegxl-rs (libjxl FFI). The pixels are in the image's
/// own color space; see `jxl_decode_with_profile` for the profile describing it.
pub fn jxl_decode(data: &[u8]) -> Result<DynamicImage> {
    jxl_decode_with_profile(data).map(|(di, _)| di)
}

/// Decode JXL data, returning the pixels along with the ICC profile that describes them
/// (libjxl synthesizes one for images stored with an enumerated color space).
#[cfg(feature = "jxl")]
pub(crate) fn jxl_decode_with_profile(data: &[u8]) -> Result<(DynamicImage, Option<Vec<u8>>)> {
    let decoder = jpegxl_rs::decoder_builder()
        .icc_profile(true)
        .build()
        .map_err(|_| ImageError::Unknown)?;
    let (info, pixels) = decoder
        .decode_with::<u8>(data)
        .map_err(|_| ImageError::Unknown)?;
    let w = info.width;
    let h = info.height;
    // Determine channels from pixel buffer length rather than metadata field name
    // (jpegxl-rs Metadata doesn't expose num_channels directly in 0.11).
    let image = if pixels.len() == (w * h * 4) as usize {
        image::RgbaImage::from_raw(w, h, pixels).map(DynamicImage::ImageRgba8)
    } else {
        image::RgbImage::from_raw(w, h, pixels).map(DynamicImage::ImageRgb8)
    };
    Ok((image.ok_or(ImageError::Unknown)?, info.icc_profile))
}

/// Stub used when the `jxl` feature is disabled: JXL inputs report a clear error.
#[cfg(not(feature = "jxl"))]
pub(crate) fn jxl_decode_with_profile(_data: &[u8]) -> Result<(DynamicImage, Option<Vec<u8>>)> {
    Err(ImageError::Unsupported {
        message: "JXL decoding requires the `jxl` feature".to_string(),
    })
}

pub fn load<R: BufRead + Seek>(r: R, ext: &str) -> Result<ImageInfo> {
    let format = ImageFormat::from_extension(OsStr::new(ext)).unwrap_or(ImageFormat::Jpeg);
    let result = image::load(r, format).context(ImageSnafu { category: "load" })?;
    // Preserve the decoded channel layout (opaque images stay RGB8) so encoders
    // can skip the alpha plane.
    Ok(result.into())
}

/// The rav1e speed `to_avif` encodes at: 0 selects the default (3), 1–10 are used as given.
pub(crate) fn avif_speed(speed: u8) -> u8 {
    if speed == 0 {
        3
    } else {
        speed
    }
}

/// The GIF encoder panics outside 1..=30, so map any `speed` (0 included) into range.
fn gif_speed(speed: u8) -> i32 {
    speed.clamp(1, 30) as i32
}

/// Signature box that opens a JPEG XL ISOBMFF container.
const JXL_CONTAINER_SIG: &[u8] = &[
    0, 0, 0, 0x0c, b'J', b'X', b'L', b' ', 0x0d, 0x0a, 0x87, 0x0a,
];

/// One ISOBMFF box (the container format of JPEG XL and AVIF): its 4-byte type, payload,
/// and the whole box as stored.
struct IsoBox<'a> {
    kind: &'a [u8],
    payload: &'a [u8],
    raw: &'a [u8],
}

/// The sequence of ISOBMFF boxes laid out in `data`. A box running past the end of the data
/// is clamped to it.
fn iso_boxes(data: &[u8]) -> impl Iterator<Item = IsoBox<'_>> {
    let mut rest = data;
    std::iter::from_fn(move || {
        if rest.len() < 8 {
            return None;
        }
        let size = u32::from_be_bytes(rest[0..4].try_into().ok()?) as usize;
        let (header, size) = match size {
            1 => (
                16,
                usize::try_from(u64::from_be_bytes(rest.get(8..16)?.try_into().ok()?)).ok()?,
            ),
            0 => (8, rest.len()),
            n => (8, n),
        };
        let end = size.max(header).min(rest.len());
        let iso_box = IsoBox {
            kind: &rest[4..8],
            payload: rest.get(header..end)?,
            raw: &rest[..end],
        };
        rest = &rest[end..];
        Some(iso_box)
    })
}

/// The boxes following the signature of a JPEG XL container, or `None` when `data` is not
/// one (e.g. a bare codestream).
fn jxl_boxes(data: &[u8]) -> Option<impl Iterator<Item = IsoBox<'_>>> {
    data.strip_prefix(JXL_CONTAINER_SIG).map(iso_boxes)
}

/// How an image says its pixels are to be interpreted.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ColorProfile {
    /// An embedded ICC profile.
    Icc(Vec<u8>),
    /// CICP code points (ITU-T H.273): color primaries and transfer characteristics.
    Cicp { primaries: u8, transfer: u8 },
}

/// The color information an AVIF carries in its `colr` property (`meta` → `iprp` → `ipco`):
/// the ICC profile when one is embedded, otherwise the CICP code points. `None` when there is
/// no `colr` box — the usual case for sRGB files, this crate's own output included.
pub(crate) fn avif_color_profile(data: &[u8]) -> Option<ColorProfile> {
    fn child<'a>(boxes: &'a [u8], kind: &[u8]) -> Option<&'a [u8]> {
        iso_boxes(boxes).find(|b| b.kind == kind).map(|b| b.payload)
    }
    // `meta` is a FullBox: 4 bytes of version and flags precede its children.
    let meta = child(data, b"meta")?.get(4..)?;
    let ipco = child(child(meta, b"iprp")?, b"ipco")?;
    let mut cicp = None;
    for colr in iso_boxes(ipco).filter(|b| b.kind == b"colr") {
        let Some((kind, body)) = colr.payload.split_at_checked(4) else {
            continue;
        };
        match (kind, body) {
            // An ICC profile wins over code points when a file carries both.
            (b"prof" | b"rICC", icc) => return Some(ColorProfile::Icc(icc.to_vec())),
            // Two 16-bit code points; every defined value fits the low byte.
            (b"nclx", &[0, primaries, 0, transfer, ..]) if cicp.is_none() => {
                cicp = Some(ColorProfile::Cicp {
                    primaries,
                    transfer,
                });
            }
            _ => {}
        }
    }
    cicp
}

/// Remove the metadata boxes of a JPEG XL container: `Exif`, `xml ` (XMP), their brotli-
/// compressed `brob` form, and `jbrd` — the JPEG reconstruction data, which also carries the
/// source JPEG's other APP markers and can't rebuild the file once its EXIF is gone. The
/// image itself is untouched. Returns `None` when there is nothing to remove (bare
/// codestreams included).
pub(crate) fn strip_jxl_metadata(data: &[u8]) -> Option<Vec<u8>> {
    const METADATA: [&[u8]; 3] = [b"Exif", b"xml ", b"jbrd"];
    let is_metadata = |b: &IsoBox| {
        METADATA.contains(&b.kind)
            || (b.kind == b"brob" && b.payload.get(..4).is_some_and(|t| METADATA.contains(&t)))
    };
    if !jxl_boxes(data)?.any(|b| is_metadata(&b)) {
        return None;
    }
    let mut out = JXL_CONTAINER_SIG.to_vec();
    for b in jxl_boxes(data)?.filter(|b| !is_metadata(b)) {
        out.extend_from_slice(b.raw);
    }
    Some(out)
}

/// Losslessly recompress a JPEG as JPEG XL, typically ~20% smaller. The JPEG's DCT
/// coefficients are kept as they are, so the decoded pixels match the JPEG's, and the stored
/// reconstruction data lets the original file be rebuilt bit for bit. EXIF/XMP are carried
/// over; [`strip_exif_bytes`](crate::strip_exif_bytes) with `"jxl"` removes them.
#[cfg(feature = "jxl")]
pub fn jpeg_to_jxl(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = jpegxl_rs::encoder_builder()
        .use_container(true)
        .build()
        .map_err(|_| ImageError::Unknown)?;
    let result = encoder.encode_jpeg(data).map_err(|e| ImageError::Encode {
        category: "jxl_transcode".to_string(),
        message: format!("{e:?}"),
    })?;
    Ok(result.data)
}

/// Stub used when the `jxl` feature is disabled.
#[cfg(not(feature = "jxl"))]
pub fn jpeg_to_jxl(_data: &[u8]) -> Result<Vec<u8>> {
    Err(ImageError::Unsupported {
        message: "JXL encoding requires the `jxl` feature".to_string(),
    })
}

/// Width and height from a JPEG XL header (bare codestream or ISOBMFF container), read
/// without decoding so oversized images can be rejected before libjxl allocates for them.
/// Returns `None` when the header can't be parsed.
pub(crate) fn jxl_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let codestream = if data.starts_with(&[0xff, 0x0a]) {
        data
    } else {
        jxl_boxes(data)?.find_map(|b| match b.kind {
            b"jxlc" => Some(b.payload),
            // Partial codestream boxes carry a 4-byte index before the data.
            b"jxlp" => b.payload.get(4..),
            _ => None,
        })?
    };
    if !codestream.starts_with(&[0xff, 0x0a]) {
        return None;
    }

    // SizeHeader (ISO/IEC 18181-1), bits read least-significant first.
    let bytes = &codestream[2..];
    let mut pos = 0usize;
    let mut bits = |n: usize| -> Option<u32> {
        let mut v = 0u32;
        for i in 0..n {
            let byte = *bytes.get(pos / 8)?;
            v |= u32::from((byte >> (pos % 8)) & 1) << i;
            pos += 1;
        }
        Some(v)
    };
    // A dimension stored as `U32(Bits(9), Bits(13), Bits(18), Bits(30))` minus one.
    fn u32_field(bits: &mut impl FnMut(usize) -> Option<u32>) -> Option<u32> {
        let width = [9, 13, 18, 30][bits(2)? as usize];
        Some(bits(width)? + 1)
    }
    let div8 = bits(1)? == 1;
    let height = if div8 {
        (bits(5)? + 1) * 8
    } else {
        u32_field(&mut bits)?
    };
    let ratio = bits(3)?;
    let h = height as u64;
    let width = match ratio {
        0 if div8 => (bits(5)? + 1) * 8,
        0 => u32_field(&mut bits)?,
        1 => height,
        2 => (h * 12 / 10) as u32,
        3 => (h * 4 / 3) as u32,
        4 => (h * 3 / 2) as u32,
        5 => (h * 16 / 9) as u32,
        6 => (h * 5 / 4) as u32,
        _ => (h * 2) as u32,
    };
    Some((width, height))
}

/// Re-encode an animated GIF as an animated WebP, keeping every frame and its timing.
/// `quality` >= 100 encodes losslessly. Returns `None` for a single-frame GIF so the caller
/// can use the regular still-image encoder instead.
pub fn gif_to_animated_webp<R>(r: R, quality: u8) -> Result<Option<Vec<u8>>>
where
    R: std::io::BufRead,
    R: std::io::Seek,
{
    let decoder = gif::GifDecoder::new(r).context(ImageSnafu {
        category: "gif_decode",
    })?;
    let frames = decoder.into_frames().collect_frames().context(ImageSnafu {
        category: "gif_decode",
    })?;
    if frames.len() <= 1 {
        return Ok(None);
    }
    let (width, height) = frames[0].buffer().dimensions();
    let mut config = webp::WebPConfig::new().map_err(|_| ImageError::Encode {
        category: "webp_anim_config".to_string(),
        message: "failed to initialise WebP config".to_string(),
    })?;
    if quality >= 100 {
        config.lossless = 1;
    } else {
        config.quality = quality as f32;
    }
    let mut encoder = webp::AnimEncoder::new(width, height, &config);
    encoder.set_loop_count(0);
    // Frames are composited to the full canvas by the decoder; timestamps are cumulative.
    let mut timestamp_ms = 0i32;
    for frame in &frames {
        let buf = frame.buffer();
        encoder.add_frame(webp::AnimFrame::from_rgba(
            buf.as_raw(),
            buf.width(),
            buf.height(),
            timestamp_ms,
        ));
        let (numer, denom) = frame.delay().numer_denom_ms();
        timestamp_ms = timestamp_ms.saturating_add((numer / denom.max(1)) as i32);
    }
    let data = encoder.try_encode().map_err(|e| ImageError::Encode {
        category: "webp_anim_encode".to_string(),
        message: format!("{e:?}"),
    })?;
    Ok(Some(data.to_vec()))
}

pub fn to_gif<R>(r: R, speed: u8) -> Result<Vec<u8>>
where
    R: std::io::BufRead,
    R: std::io::Seek,
{
    let decoder = gif::GifDecoder::new(r).context(ImageSnafu {
        category: "gif_decode",
    })?;
    let frames = decoder.into_frames();

    let mut w = Vec::new();

    {
        let mut encoder = gif::GifEncoder::new_with_speed(&mut w, gif_speed(speed));
        encoder
            .set_repeat(gif::Repeat::Infinite)
            .context(ImageSnafu {
                category: "gif_set_repeat",
            })?;
        encoder.try_encode_frames(frames).context(ImageSnafu {
            category: "gif_encode",
        })?;
    }

    Ok(w)
}

impl ImageInfo {
    /// Encode the image as a single-frame GIF (NeuQuant palette). Used when the pixels no
    /// longer match an original GIF buffer — e.g. after a resize, or converting from another
    /// format. `speed` trades palette quality for speed (clamped to 1..=30).
    pub fn to_gif(&self, speed: u8) -> Result<Vec<u8>> {
        let mut w = Vec::new();
        {
            let mut encoder = gif::GifEncoder::new_with_speed(&mut w, gif_speed(speed));
            let frame = image::Frame::new(self.image.to_rgba8());
            encoder.encode_frame(frame).context(ImageSnafu {
                category: "gif_encode",
            })?;
        }
        Ok(w)
    }

    /// Optimize image to png. `quality` below 100 quantizes to a palette of at most 256
    /// colors (lossy, best effort: it never aborts); `quality` >= 100 encodes losslessly.
    pub fn to_png(&self, quality: u8) -> Result<Vec<u8>> {
        let rgba = self.rgba_bytes();
        let pixels: &[RGBA8] = rgba.as_rgba();
        let width = self.width();
        let height = self.height();

        if quality >= 100 {
            // No quantization. lodepng still picks the smallest color type that holds every
            // pixel exactly (palette for <= 256 colors, gray, no alpha when opaque, …).
            // Maximum deflate effort and entropy-based filters trade encode time for size.
            let mut enc = lodepng::Encoder::new();
            enc.settings_mut().set_level(9);
            enc.set_filter_strategy(lodepng::FilterStrategy::ENTROPY, true);
            return enc.encode(pixels, width, height).context(LodePNGSnafu {
                category: "png_encode",
            });
        }

        let mut liq = imagequant::new();
        liq.set_quality(0, quality).context(ImageQuantSnafu {
            category: "png_set_quality",
        })?;

        let mut img = liq
            .new_image(pixels, width, height, 0.0)
            .context(ImageQuantSnafu {
                category: "png_new_image",
            })?;

        let mut res = liq.quantize(&mut img).context(ImageQuantSnafu {
            category: "png_quantize",
        })?;

        res.set_dithering_level(1.0).context(ImageQuantSnafu {
            category: "png_set_level",
        })?;

        let (palette, pixels) = res.remapped(&mut img).context(ImageQuantSnafu {
            category: "png_remapped",
        })?;
        let mut enc = lodepng::Encoder::new();
        enc.set_palette(&palette).context(LodePNGSnafu {
            category: "png_encoder",
        })?;

        let buf = enc.encode(&pixels, width, height).context(LodePNGSnafu {
            category: "png_encode",
        })?;

        Ok(buf)
    }

    /// Optimize image to webp. quality >= 100 produces lossless output;
    /// any lower value encodes lossy at that quality (0–99).
    pub fn to_webp(&self, quality: u8) -> Result<Vec<u8>> {
        let width = self.image.width();
        let height = self.image.height();
        // Opaque images encode as RGB so no useless alpha plane is written (smaller, faster).
        let bytes = if self.opaque {
            self.rgb_bytes()
        } else {
            self.rgba_bytes()
        };
        let encoder = if self.opaque {
            Encoder::from_rgb(bytes.as_ref(), width, height)
        } else {
            Encoder::from_rgba(bytes.as_ref(), width, height)
        };
        // Lossless also goes through libwebp: its encoder compresses 25–45% smaller than
        // the pure-Rust image-webp one, at a higher CPU cost. For lossless, `quality` is the
        // effort setting (75 = libwebp's default).
        let lossless = quality >= 100;
        let effort = if lossless { 75.0 } else { quality as f32 };
        let data = encoder
            .encode_simple(lossless, effort)
            .map_err(|e| ImageError::Encode {
                category: "webp_encode".to_string(),
                message: format!("{e:?}"),
            })?;
        Ok(data.to_vec())
    }

    /// Optimize image to avif.
    /// `speed` accepts a value in the range 0-10, where 0 is the slowest and 10 is the fastest.
    /// `quality` accepts a value in the range 0-100, where 0 is the worst and 100 is the best.
    pub fn to_avif(&self, quality: u8, speed: u8) -> Result<Vec<u8>> {
        let mut w = Vec::new();
        let sp = avif_speed(speed);
        let width = self.image.width();
        let height = self.image.height();
        // Opaque images skip the alpha plane (smaller output, faster encode).
        let (bytes, color) = if self.opaque {
            (self.rgb_bytes(), image::ColorType::Rgb8)
        } else {
            (self.rgba_bytes(), image::ColorType::Rgba8)
        };

        let img = avif::AvifEncoder::new_with_speed_quality(&mut w, sp, quality);
        img.write_image(bytes.as_ref(), width, height, color.into())
            .context(ImageSnafu {
                category: "avif_encode",
            })?;

        Ok(w)
    }

    /// Encode image to JPEG XL.
    /// quality >= 100 = lossless; 0–99 = lossy mapped to JXL psychovisual distance
    /// (distance 0 = best, 15 = worst; quality 80 ≈ distance 3.0).
    #[cfg(feature = "jxl")]
    pub fn to_jxl(&self, quality: u8) -> Result<Vec<u8>> {
        use jpegxl_rs::encode::EncoderFrame;
        let width = self.image.width();
        let height = self.image.height();
        // Opaque images encode as RGB (3 channels). Images with transparency keep their
        // alpha as a JXL extra channel (4 channels) so PNG → JXL stays visually correct
        // (jpegxl-rs 0.14 exposes this via has_alpha + a 4-channel EncoderFrame).
        let has_alpha = !self.opaque;
        let (pixels, channels): (Cow<'_, [u8]>, u32) = if has_alpha {
            (self.rgba_bytes(), 4)
        } else {
            (self.rgb_bytes(), 3)
        };
        // quality 0 → distance 15, quality 99 → distance ~0.15, quality >= 100 → lossless
        let mut encoder = if quality >= 100 {
            // Lossless requires uses_original_profile=true; set it explicitly so
            // JxlEncoderSetBasicInfo receives the correct value before frame encoding.
            jpegxl_rs::encoder_builder()
                .has_alpha(has_alpha)
                .lossless(true)
                .uses_original_profile(true)
                .build()
                .map_err(|_| ImageError::Unknown)?
        } else {
            let distance = (100 - quality) as f32 * 15.0 / 100.0;
            jpegxl_rs::encoder_builder()
                .has_alpha(has_alpha)
                .quality(distance)
                .build()
                .map_err(|_| ImageError::Unknown)?
        };
        let frame = EncoderFrame::new(pixels.as_ref()).num_channels(channels);
        let result = encoder
            .encode_frame::<u8, u8>(&frame, width, height)
            .map_err(|_| ImageError::Unknown)?;
        Ok(result.to_vec())
    }

    /// Stub used when the `jxl` feature is disabled.
    #[cfg(not(feature = "jxl"))]
    pub fn to_jxl(&self, _quality: u8) -> Result<Vec<u8>> {
        Err(ImageError::Unsupported {
            message: "JXL encoding requires the `jxl` feature".to_string(),
        })
    }

    /// Optimize image to jpeg, the quality 60-80 are recommended.
    pub fn to_mozjpeg(&self, quality: u8) -> Result<Vec<u8>> {
        let mut comp = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
        comp.set_size(self.width(), self.height());
        comp.set_quality(quality as f32);
        let mut comp = comp.start_compress(Vec::new()).context(IoSnafu {})?;
        comp.write_scanlines(self.rgb_bytes().as_ref())
            .context(IoSnafu {})?;
        let data = comp.finish().context(IoSnafu {})?;
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::{load, ImageInfo};
    use pretty_assertions::assert_eq;

    use std::io::Cursor;
    fn load_image() -> ImageInfo {
        let data = include_bytes!("../assets/rust-logo.png");
        load(Cursor::new(data), "png").unwrap()
    }

    #[test]
    fn test_load_image() {
        let img = load_image();
        assert_eq!(img.height(), 144);
        assert_eq!(img.width(), 144);
    }
    /// Decode encoder output and check it round-trips to the source dimensions. Exact byte
    /// counts are deliberately not asserted: they shift with every encoder release.
    fn assert_decodes(bytes: &[u8]) -> image::DynamicImage {
        assert!(!bytes.is_empty());
        let di = image::load_from_memory(bytes).unwrap();
        assert_eq!((di.width(), di.height()), (144, 144));
        di
    }

    #[test]
    fn test_to_png() {
        let img = load_image();
        let source = include_bytes!("../assets/rust-logo.png");
        // Quantized to a palette: smaller, but not pixel-exact (the logo's anti-aliased edges
        // hold more than 256 colors).
        let result = img.to_png(90).unwrap();
        let decoded = assert_decodes(&result);
        assert!(result.len() < source.len());
        assert_ne!(decoded.to_rgba8(), img.image.to_rgba8());
        // Quality 100 is lossless, and still beats the unoptimized source.
        let lossless = img.to_png(100).unwrap();
        let decoded = assert_decodes(&lossless);
        assert_eq!(decoded.to_rgba8(), img.image.to_rgba8());
        assert!(lossless.len() < source.len());
        assert!(lossless.len() > result.len());
    }
    #[test]
    fn test_to_webp() {
        let img = load_image();
        // lossless: pixels survive exactly
        let lossless = img.to_webp(100).unwrap();
        let decoded = assert_decodes(&lossless);
        assert_eq!(decoded.to_rgba8(), img.image.to_rgba8());
        // lossy: a VP8 (not VP8L) bitstream. Neither size nor pixels tell the two apart on
        // this black-only logo: libwebp's lossless output is the smaller file, and lossy
        // VP8 happens to reproduce it exactly.
        let lossy = img.to_webp(80).unwrap();
        assert_decodes(&lossy);
        let has_chunk = |buf: &[u8], id: &[u8]| buf.windows(4).any(|w| w == id);
        assert!(has_chunk(&lossless, b"VP8L") && !has_chunk(&lossless, b"VP8 "));
        assert!(has_chunk(&lossy, b"VP8 ") && !has_chunk(&lossy, b"VP8L"));
    }
    #[test]
    fn test_to_jpeg() {
        let img = load_image();
        let result = img.to_mozjpeg(90).unwrap();
        assert_decodes(&result);
    }
    #[test]
    fn test_to_avif() {
        let img = load_image();
        let result = img.to_avif(90, 3).unwrap();
        let decoded = super::avif_decode(&result).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (144, 144));
    }
    #[test]
    fn test_to_gif() {
        let img = load_image();
        // speed 0 used to panic inside the GIF encoder
        let result = img.to_gif(0).unwrap();
        assert_decodes(&result);
    }
    #[test]
    fn test_gif_to_animated_webp() {
        use image::{codecs::gif::GifEncoder, Delay, Frame, Rgba, RgbaImage};
        let frame = |v: u8| {
            Frame::from_parts(
                RgbaImage::from_pixel(8, 6, Rgba([v, 0, 0, 255])),
                0,
                0,
                Delay::from_numer_denom_ms(100, 1),
            )
        };
        let mut gif = Vec::new();
        GifEncoder::new(&mut gif)
            .encode_frames([frame(10), frame(200)])
            .unwrap();
        let webp = super::gif_to_animated_webp(Cursor::new(&gif), 80)
            .unwrap()
            .expect("two frames → animated webp");
        let anim = webp::AnimDecoder::new(&webp).decode().unwrap();
        assert_eq!(anim.len(), 2);

        // A single-frame GIF is left to the still-image encoder.
        let mut still = Vec::new();
        GifEncoder::new(&mut still)
            .encode_frames([frame(10)])
            .unwrap();
        assert!(super::gif_to_animated_webp(Cursor::new(&still), 80)
            .unwrap()
            .is_none());
    }
    #[test]
    fn test_avif_color_profile() {
        use super::{avif_color_profile, ColorProfile};
        let boxed = |kind: &[u8; 4], payload: &[u8]| {
            let mut b = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
            b.extend(kind);
            b.extend(payload);
            b
        };
        // ftyp, meta (a FullBox) → iprp → ipco → the given properties, then mdat.
        let avif = |properties: &[Vec<u8>]| {
            let ipco = [boxed(b"ispe", &[0; 12]), properties.concat()].concat();
            let iprp = boxed(b"ipco", &ipco);
            let meta = [vec![0; 4], boxed(b"hdlr", b"pict"), boxed(b"iprp", &iprp)].concat();
            [
                boxed(b"ftyp", b"avif"),
                boxed(b"meta", &meta),
                boxed(b"mdat", &[1, 2, 3]),
            ]
            .concat()
        };
        let nclx = |primaries: u8, transfer: u8| {
            let codes = [0, primaries, 0, transfer, 0, 6, 0x80];
            boxed(b"colr", &[b"nclx".as_slice(), &codes].concat())
        };
        let icc = |kind: &[u8; 4]| boxed(b"colr", &[kind.as_slice(), b"ICC-BYTES"].concat());
        let embedded = Some(ColorProfile::Icc(b"ICC-BYTES".to_vec()));

        assert_eq!(avif_color_profile(&avif(&[])), None);
        assert_eq!(
            avif_color_profile(&avif(&[nclx(12, 13)])),
            Some(ColorProfile::Cicp {
                primaries: 12,
                transfer: 13
            })
        );
        assert_eq!(avif_color_profile(&avif(&[icc(b"prof")])), embedded);
        assert_eq!(avif_color_profile(&avif(&[icc(b"rICC")])), embedded);
        // An ICC profile wins over code points, whichever comes first.
        assert_eq!(
            avif_color_profile(&avif(&[nclx(1, 13), icc(b"prof")])),
            embedded
        );
        // Truncated or foreign data is simply "no profile".
        assert_eq!(avif_color_profile(&avif(&[boxed(b"colr", b"nc")])), None);
        assert_eq!(
            avif_color_profile(&avif(&[boxed(b"colr", b"nclx\0")])),
            None
        );
        assert_eq!(avif_color_profile(b"not an avif file"), None);
        // This crate's own AVIF output carries no colr box.
        let own = load_image().to_avif(80, 10).unwrap();
        assert_eq!(avif_color_profile(&own), None);
    }
    #[test]
    #[cfg(feature = "jxl")]
    fn test_jxl_dimensions() {
        use image::{DynamicImage, RgbImage};
        for (w, h) in [(144, 144), (300, 77), (64, 48), (1000, 3), (17, 1200)] {
            let info: ImageInfo = DynamicImage::ImageRgb8(RgbImage::new(w, h)).into();
            let jxl = info.to_jxl(80).unwrap();
            assert_eq!(super::jxl_dimensions(&jxl), Some((w, h)), "{w}x{h}");
        }
        assert_eq!(super::jxl_dimensions(b"not a jxl"), None);
    }
    #[test]
    #[cfg(feature = "jxl")]
    fn test_jpeg_to_jxl() {
        use img_parts::{jpeg::Jpeg, ImageEXIF};
        // A JPEG carrying EXIF (a minimal little-endian TIFF header with an empty IFD).
        let jpeg = load_image().to_mozjpeg(90).unwrap();
        let mut parts = Jpeg::from_bytes(jpeg.into()).unwrap();
        parts.set_exif(Some(b"II*\0\x08\0\0\0\0\0\0\0\0\0".to_vec().into()));
        let jpeg = parts.encoder().bytes().to_vec();

        let jxl = super::jpeg_to_jxl(&jpeg).unwrap();
        assert_eq!(super::jxl_dimensions(&jxl), Some((144, 144)));
        let has_box = |data: &[u8], kind: &[u8]| {
            super::jxl_boxes(data).is_some_and(|mut boxes| {
                boxes.any(|b| b.kind == kind || (b.kind == b"brob" && b.payload.starts_with(kind)))
            })
        };
        // Reconstruction data and the EXIF block are carried over.
        assert!(has_box(&jxl, b"jbrd"));
        assert!(has_box(&jxl, b"Exif"));

        let stripped = super::strip_jxl_metadata(&jxl).unwrap();
        assert!(!has_box(&stripped, b"jbrd") && !has_box(&stripped, b"Exif"));
        assert!(stripped.len() < jxl.len());
        // Still the same image, and nothing left to strip.
        let decoded = super::jxl_decode(&stripped).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (144, 144));
        assert_eq!(super::jxl_dimensions(&stripped), Some((144, 144)));
        assert_eq!(super::strip_jxl_metadata(&stripped), None);
        // A bare codestream has no boxes to strip.
        assert_eq!(
            super::strip_jxl_metadata(&load_image().to_jxl(80).unwrap()),
            None
        );
    }
    #[test]
    #[cfg(feature = "jxl")]
    fn test_to_jxl() {
        let img = load_image();
        // lossy
        let lossy = img.to_jxl(80).unwrap();
        assert_ne!(lossy.len(), 0);
        // lossless
        let lossless = img.to_jxl(100).unwrap();
        assert_ne!(lossless.len(), 0);
        // The source PNG is transparent; the lossless round-trip must keep its alpha
        // (regression guard — alpha was previously dropped on the RGB-only encode path).
        let decoded = super::jxl_decode(&lossless).unwrap();
        assert_eq!(decoded.width(), 144);
        assert_eq!(decoded.height(), 144);
        let rgba = decoded.to_rgba8();
        assert!(
            rgba.pixels().any(|p| p.0[3] < 255),
            "alpha channel was lost during JXL encode"
        );
    }
}
