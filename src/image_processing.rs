use super::images::{avif_decode, to_gif, ImageError, ImageInfo};
use async_trait::async_trait;
use base64::{engine::general_purpose, Engine as _};
use dssim::Dssim;
use image::imageops::{crop, grayscale, overlay, resize, FilterType};
use image::{load, DynamicImage, ImageFormat, RgbaImage};
use rgb::FromSlice;
use snafu::{ensure, ResultExt, Snafu};
use std::ffi::OsStr;
use std::fs::File;
use std::io::Cursor;
use std::io::Read;
use std::time::Duration;
use substring::Substring;
use urlencoding::decode;

pub const PROCESS_LOAD: &str = "load";
pub const PROCESS_RESIZE: &str = "resize";
pub const PROCESS_OPTIM: &str = "optim";
pub const PROCESS_CROP: &str = "crop";
pub const PROCESS_GRAY: &str = "gray";
pub const PROCESS_WATERMARK: &str = "watermark";
pub const PROCESS_DIFF: &str = "diff";

const IMAGE_TYPE_GIF: &str = "gif";
const IMAGE_TYPE_PNG: &str = "png";
const IMAGE_TYPE_AVIF: &str = "avif";
const IMAGE_TYPE_WEBP: &str = "webp";
const IMAGE_TYPE_JPEG: &str = "jpeg";

#[derive(Debug, Snafu)]
pub enum ImageProcessingError {
    #[snafu(display("Process image fail, message:{message}"))]
    ParamsInvalid { message: String },
    #[snafu(display("{source}"))]
    Reqwest { source: reqwest::Error },
    #[snafu(display("{source}"))]
    HTTPHeaderToStr { source: reqwest::header::ToStrError },
    #[snafu(display("{source}"))]
    Base64Decode { source: base64::DecodeError },
    #[snafu(display("{source}"))]
    Image { source: image::ImageError },
    #[snafu(display("{source}"))]
    Images { source: ImageError },
    #[snafu(display("{source}"))]
    ParseInt { source: std::num::ParseIntError },
    #[snafu(display("{source}"))]
    FromUtf { source: std::string::FromUtf8Error },
    #[snafu(display("{source}"))]
    Io { source: std::io::Error },
}
type Result<T, E = ImageProcessingError> = std::result::Result<T, E>;

/// Run process image task.
/// Load task: ["load", "url"]
/// Resize task: ["resize", "width", "height"]
/// Gray task: ["gray"]
/// Optim task: ["optim", "webp", "quality", "speed"]
/// Crop task: ["crop", "x", "y", "width", "height"]
/// Watermark task: ["watermark", "url", "position", "margin left", "margin top"]
/// Diff task: ["diff"]
pub async fn run(tasks: Vec<Vec<String>>) -> Result<ProcessImage> {
    let mut img = ProcessImage {
        ..Default::default()
    };
    let he = ParamsInvalidSnafu {
        message: "params is invalid",
    };
    for params in tasks {
        if params.is_empty() {
            continue;
        }
        let sub_params = params[1..].to_vec();
        let task = &params[0];
        match task.as_str() {
            PROCESS_LOAD => {
                let data = &sub_params[0];
                let mut ext = "";
                if sub_params.len() >= 2 {
                    ext = &sub_params[1];
                }
                img = LoaderProcess::new(data, ext).process(img).await?;
                img.original = Some(img.di.to_rgba8())
            }
            PROCESS_RESIZE => {
                // 参数不符合
                ensure!(sub_params.len() >= 2, he);
                let width = sub_params[0].parse::<u32>().context(ParseIntSnafu {})?;
                let height = sub_params[1].parse::<u32>().context(ParseIntSnafu {})?;
                img = ResizeProcess::new(width, height).process(img).await?;
            }
            PROCESS_GRAY => {
                img = GrayProcess::new().process(img).await?;
            }
            PROCESS_OPTIM => {
                // 参数不符合
                ensure!(sub_params.len() == 3, he);
                let output_type = &sub_params[0];
                let mut quality = 80;
                if sub_params.len() > 1 {
                    quality = sub_params[1].parse::<u8>().context(ParseIntSnafu {})?;
                }

                let mut speed = 3;
                if sub_params.len() > 2 {
                    speed = sub_params[2].parse::<u8>().context(ParseIntSnafu {})?;
                }

                img = OptimProcess::new(output_type, quality, speed)
                    .process(img)
                    .await?;
            }
            PROCESS_CROP => {
                // 参数不符合
                ensure!(sub_params.len() >= 4, he);
                let x = sub_params[0].parse::<u32>().context(ParseIntSnafu {})?;
                let y = sub_params[1].parse::<u32>().context(ParseIntSnafu {})?;
                let width = sub_params[2].parse::<u32>().context(ParseIntSnafu {})?;
                let height = sub_params[3].parse::<u32>().context(ParseIntSnafu {})?;
                img = CropProcess::new(x, y, width, height).process(img).await?;
            }
            PROCESS_WATERMARK => {
                // 参数不符合
                ensure!(!sub_params.is_empty(), he);
                let url = decode(sub_params[0].as_str())
                    .context(FromUtfSnafu {})?
                    .to_string();
                let mut position = WatermarkPosition::RightBottom;
                if sub_params.len() > 1 {
                    position = (sub_params[1].as_str()).into();
                }
                let mut margin_left = 0;
                if sub_params.len() > 2 {
                    margin_left = sub_params[2].parse::<i64>().context(ParseIntSnafu {})?;
                }
                let mut margin_top = 0;
                if sub_params.len() > 3 {
                    margin_top = sub_params[3].parse::<i64>().context(ParseIntSnafu {})?;
                }
                let watermark = LoaderProcess::new(&url, "")
                    .process(ProcessImage {
                        ..Default::default()
                    })
                    .await?;

                let pro = WatermarkProcess::new(watermark.di, position, margin_left, margin_top);
                img = pro.process(img).await?;
            }
            PROCESS_DIFF => {
                img.diff = img.get_diff();
            }
            _ => {}
        }
    }
    Ok(img)
}

