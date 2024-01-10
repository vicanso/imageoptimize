use avif_decode::Decoder;
use image::codecs::avif;
use image::codecs::gif;
use image::codecs::webp;
use image::{AnimationDecoder, DynamicImage, ImageEncoder, ImageFormat, RgbaImage};
use lodepng::Bitmap;
use rgb::{ComponentBytes, RGB8, RGBA8};
use snafu::{ResultExt, Snafu};
use std::{
    ffi::OsStr,
    io::{BufRead, Read, Seek},
};

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
    #[snafu(display("Handle image fail, category:mozjpeg, message:unknown"))]
    Mozjpeg {},
    #[snafu(display("Io fail, {source}"))]
    Io { source: std::io::Error },
    #[snafu(display("Handle image fail"))]
    Unknown,
}

type Result<T, E = ImageError> = std::result::Result<T, E>;

pub struct ImageInfo {
    // rgba像素
    pub buffer: Vec<RGBA8>,
    /// Width in pixels
    pub width: usize,
    /// Height in pixels
    pub height: usize,
}

impl From<Bitmap<RGBA8>> for ImageInfo {
    fn from(info: Bitmap<RGBA8>) -> Self {
        ImageInfo {
            buffer: info.buffer,
            width: info.width,
            height: info.height,
        }
    }
}

impl From<RgbaImage> for ImageInfo {
    fn from(img: RgbaImage) -> Self {
        let width = img.width() as usize;
        let height = img.height() as usize;
        let mut buffer = Vec::with_capacity(width * height);

        for ele in img.chunks(4) {
            buffer.push(RGBA8 {
                r: ele[0],
                g: ele[1],
                b: ele[2],
                a: ele[3],
            })
        }

        ImageInfo {
            buffer,
            width,
            height,
        }
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
            let width = img.width();
            let height = img.height();
            let mut buf = Vec::with_capacity(width * height * 3);
            for item in img.buf() {
                buf.push(item.r);
                buf.push(item.g);
                buf.push(item.b);
            }
            let rgb_image = image::RgbImage::from_raw(width as u32, height as u32, buf)
                .ok_or(ImageError::Unknown)?;
            Ok(DynamicImage::ImageRgb8(rgb_image))
        }
        avif_decode::Image::Rgba8(img) => {
            let width = img.width();
            let height = img.height();
            let mut buf = Vec::with_capacity(width * height * 4);
            for item in img.buf() {
                buf.push(item.r);
                buf.push(item.g);
                buf.push(item.b);
                buf.push(item.a);
            }
            let rgba_image = image::RgbaImage::from_raw(width as u32, height as u32, buf)
                .ok_or(ImageError::Unknown)?;
            Ok(DynamicImage::ImageRgba8(rgba_image))
        }
        avif_decode::Image::Rgba16(img) => {
            let width = img.width();
            let height = img.height();
            let mut buf = Vec::with_capacity(width * height * 4);
            for item in img.buf() {
                buf.push((item.r / 257) as u8);
                buf.push((item.g / 257) as u8);
                buf.push((item.b / 257) as u8);
                buf.push((item.a / 257) as u8);
            }
            let rgba_image = image::RgbaImage::from_raw(width as u32, height as u32, buf)
                .ok_or(ImageError::Unknown)?;
            Ok(DynamicImage::ImageRgba8(rgba_image))
        }
        avif_decode::Image::Rgb16(img) => {
            let width = img.width();
            let height = img.height();
            let mut buf = Vec::with_capacity(width * height * 3);
            for item in img.buf() {
                buf.push((item.r / 257) as u8);
                buf.push((item.g / 257) as u8);
                buf.push((item.b / 257) as u8);
            }
            let rgb_image = image::RgbImage::from_raw(width as u32, height as u32, buf)
                .ok_or(ImageError::Unknown)?;
            Ok(DynamicImage::ImageRgb8(rgb_image))
        }
        _ => Err(ImageError::Unknown),
    }
}

