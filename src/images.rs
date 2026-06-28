use avif_decode::Decoder;
use image::codecs::avif;
use image::codecs::gif;
use image::codecs::webp::WebPEncoder;
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
        let opaque = match &image {
            DynamicImage::ImageRgba8(img) => img.as_raw().par_chunks(4).all(|p| p[3] == 255),
            other => !other.color().has_alpha(),
        };
        ImageInfo { image, opaque }
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

/// Decode data from JXL format using jpegxl-rs (libjxl FFI).
#[cfg(feature = "jxl")]
pub fn jxl_decode(data: &[u8]) -> Result<DynamicImage> {
    let decoder = jpegxl_rs::decoder_builder()
        .build()
        .map_err(|_| ImageError::Unknown)?;
    let (info, pixels) = decoder
        .decode_with::<u8>(data)
        .map_err(|_| ImageError::Unknown)?;
    let w = info.width;
    let h = info.height;
    // Determine channels from pixel buffer length rather than metadata field name
    // (jpegxl-rs Metadata doesn't expose num_channels directly in 0.11).
    if pixels.len() == (w * h * 4) as usize {
        image::RgbaImage::from_raw(w, h, pixels)
            .ok_or(ImageError::Unknown)
            .map(DynamicImage::ImageRgba8)
    } else {
        image::RgbImage::from_raw(w, h, pixels)
            .ok_or(ImageError::Unknown)
            .map(DynamicImage::ImageRgb8)
    }
}

/// Stub used when the `jxl` feature is disabled: JXL inputs report a clear error.
#[cfg(not(feature = "jxl"))]
pub fn jxl_decode(_data: &[u8]) -> Result<DynamicImage> {
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
        let mut encoder = gif::GifEncoder::new_with_speed(&mut w, speed as i32);
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
    /// Optimize image to png, the quality is min 0, max 100, which means best effort,
    /// and never aborts the process.
    pub fn to_png(&self, quality: u8) -> Result<Vec<u8>> {
        let rgba = self.rgba_bytes();
        let pixels: &[RGBA8] = rgba.as_rgba();
        let width = self.width();
        let height = self.height();

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
        let (bytes, color) = if self.opaque {
            (self.rgb_bytes(), image::ColorType::Rgb8)
        } else {
            (self.rgba_bytes(), image::ColorType::Rgba8)
        };
        if quality >= 100 {
            let mut w = Vec::new();
            WebPEncoder::new_lossless(&mut w)
                .encode(bytes.as_ref(), width, height, color.into())
                .context(ImageSnafu {
                    category: "webp_encode",
                })?;
            Ok(w)
        } else {
            let encoder = if self.opaque {
                Encoder::from_rgb(bytes.as_ref(), width, height)
            } else {
                Encoder::from_rgba(bytes.as_ref(), width, height)
            };
            Ok(encoder.encode(quality as f32).to_vec())
        }
    }

    /// Optimize image to avif.
    /// `speed` accepts a value in the range 0-10, where 0 is the slowest and 10 is the fastest.
    /// `quality` accepts a value in the range 0-100, where 0 is the worst and 100 is the best.
    pub fn to_avif(&self, quality: u8, speed: u8) -> Result<Vec<u8>> {
        let mut w = Vec::new();
        let sp = if speed == 0 { 3 } else { speed };
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
    #[test]
    fn test_to_png() {
        let img = load_image();
        let result = img.to_png(90).unwrap();
        // 直接判断长度可能导致版本更新则需要重新修改测试
        assert_eq!(result.len(), 1665);
    }
    #[test]
    fn test_to_webp() {
        let img = load_image();
        // lossless
        let result = img.to_webp(100).unwrap();
        assert_eq!(result.len(), 2764);
        // lossy
        let result = img.to_webp(80).unwrap();
        assert_ne!(result.len(), 0);
        assert!(result.len() < 2764);
    }
    #[test]
    fn test_to_jpeg() {
        let img = load_image();
        let result = img.to_mozjpeg(90).unwrap();
        assert_eq!(result.len(), 392);
    }
    #[test]
    fn test_to_avif() {
        let img = load_image();
        let result = img.to_avif(90, 3).unwrap();
        assert_eq!(result.len(), 2402);
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