#[derive(Default, Clone)]
pub struct ProcessImage {
    original: Option<RgbaImage>,
    di: DynamicImage,
    pub diff: f64,
    pub original_size: usize,
    buffer: Vec<u8>,
    pub ext: String,
}

impl ProcessImage {
    pub fn new(data: Vec<u8>, ext: &str) -> Result<Self> {
        let format = ImageFormat::from_extension(OsStr::new(ext));
        ensure!(
            format.is_some(),
            ParamsInvalidSnafu {
                message: "Image format is not support".to_string(),
            }
        );
        // 已保证format不为空
        let di = load(Cursor::new(&data), format.unwrap()).context(ImageSnafu {})?;
        Ok(ProcessImage {
            original_size: data.len(),
            di,
            buffer: data,
            ext: ext.to_string(),
            ..Default::default()
        })
    }
    pub fn get_buffer(&self) -> Result<Vec<u8>> {
        if self.buffer.is_empty() {
            let mut bytes: Vec<u8> = Vec::new();
            let format =
                ImageFormat::from_extension(self.ext.as_str()).unwrap_or(ImageFormat::Jpeg);
            self.di
                .write_to(&mut Cursor::new(&mut bytes), format)
                .context(ImageSnafu {})?;
            Ok(bytes)
        } else {
            Ok(self.buffer.clone())
        }
    }
    fn support_dssim(&self) -> bool {
        self.ext != IMAGE_TYPE_GIF
    }
    fn get_diff(&self) -> f64 {
        // 如果无数据
        if self.original.is_none() {
            return -1.0;
        }
        // 如果是gif或者禁用了dssim
        if !self.support_dssim() {
            return -1.0;
        }
        // 已确保一定有数据
        let original = self.original.as_ref().unwrap();
        // 如果宽高不一致，则不比对
        if original.width() != self.di.width() || original.height() != self.di.height() {
            return -1.0;
        }
        let width = original.width() as usize;
        let height = original.height() as usize;
        let attr = Dssim::new();
        let gp1 = attr
            .create_image_rgba(original.as_raw().as_rgba(), width, height)
            .unwrap();
        let gp2 = attr
            .create_image_rgba(self.di.to_rgba8().as_raw().as_rgba(), width, height)
            .unwrap();
        let (diff, _) = attr.compare(&gp1, gp2);
        let value: f64 = diff.into();
        // 放大1千倍
        value * 1000.0
    }
}