pub fn load<R: BufRead + Seek>(r: R, ext: &str) -> Result<ImageInfo> {
    let format = ImageFormat::from_extension(OsStr::new(ext)).unwrap_or(ImageFormat::Jpeg);
    let result = image::load(r, format).context(ImageSnafu { category: "load" })?;
    let img = result.to_rgba8();
    Ok(img.into())
}

pub fn to_gif<R: Read>(r: R, speed: u8) -> Result<Vec<u8>> {
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
        encoder
            .try_encode_frames(frames.into_iter())
            .context(ImageSnafu {
                category: "git_encode",
            })?;
    }

    Ok(w)
}

impl ImageInfo {
    // 转换获取rgb颜色
    fn get_rgb8(&self) -> Vec<RGB8> {
        let mut output_data: Vec<RGB8> = Vec::with_capacity(self.width * self.height);

        for ele in &self.buffer {
            output_data.push(ele.rgb())
        }

        output_data
    }
    /// Optimize image to png, the quality is min 0, max 100, which means best effort,
    /// and never aborts the process.
    pub fn to_png(&self, quality: u8) -> Result<Vec<u8>> {
        let mut liq = imagequant::new();
        liq.set_quality(0, quality).context(ImageQuantSnafu {
            category: "png_set_quality",
        })?;

        let mut img = liq
            .new_image(self.buffer.as_ref(), self.width, self.height, 0.0)
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

        let buf = enc
            .encode(&pixels, self.width, self.height)
            .context(LodePNGSnafu {
                category: "png_encode",
            })?;

        Ok(buf)
    }
    /// Optimize image to webp, the quality is min 0, max 100, the max means lossless.
    pub fn to_webp(&self, quality: u8) -> Result<Vec<u8>> {
        let mut w = Vec::new();

        let q = match quality {
            100 => webp::WebPQuality::lossless(),
            _ => webp::WebPQuality::lossy(quality),
        };
        let img = webp::WebPEncoder::new_with_quality(&mut w, q);

        img.encode(
            self.buffer.as_bytes(),
            self.width as u32,
            self.height as u32,
            image::ColorType::Rgba8,
        )
        .context(ImageSnafu {
            category: "webp_encode",
        })?;

        Ok(w)
    }
    /// Optimize image to avif.
    /// `speed` accepts a value in the range 0-10, where 0 is the slowest and 10 is the fastest.
    /// `quality` accepts a value in the range 0-100, where 0 is the worst and 100 is the best.
    pub fn to_avif(&self, quality: u8, speed: u8) -> Result<Vec<u8>> {
        let mut w = Vec::new();
        let mut sp = speed;
        if sp == 0 {
            sp = 3;
        }

        let img = avif::AvifEncoder::new_with_speed_quality(&mut w, sp, quality);
        img.write_image(
            self.buffer.as_bytes(),
            self.width as u32,
            self.height as u32,
            image::ColorType::Rgba8,
        )
        .context(ImageSnafu {
            category: "avif_encode",
        })?;

        Ok(w)
    }
    /// Optimize image to jpeg, the quality 60-80 are recommended.
    pub fn to_mozjpeg(&self, quality: u8) -> Result<Vec<u8>> {
        let mut comp = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
        comp.set_size(self.width, self.height);
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
        assert_eq!(img.height, 144);
        assert_eq!(img.width, 144);
    }
    #[test]
    fn test_to_png() {
        let img = load_image();
        let result = img.to_png(90).unwrap();
        // 直接判断长度可能导致版本更新则需要重新修改测试
        assert_eq!(result.len(), 1742);
    }
    #[test]
    fn test_to_webp() {
        let img = load_image();
        let result = img.to_webp(90).unwrap();
        assert_eq!(result.len(), 2092);
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
        assert_eq!(result.len(), 2337);
    }
}
