use avif_decode::Decoder;
use image::codecs::avif;
use image::codecs::gif;
use image::codecs::webp::WebPEncoder;
use image::{AnimationDecoder, DynamicImage, ImageEncoder, ImageFormat, RgbaImage};
use lodepng::Bitmap;
use rgb::{ComponentBytes, FromSlice, RGB8, RGBA8};
use snafu::{ResultExt, Snafu};
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
    #[snafu(display("Handle image fail"))]
    Unknown,
}

type Result<T, E = ImageError> = std::result::Result<T, E>;

/// Holds a decoded RGBA image ready for encoding. Internally backed by `RgbaImage`
/// so all encoders can borrow its raw bytes without any intermediate copies.
pub struct ImageInfo {
    pub image: RgbaImage,
}

impl ImageInfo {
    pub fn width(&self) -> usize {
        self.image.width() as usize
    }
    pub fn height(&self) -> usize {
        self.image.height() as usize
    }
}

impl From<Bitmap<RGBA8>> for ImageInfo {
    fn from(info: Bitmap<RGBA8>) -> Self {
        let raw = info.buffer.as_bytes().to_vec();
        let image =
            RgbaImage::from_raw(info.width as u32, info.height as u32, raw).unwrap_or_default();
        ImageInfo { image }
    }
}

impl From<RgbaImage> for ImageInfo {
    fn from(image: RgbaImage) -> Self {
        ImageInfo { image }
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
            let buf: Vec<u8> = img.buf().iter().flat_map(|p| [p.r, p.g, p.b]).collect();
            image::RgbImage::from_raw(width, height, buf)
                .ok_or(ImageError::Unknown)
                .map(DynamicImage::ImageRgb8)
        }
        avif_decode::Image::Rgba8(img) => {
            let (width, height) = (img.width() as u32, img.height() as u32);
            let buf: Vec<u8> = img
                .buf()
                .iter()
                .flat_map(|p| [p.r, p.g, p.b, p.a])
                .collect();
            image::RgbaImage::from_raw(width, height, buf)
                .ok_or(ImageError::Unknown)
                .map(DynamicImage::ImageRgba8)
        }
        avif_decode::Image::Rgba16(img) => {
            let (width, height) = (img.width() as u32, img.height() as u32);
            let buf: Vec<u8> = img
                .buf()
                .iter()
                .flat_map(|p| {
                    [
                        (p.r / 257) as u8,
                        (p.g / 257) as u8,
                        (p.b / 257) as u8,
                        (p.a / 257) as u8,
                    ]
                })
                .collect();
            image::RgbaImage::from_raw(width, height, buf)
                .ok_or(ImageError::Unknown)
                .map(DynamicImage::ImageRgba8)
        }
        avif_decode::Image::Rgb16(img) => {
            let (width, height) = (img.width() as u32, img.height() as u32);
            let buf: Vec<u8> = img
                .buf()
                .iter()
                .flat_map(|p| [(p.r / 257) as u8, (p.g / 257) as u8, (p.b / 257) as u8])
                .collect();
            image::RgbImage::from_raw(width, height, buf)
                .ok_or(ImageError::Unknown)
                .map(DynamicImage::ImageRgb8)
        }
        _ => Err(ImageError::Unknown),
    }
}

pub fn load<R: BufRead + Seek>(r: R, ext: &str) -> Result<ImageInfo> {
    let format = ImageFormat::from_extension(OsStr::new(ext)).unwrap_or(ImageFormat::Jpeg);
    let result = image::load(r, format).context(ImageSnafu { category: "load" })?;
    Ok(result.to_rgba8().into())
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
    fn get_rgb8(&self) -> Vec<RGB8> {
        self.image
            .as_raw()
            .as_rgba()
            .iter()
            .map(|p| p.rgb())
            .collect()
    }

    /// Optimize image to png, the quality is min 0, max 100, which means best effort,
    /// and never aborts the process.
    pub fn to_png(&self, quality: u8) -> Result<Vec<u8>> {
        let pixels: &[RGBA8] = self.image.as_raw().as_rgba();
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
        if quality >= 100 {
            let mut w = Vec::new();
            WebPEncoder::new_lossless(&mut w)
                .encode(
                    self.image.as_raw(),
                    self.image.width(),
                    self.image.height(),
                    image::ColorType::Rgba8.into(),
                )
                .context(ImageSnafu {
                    category: "webp_encode",
                })?;
            Ok(w)
        } else {
            let di = DynamicImage::ImageRgba8(self.image.clone());
            let encoder = Encoder::from_image(&di).map_err(|_| ImageError::Unknown)?;
            Ok(encoder.encode(quality as f32).to_vec())
        }
    }

    /// Optimize image to avif.
    /// `speed` accepts a value in the range 0-10, where 0 is the slowest and 10 is the fastest.
    /// `quality` accepts a value in the range 0-100, where 0 is the worst and 100 is the best.
    pub fn to_avif(&self, quality: u8, speed: u8) -> Result<Vec<u8>> {
        let mut w = Vec::new();
        let sp = if speed == 0 { 3 } else { speed };

        let img = avif::AvifEncoder::new_with_speed_quality(&mut w, sp, quality);
        img.write_image(
            self.image.as_raw(),
            self.image.width(),
            self.image.height(),
            image::ColorType::Rgba8.into(),
        )
        .context(ImageSnafu {
            category: "avif_encode",
        })?;

        Ok(w)
    }

    /// Optimize image to jpeg, the quality 60-80 are recommended.
    pub fn to_mozjpeg(&self, quality: u8) -> Result<Vec<u8>> {
        let mut comp = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
        comp.set_size(self.width(), self.height());
        comp.set_quality(quality as f32);
        let mut comp = comp.start_compress(Vec::new()).context(IoSnafu {})?;
        comp.write_scanlines(self.get_rgb8().as_bytes())
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
}