#[async_trait]

pub trait Process {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage>;
}

/// Loader process loads the image data from http, file or base64.
pub struct LoaderProcess {
    data: String,
    ext: String,
}

impl LoaderProcess {
    pub fn new(data: &str, ext: &str) -> Self {
        LoaderProcess {
            data: data.to_string(),
            ext: ext.to_string(),
        }
    }
    async fn fetch_data(&self) -> Result<ProcessImage> {
        let data = &self.data;
        let mut ext = self.ext.clone();
        let from_http = data.starts_with("http");
        let file_prefix = "file://";
        let from_file = data.starts_with(file_prefix);
        let original_data = if from_http {
            let resp = reqwest::Client::builder()
                .build()
                .context(ReqwestSnafu {})?
                .get(data)
                .timeout(Duration::from_secs(5 * 60))
                .send()
                .await
                .context(ReqwestSnafu {})?;

            if let Some(content_type) = resp.headers().get("Content-Type") {
                let str = content_type.to_str().context(HTTPHeaderToStrSnafu {})?;
                let arr: Vec<_> = str.split('/').collect();
                if arr.len() == 2 {
                    ext = arr[1].to_string();
                }
            }
            resp.bytes().await.context(ReqwestSnafu {})?.into()
        } else if from_file {
            let mut file =
                File::open(data.substring(file_prefix.len(), data.len())).context(IoSnafu)?;
            ext = data.split('.').last().unwrap_or_default().to_string();

            let mut contents = vec![];
            file.read_to_end(&mut contents).context(IoSnafu)?;
            contents
        } else {
            general_purpose::STANDARD
                .decode(data.as_bytes())
                .context(Base64DecodeSnafu {})?
        };
        ProcessImage::new(original_data, &ext)
    }
}

// 图片加载
#[async_trait]
impl Process for LoaderProcess {
    async fn process(&self, _: ProcessImage) -> Result<ProcessImage> {
        let result = self.fetch_data().await?;
        Ok(result)
    }
}

/// Resize process resizes the image size.
pub struct ResizeProcess {
    width: u32,
    height: u32,
}

impl ResizeProcess {
    pub fn new(width: u32, height: u32) -> Self {
        ResizeProcess { width, height }
    }
}

#[async_trait]
impl Process for ResizeProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        let mut w = self.width;
        let mut h = self.height;
        if w == 0 && h == 0 {
            return Ok(img);
        }
        let width = img.di.width();
        let height = img.di.height();
        // 如果宽或者高为0，则计算对应的宽高
        if w == 0 {
            w = width * h / height;
        }
        if h == 0 {
            h = height * w / width;
        }
        let result = resize(&img.di, w, h, FilterType::Lanczos3);
        img.buffer = vec![];
        img.di = DynamicImage::ImageRgba8(result);
        Ok(img)
    }
}

/// Gray process changes the image to gray mode.
#[derive(Default)]
pub struct GrayProcess {}

impl GrayProcess {
    pub fn new() -> Self {
        GrayProcess {}
    }
}

#[async_trait]
impl Process for GrayProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        img.di = DynamicImage::ImageLuma8(grayscale(&img.di));
        img.buffer = vec![];
        Ok(img)
    }
}

pub enum WatermarkPosition {
    LeftTop,
    Top,
    RightTop,
    Left,
    Center,
    Right,
    LeftBottom,
    Bottom,
    RightBottom,
}

impl From<&str> for WatermarkPosition {
    fn from(value: &str) -> Self {
        match value {
            "leftTop" => WatermarkPosition::LeftTop,
            "top" => WatermarkPosition::Top,
            "rightTop" => WatermarkPosition::RightTop,
            "left" => WatermarkPosition::Left,
            "center" => WatermarkPosition::Center,
            "right" => WatermarkPosition::Right,
            "leftBottom" => WatermarkPosition::LeftBottom,
            "bottom" => WatermarkPosition::Bottom,
            _ => WatermarkPosition::RightBottom,
        }
    }
}

/// Watermark process adds a watermark over the image.
pub struct WatermarkProcess {
    watermark: DynamicImage,
    position: WatermarkPosition,
    margin_left: i64,
    margin_top: i64,
}

impl WatermarkProcess {
    pub fn new(
        watermark: DynamicImage,
        position: WatermarkPosition,
        margin_left: i64,
        margin_top: i64,
    ) -> Self {
        WatermarkProcess {
            watermark,
            position,
            margin_left,
            margin_top,
        }
    }
}

#[async_trait]
impl Process for WatermarkProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        let di = img.di;
        let w = di.width() as i64;
        let h = di.height() as i64;
        let ww = self.watermark.width() as i64;
        let wh = self.watermark.height() as i64;
        let mut x: i64 = 0;
        let mut y: i64 = 0;
        match self.position {
            WatermarkPosition::Top => {
                x = (w - ww) >> 1;
            }
            WatermarkPosition::RightTop => {
                x = w - ww;
            }
            WatermarkPosition::Left => {
                y = (h - wh) >> 1;
            }
            WatermarkPosition::Center => {
                x = (w - ww) >> 1;
                y = (h - wh) >> 1;
            }
            WatermarkPosition::Right => {
                x = w - ww;
                y = (h - wh) >> 1;
            }
            WatermarkPosition::LeftBottom => {
                y = h - wh;
            }
            WatermarkPosition::Bottom => {
                x = (w - ww) >> 1;
                y = h - wh;
            }
            WatermarkPosition::RightBottom => {
                x = w - ww;
                y = h - wh;
            }
            _ => (),
        }
        x += self.margin_left;
        y += self.margin_top;
        let mut bottom: DynamicImage = di;
        overlay(&mut bottom, &self.watermark, x, y);
        img.buffer = vec![];
        img.di = bottom;
        Ok(img)
    }
}

/// Crop process crops the image.
pub struct CropProcess {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl CropProcess {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

#[async_trait]
impl Process for CropProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        let mut r = img.di;
        let result = crop(&mut r, self.x, self.y, self.width, self.height);
        img.di = DynamicImage::ImageRgba8(result.to_image());
        img.buffer = vec![];
        Ok(img)
    }
}

/// Optim process optimizes the image of multi format.
pub struct OptimProcess {
    output_type: String,
    quality: u8,
    speed: u8,
}

impl OptimProcess {
    pub fn new(output_type: &str, quality: u8, speed: u8) -> Self {
        Self {
            output_type: output_type.to_string(),
            quality,
            speed,
        }
    }
}

#[async_trait]
impl Process for OptimProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;

        let info: ImageInfo = img.di.to_rgba8().into();
        let quality = self.quality;
        let speed = self.speed;
        let original_type = img.ext.clone();

        let original_size = img.buffer.len();
        let mut output_type = self.output_type.clone();
        // 如果未指定输出，则保持原有
        if output_type.is_empty() {
            output_type = original_type.clone();
        }

        img.ext = output_type.clone();

        let data = match output_type.as_str() {
            IMAGE_TYPE_GIF => {
                let c = Cursor::new(&img.buffer);
                to_gif(c, 10).context(ImagesSnafu {})?
            }
            _ => {
                match output_type.as_str() {
                    IMAGE_TYPE_PNG => info.to_png(quality).context(ImagesSnafu {})?,
                    IMAGE_TYPE_AVIF => info.to_avif(quality, speed).context(ImagesSnafu {})?,
                    IMAGE_TYPE_WEBP => info.to_webp(quality).context(ImagesSnafu {})?,
                    // 其它的全部使用jpeg
                    _ => {
                        img.ext = IMAGE_TYPE_JPEG.to_string();
                        info.to_mozjpeg(quality).context(ImagesSnafu {})?
                    }
                }
            }
        };
        // 类型不一样
        // 或者类型一样但是数据最小
        // 或者无原始数据
        if img.ext != original_type || data.len() < original_size || original_size == 0 {
            img.buffer = data;
            // 支持dssim再根据数据生成image
            // 否则无此必要
            if img.support_dssim() {
                // image 的avif decoder有其它依赖
                // 暂使用其它模块
                // decode如果失败则忽略
                // 因为只用于计算dssim
                let result = if img.ext == IMAGE_TYPE_AVIF {
                    avif_decode(&img.buffer).context(ImagesSnafu {})
                } else {
                    let c = Cursor::new(&img.buffer);
                    let format = ImageFormat::from_extension(OsStr::new(img.ext.as_str()));
                    load(c, format.unwrap()).context(ImageSnafu {})
                };
                if let Ok(value) = result {
                    img.di = value;
                }
            }
        }

        Ok(img)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CropProcess, GrayProcess, LoaderProcess, OptimProcess, ResizeProcess, WatermarkProcess,
    };
    use crate::image_processing::{Process, ProcessImage};
    use base64::{engine::general_purpose, Engine as _};
    use pretty_assertions::assert_eq;
    fn new_process_image() -> ProcessImage {
        let data = include_bytes!("../assets/rust-logo.png");
        ProcessImage::new(data.to_vec(), "png").unwrap()
    }

    #[test]
    fn test_load_process() {
        let p = LoaderProcess::new(
            "https://www.baidu.com/img/PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png",
            "",
        );
        let result = tokio_test::block_on(p.fetch_data()).unwrap();
        assert_ne!(result.buffer.len(), 0);
        assert_eq!(result.ext, "png");

        let file = format!(
            "file://{}/assets/rust-logo.png",
            std::env::current_dir().unwrap().to_string_lossy()
        );
        let p = LoaderProcess::new(&file, "");
        let result = tokio_test::block_on(p.fetch_data()).unwrap();
        assert_ne!(result.buffer.len(), 0);
        assert_eq!(result.ext, "png");

        let data = include_bytes!("../assets/rust-logo.png");
        let p = LoaderProcess::new(&general_purpose::STANDARD.encode(data), "png");
        let result = tokio_test::block_on(p.process(ProcessImage::default())).unwrap();
        assert_ne!(result.buffer.len(), 0);
        assert_eq!(result.ext, "png");
    }

    #[test]
    fn test_resize_process() {
        let p = new_process_image();
        let result = tokio_test::block_on(ResizeProcess::new(48, 0).process(p)).unwrap();
        assert_eq!(result.di.width(), 48);
        assert_eq!(result.di.height(), 48);
    }

    #[test]
    fn test_gray_process() {
        let p = new_process_image();
        let result = tokio_test::block_on(GrayProcess::new().process(p)).unwrap();
        assert_eq!(result.di.width(), 144);
        assert_eq!(result.di.height(), 144);
    }

    #[test]
    fn test_watermark_process() {
        let watermark =
            tokio_test::block_on(ResizeProcess::new(48, 0).process(new_process_image())).unwrap();
        let p = new_process_image();
        let result = tokio_test::block_on(
            WatermarkProcess::new(watermark.di, "rightBottom".into(), 0, 0).process(p),
        )
        .unwrap();
        assert_eq!(result.di.width(), 144);
        assert_eq!(result.di.height(), 144);
    }

    #[test]
    fn test_crop_process() {
        let p = new_process_image();
        let result = tokio_test::block_on(CropProcess::new(40, 40, 48, 48).process(p)).unwrap();
        assert_eq!(result.di.width(), 48);
        assert_eq!(result.di.height(), 48);
    }

    #[test]
    fn test_optim_process() {
        // to png
        let result =
            tokio_test::block_on(OptimProcess::new("png", 70, 0).process(new_process_image()))
                .unwrap();
        assert_eq!(result.ext, "png");
        assert_ne!(result.get_diff(), 0.0_f64);
        assert_eq!(result.buffer.len(), 1483);

        let result =
            tokio_test::block_on(OptimProcess::new("avif", 70, 0).process(new_process_image()))
                .unwrap();
        assert_eq!(result.ext, "avif");
        assert_eq!(result.buffer.len(), 2367);

        let result =
            tokio_test::block_on(OptimProcess::new("webp", 70, 0).process(new_process_image()))
                .unwrap();
        assert_eq!(result.ext, "webp");
        assert_eq!(result.buffer.len(), 2094);

        let result =
            tokio_test::block_on(OptimProcess::new("jpeg", 70, 0).process(new_process_image()))
                .unwrap();
        assert_eq!(result.ext, "jpeg");
        assert_eq!(result.buffer.len(), 392);
    }
}
