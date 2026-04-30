use super::images::{avif_decode, to_gif, ImageError, ImageInfo};
use base64::{engine::general_purpose, Engine as _};
use bytes::Bytes;
use dssim_core::Dssim;
use exif::{In, Reader, Tag};
use image::imageops::{
    blur, brighten, crop, flip_horizontal, flip_vertical, grayscale, overlay, resize, rotate180,
    rotate270, rotate90, unsharpen, FilterType,
};
use image::{load, DynamicImage, ImageFormat, RgbaImage};
use img_parts::ImageEXIF;
use rgb::FromSlice;
use snafu::{ensure, ResultExt, Snafu};
use std::borrow::Cow;
use std::io::Cursor;
use std::sync::OnceLock;
use std::time::Duration;
use urlencoding::decode;

pub const PROCESS_LOAD: &str = "load";
pub const PROCESS_RESIZE: &str = "resize";
pub const PROCESS_OPTIM: &str = "optim";
pub const PROCESS_CROP: &str = "crop";
pub const PROCESS_GRAY: &str = "gray";
pub const PROCESS_WATERMARK: &str = "watermark";
pub const PROCESS_DIFF: &str = "diff";
pub const PROCESS_FLIP: &str = "flip";
pub const PROCESS_ROTATE: &str = "rotate";
pub const PROCESS_BRIGHTEN: &str = "brighten";
pub const PROCESS_CONTRAST: &str = "contrast";
pub const PROCESS_SHARPEN: &str = "sharpen";
pub const PROCESS_PADDING: &str = "padding";
pub const PROCESS_BLUR: &str = "blur";
pub const PROCESS_STRIP: &str = "strip";

const IMAGE_TYPE_GIF: &str = "gif";
const IMAGE_TYPE_PNG: &str = "png";
const IMAGE_TYPE_AVIF: &str = "avif";
const IMAGE_TYPE_WEBP: &str = "webp";
const IMAGE_TYPE_JPEG: &str = "jpeg";

static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn get_http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(reqwest::Client::new)
}

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
pub fn new_load_task(url: &str) -> Vec<String> {
    vec![PROCESS_LOAD.to_string(), url.to_string()]
}

pub fn new_resize_task(width: u32, height: u32) -> Vec<String> {
    vec![
        PROCESS_RESIZE.to_string(),
        width.to_string(),
        height.to_string(),
    ]
}

pub fn new_gray_task() -> Vec<String> {
    vec![PROCESS_GRAY.to_string()]
}

pub fn new_optim_task(output_type: &str, quality: u8, speed: u8) -> Vec<String> {
    vec![
        PROCESS_OPTIM.to_string(),
        output_type.to_string(),
        quality.to_string(),
        speed.to_string(),
    ]
}

pub fn new_crop_task(x: u32, y: u32, width: u32, height: u32) -> Vec<String> {
    vec![
        PROCESS_CROP.to_string(),
        x.to_string(),
        y.to_string(),
        width.to_string(),
        height.to_string(),
    ]
}

pub fn new_watermark_task(
    url: &str,
    position: &str,
    margin_left: i32,
    margin_top: i32,
) -> Vec<String> {
    vec![
        PROCESS_WATERMARK.to_string(),
        url.to_string(),
        position.to_string(),
        margin_left.to_string(),
        margin_top.to_string(),
    ]
}

pub fn new_diff_task() -> Vec<String> {
    vec![PROCESS_DIFF.to_string()]
}

pub fn new_flip_task(direction: &str) -> Vec<String> {
    vec![PROCESS_FLIP.to_string(), direction.to_string()]
}

pub fn new_rotate_task(degrees: u16) -> Vec<String> {
    vec![PROCESS_ROTATE.to_string(), degrees.to_string()]
}

/// `value` is added to each channel: positive brightens, negative darkens (-255..=255).
pub fn new_brighten_task(value: i32) -> Vec<String> {
    vec![PROCESS_BRIGHTEN.to_string(), value.to_string()]
}

/// `contrast` > 0 increases contrast, < 0 decreases it.
pub fn new_contrast_task(contrast: f32) -> Vec<String> {
    vec![PROCESS_CONTRAST.to_string(), contrast.to_string()]
}

/// USM sharpening. `sigma` controls blur radius (e.g. 1.0), `threshold` is the
/// minimum brightness difference to apply sharpening (e.g. 0).
pub fn new_sharpen_task(sigma: f32, threshold: i32) -> Vec<String> {
    vec![
        PROCESS_SHARPEN.to_string(),
        sigma.to_string(),
        threshold.to_string(),
    ]
}

/// Gaussian blur. `sigma` controls the blur radius (e.g. `2.0`).
pub fn new_blur_task(sigma: f32) -> Vec<String> {
    vec![PROCESS_BLUR.to_string(), sigma.to_string()]
}

/// Strip EXIF metadata (including GPS) from the encoded buffer without re-encoding.
/// Supports JPEG, PNG, and WebP. Other formats are returned unchanged.
pub fn new_strip_task() -> Vec<String> {
    vec![PROCESS_STRIP.to_string()]
}

/// Strip EXIF metadata from raw image bytes without re-encoding.
/// `ext` is the format extension (`"jpeg"`, `"jpg"`, `"png"`, `"webp"`).
/// Formats that are not supported are returned unchanged.
pub fn strip_exif_bytes(data: Vec<u8>, ext: &str) -> Vec<u8> {
    let b = Bytes::from(data);
    let stripped: Option<Bytes> = match ext {
        "jpeg" | "jpg" => img_parts::jpeg::Jpeg::from_bytes(b.clone())
            .ok()
            .and_then(|mut img| {
                img.exif()?;
                img.set_exif(None);
                Some(img.encoder().bytes())
            }),
        "png" => img_parts::png::Png::from_bytes(b.clone())
            .ok()
            .and_then(|mut img| {
                img.exif()?;
                img.set_exif(None);
                Some(img.encoder().bytes())
            }),
        "webp" => img_parts::webp::WebP::from_bytes(b.clone())
            .ok()
            .and_then(|mut img| {
                img.exif()?;
                img.set_exif(None);
                Some(img.encoder().bytes())
            }),
        _ => None,
    };
    stripped.unwrap_or(b).to_vec()
}

/// Resize to fit within `max_width × max_height`, preserving aspect ratio.
/// No-op when the image already fits. Pass 0 to leave a dimension unconstrained.
pub fn new_fit_task(max_width: u32, max_height: u32) -> Vec<String> {
    vec![
        PROCESS_RESIZE.to_string(),
        max_width.to_string(),
        max_height.to_string(),
        "fit".to_string(),
    ]
}

/// Extend the canvas to `width × height`, centering the original. `color` is an
/// optional hex string (`#rrggbb` or `#rrggbbaa`); defaults to transparent.
pub fn new_padding_task(width: u32, height: u32, color: &str) -> Vec<String> {
    vec![
        PROCESS_PADDING.to_string(),
        width.to_string(),
        height.to_string(),
        color.to_string(),
    ]
}

pub async fn run_with_image(
    mut image: ProcessImage,
    tasks: Vec<Vec<String>>,
) -> Result<ProcessImage> {
    let he = ParamsInvalidSnafu {
        message: "params is invalid",
    };
    for params in tasks {
        if params.is_empty() {
            continue;
        }
        let sub_params = &params[1..];
        let task = &params[0];
        match task.as_str() {
            PROCESS_LOAD => {
                let data = &sub_params[0];
                let mut ext = "";
                if sub_params.len() >= 2 {
                    ext = &sub_params[1];
                }
                image = LoaderProcess::new(data, ext).process(image).await?;
            }
            PROCESS_RESIZE => {
                ensure!(sub_params.len() >= 2, he);
                let width = sub_params[0].parse::<u32>().context(ParseIntSnafu {})?;
                let height = sub_params[1].parse::<u32>().context(ParseIntSnafu {})?;
                let fit = sub_params.get(2).map(|s| s == "fit").unwrap_or(false);
                let proc = if fit {
                    ResizeProcess::new_fit(width, height)
                } else {
                    ResizeProcess::new(width, height)
                };
                image = proc.process(image).await?;
            }
            PROCESS_GRAY => {
                image = GrayProcess::new().process(image).await?;
            }
            PROCESS_FLIP => {
                let direction = sub_params.first().map(|s| s.as_str()).unwrap_or("h");
                image = FlipProcess::new(direction).process(image).await?;
            }
            PROCESS_ROTATE => {
                let degrees = sub_params
                    .first()
                    .and_then(|s| s.parse::<u16>().ok())
                    .unwrap_or(90);
                image = RotateProcess::new(degrees).process(image).await?;
            }
            PROCESS_BRIGHTEN => {
                let value = sub_params
                    .first()
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(0);
                image = BrightenProcess::new(value).process(image).await?;
            }
            PROCESS_CONTRAST => {
                let value = sub_params
                    .first()
                    .and_then(|s| s.parse::<f32>().ok())
                    .unwrap_or(0.0);
                image = ContrastProcess::new(value).process(image).await?;
            }
            PROCESS_SHARPEN => {
                let sigma = sub_params
                    .first()
                    .and_then(|s| s.parse::<f32>().ok())
                    .unwrap_or(1.0);
                let threshold = sub_params
                    .get(1)
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(0);
                image = SharpenProcess::new(sigma, threshold).process(image).await?;
            }
            PROCESS_BLUR => {
                let sigma = sub_params
                    .first()
                    .and_then(|s| s.parse::<f32>().ok())
                    .unwrap_or(1.0);
                image = BlurProcess::new(sigma).process(image).await?;
            }
            PROCESS_STRIP => {
                image = StripProcess::new().process(image).await?;
            }
            PROCESS_PADDING => {
                ensure!(sub_params.len() >= 2, he);
                let width = sub_params[0].parse::<u32>().context(ParseIntSnafu {})?;
                let height = sub_params[1].parse::<u32>().context(ParseIntSnafu {})?;
                let color = sub_params.get(2).map(|s| s.as_str()).unwrap_or("");
                image = PaddingProcess::new(width, height, color)
                    .process(image)
                    .await?;
            }
            PROCESS_OPTIM => {
                // 参数不符合
                ensure!(sub_params.len() >= 3, he);
                let output_type = &sub_params[0];
                let quality = sub_params[1].parse::<u8>().context(ParseIntSnafu {})?;
                let speed = sub_params[2].parse::<u8>().context(ParseIntSnafu {})?;

                image = OptimProcess::new(output_type, quality, speed)
                    .process(image)
                    .await?;
            }
            PROCESS_CROP => {
                // 参数不符合
                ensure!(sub_params.len() >= 4, he);
                let x = sub_params[0].parse::<u32>().context(ParseIntSnafu {})?;
                let y = sub_params[1].parse::<u32>().context(ParseIntSnafu {})?;
                let width = sub_params[2].parse::<u32>().context(ParseIntSnafu {})?;
                let height = sub_params[3].parse::<u32>().context(ParseIntSnafu {})?;
                image = CropProcess::new(x, y, width, height).process(image).await?;
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
                    .process(ProcessImage::default())
                    .await?;

                let pro = WatermarkProcess::new(watermark.di, position, margin_left, margin_top);
                image = pro.process(image).await?;
            }
            PROCESS_DIFF => {
                image.diff = image.get_diff();
            }
            _ => {}
        }
    }
    Ok(image)
}

pub async fn run(tasks: Vec<Vec<String>>) -> Result<ProcessImage> {
    run_with_image(ProcessImage::default(), tasks).await
}

fn get_exif_orientation(data: &[u8]) -> u32 {
    Reader::new()
        .read_from_container(&mut Cursor::new(data))
        .ok()
        .and_then(|exif| exif.get_field(Tag::Orientation, In::PRIMARY).cloned())
        .and_then(|field| field.value.get_uint(0))
        .unwrap_or(1)
}

fn apply_orientation(di: DynamicImage, orientation: u32) -> DynamicImage {
    match orientation {
        2 => DynamicImage::ImageRgba8(flip_horizontal(&di)),
        3 => DynamicImage::ImageRgba8(rotate180(&di)),
        4 => DynamicImage::ImageRgba8(flip_vertical(&di)),
        5 => {
            let tmp = DynamicImage::ImageRgba8(flip_horizontal(&di));
            DynamicImage::ImageRgba8(rotate270(&tmp))
        }
        6 => DynamicImage::ImageRgba8(rotate90(&di)),
        7 => {
            let tmp = DynamicImage::ImageRgba8(flip_horizontal(&di));
            DynamicImage::ImageRgba8(rotate90(&tmp))
        }
        8 => DynamicImage::ImageRgba8(rotate270(&di)),
        _ => di,
    }
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
        let format = image::guess_format(&data).or_else(|_| {
            ImageFormat::from_extension(ext).ok_or(ImageProcessingError::ParamsInvalid {
                message: "Image format is not supported".to_string(),
            })
        })?;
        let di = load(Cursor::new(&data), format).context(ImageSnafu {})?;
        let orientation = get_exif_orientation(&data);
        let di = apply_orientation(di, orientation);
        let original_size = data.len();
        // Clear buffer when orientation was corrected so get_buffer() re-encodes
        // from the oriented di rather than returning bytes with the stale EXIF tag.
        let buffer = if orientation == 1 { data } else { vec![] };
        Ok(ProcessImage {
            original_size,
            original: Some(di.to_rgba8()),
            di,
            buffer,
            diff: -1.0,
            ext: ext.to_string(),
        })
    }
    pub fn get_buffer(&self) -> Result<Cow<'_, [u8]>> {
        if self.buffer.is_empty() {
            let mut bytes: Vec<u8> = Vec::new();
            let format = ImageFormat::from_extension(&self.ext).unwrap_or(ImageFormat::Jpeg);
            self.di
                .write_to(&mut Cursor::new(&mut bytes), format)
                .context(ImageSnafu {})?;
            Ok(Cow::Owned(bytes))
        } else {
            Ok(Cow::Borrowed(&self.buffer))
        }
    }
    pub fn get_size(&self) -> (u32, u32) {
        (self.di.width(), self.di.height())
    }
    fn support_dssim(&self) -> bool {
        self.ext != IMAGE_TYPE_GIF
    }
    fn get_diff(&self) -> f64 {
        let Some(original) = &self.original else {
            return -1.0;
        };
        if !self.support_dssim() {
            return -1.0;
        }
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
        let tmp;
        let current_rgba = match &self.di {
            DynamicImage::ImageRgba8(img) => img,
            other => {
                tmp = other.to_rgba8();
                &tmp
            }
        };
        let gp2 = attr
            .create_image_rgba(current_rgba.as_raw().as_rgba(), width, height)
            .unwrap();
        let (diff, _) = attr.compare(&gp1, gp2);
        let value: f64 = diff.into();
        // 放大1千倍
        value * 1000.0
    }
}

#[allow(async_fn_in_trait)]
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
            let resp = get_http_client()
                .get(data)
                .timeout(Duration::from_secs(5 * 60))
                .send()
                .await
                .context(ReqwestSnafu {})?;

            if let Some(content_type) = resp.headers().get("Content-Type") {
                let str = content_type.to_str().context(HTTPHeaderToStrSnafu {})?;
                if let Some((_, t)) = str.split_once('/') {
                    ext = t.to_string();
                }
            }
            resp.bytes().await.context(ReqwestSnafu {})?.into()
        } else if from_file {
            ext = data.split('.').next_back().unwrap_or_default().to_string();
            std::fs::read(&data[file_prefix.len()..]).context(IoSnafu)?
        } else {
            general_purpose::STANDARD
                .decode(data.as_bytes())
                .context(Base64DecodeSnafu {})?
        };
        ProcessImage::new(original_data, &ext)
    }
}

// 图片加载
impl Process for LoaderProcess {
    async fn process(&self, _: ProcessImage) -> Result<ProcessImage> {
        let result = self.fetch_data().await?;
        Ok(result)
    }
}

/// Resize process resizes the image.
/// In exact mode (fit=false) it scales to the given width×height (0 = proportional).
/// In fit mode (fit=true) it scales down to fit within the bounds while preserving
/// aspect ratio; images already within the bounds are left untouched.
pub struct ResizeProcess {
    width: u32,
    height: u32,
    fit: bool,
}

impl ResizeProcess {
    pub fn new(width: u32, height: u32) -> Self {
        ResizeProcess {
            width,
            height,
            fit: false,
        }
    }
    pub fn new_fit(max_width: u32, max_height: u32) -> Self {
        ResizeProcess {
            width: max_width,
            height: max_height,
            fit: true,
        }
    }
}

impl Process for ResizeProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        if self.width == 0 && self.height == 0 {
            return Ok(img);
        }
        let src_w = img.di.width();
        let src_h = img.di.height();

        let (new_w, new_h) = if self.fit {
            let fits_w = self.width == 0 || src_w <= self.width;
            let fits_h = self.height == 0 || src_h <= self.height;
            if fits_w && fits_h {
                return Ok(img);
            }
            let scale_w = if self.width > 0 && src_w > self.width {
                self.width as f64 / src_w as f64
            } else {
                1.0
            };
            let scale_h = if self.height > 0 && src_h > self.height {
                self.height as f64 / src_h as f64
            } else {
                1.0
            };
            let scale = scale_w.min(scale_h);
            (
                (src_w as f64 * scale).round() as u32,
                (src_h as f64 * scale).round() as u32,
            )
        } else {
            let mut w = self.width;
            let mut h = self.height;
            if w == 0 {
                w = src_w * h / src_h;
            }
            if h == 0 {
                h = src_h * w / src_w;
            }
            (w, h)
        };

        let result = resize(&img.di, new_w, new_h, FilterType::Lanczos3);
        img.buffer.clear();
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

impl Process for GrayProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        img.di = DynamicImage::ImageLuma8(grayscale(&img.di));
        img.buffer.clear();
        Ok(img)
    }
}

pub struct FlipProcess {
    horizontal: bool,
}

impl FlipProcess {
    pub fn new(direction: &str) -> Self {
        FlipProcess {
            horizontal: direction != "v" && direction != "vertical",
        }
    }
}

impl Process for FlipProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        let flipped = if self.horizontal {
            flip_horizontal(&img.di)
        } else {
            flip_vertical(&img.di)
        };
        img.di = DynamicImage::ImageRgba8(flipped);
        img.buffer.clear();
        Ok(img)
    }
}

pub struct RotateProcess {
    degrees: u16,
}

impl RotateProcess {
    pub fn new(degrees: u16) -> Self {
        RotateProcess { degrees }
    }
}

impl Process for RotateProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        let rotated = match self.degrees % 360 {
            90 => rotate90(&img.di),
            180 => rotate180(&img.di),
            270 => rotate270(&img.di),
            _ => return Ok(img),
        };
        img.di = DynamicImage::ImageRgba8(rotated);
        img.buffer.clear();
        Ok(img)
    }
}

pub struct BrightenProcess {
    value: i32,
}

impl BrightenProcess {
    pub fn new(value: i32) -> Self {
        BrightenProcess { value }
    }
}

impl Process for BrightenProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        img.di = DynamicImage::ImageRgba8(brighten(&img.di, self.value));
        img.buffer.clear();
        Ok(img)
    }
}

pub struct ContrastProcess {
    value: f32,
}

impl ContrastProcess {
    pub fn new(value: f32) -> Self {
        ContrastProcess { value }
    }
}

impl Process for ContrastProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        img.di = img.di.adjust_contrast(self.value);
        img.buffer.clear();
        Ok(img)
    }
}

pub struct SharpenProcess {
    sigma: f32,
    threshold: i32,
}

impl SharpenProcess {
    pub fn new(sigma: f32, threshold: i32) -> Self {
        SharpenProcess { sigma, threshold }
    }
}

impl Process for SharpenProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        img.di = DynamicImage::ImageRgba8(unsharpen(&img.di, self.sigma, self.threshold));
        img.buffer.clear();
        Ok(img)
    }
}

pub struct BlurProcess {
    sigma: f32,
}

impl BlurProcess {
    pub fn new(sigma: f32) -> Self {
        BlurProcess { sigma }
    }
}

impl Process for BlurProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        img.di = DynamicImage::ImageRgba8(blur(&img.di, self.sigma));
        img.buffer.clear();
        Ok(img)
    }
}

#[derive(Default)]
pub struct StripProcess;

impl StripProcess {
    pub fn new() -> Self {
        StripProcess
    }
}

impl Process for StripProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        if img.buffer.is_empty() {
            return Ok(img);
        }
        let buf = std::mem::take(&mut img.buffer);
        img.buffer = strip_exif_bytes(buf, &img.ext);
        Ok(img)
    }
}

fn parse_hex_color(color: &str) -> image::Rgba<u8> {
    let hex = color.trim_start_matches('#');
    let parse = |s: &str| u8::from_str_radix(s, 16).unwrap_or(0);
    match hex.len() {
        6 => image::Rgba([parse(&hex[0..2]), parse(&hex[2..4]), parse(&hex[4..6]), 255]),
        8 => image::Rgba([
            parse(&hex[0..2]),
            parse(&hex[2..4]),
            parse(&hex[4..6]),
            parse(&hex[6..8]),
        ]),
        _ => image::Rgba([0, 0, 0, 0]),
    }
}

pub struct PaddingProcess {
    width: u32,
    height: u32,
    color: image::Rgba<u8>,
}

impl PaddingProcess {
    pub fn new(width: u32, height: u32, color: &str) -> Self {
        PaddingProcess {
            width,
            height,
            color: parse_hex_color(color),
        }
    }
}

impl Process for PaddingProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        let src_w = img.di.width();
        let src_h = img.di.height();
        let dst_w = self.width.max(src_w);
        let dst_h = self.height.max(src_h);

        if dst_w == src_w && dst_h == src_h {
            return Ok(img);
        }

        let mut canvas = RgbaImage::from_pixel(dst_w, dst_h, self.color);
        let x = ((dst_w - src_w) / 2) as i64;
        let y = ((dst_h - src_h) / 2) as i64;
        overlay(&mut canvas, &img.di, x, y);
        img.di = DynamicImage::ImageRgba8(canvas);
        img.buffer.clear();
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

impl Process for WatermarkProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        let w = img.di.width() as i64;
        let h = img.di.height() as i64;
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
        let mut bottom = img.di;
        overlay(&mut bottom, &self.watermark, x, y);
        img.buffer.clear();
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

impl Process for CropProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        let mut r = img.di;
        let result = crop(&mut r, self.x, self.y, self.width, self.height);
        img.di = DynamicImage::ImageRgba8(result.to_image());
        img.buffer.clear();
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

impl Process for OptimProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;

        // Move img.di into ImageInfo: zero-copy when already ImageRgba8, one conversion otherwise.
        let di = std::mem::take(&mut img.di);
        let info: ImageInfo = match di {
            DynamicImage::ImageRgba8(rgba) => ImageInfo { image: rgba },
            other => ImageInfo {
                image: other.to_rgba8(),
            },
        };

        let quality = self.quality;
        let speed = self.speed;
        let original_type = img.ext.clone();
        let original_size = img.buffer.len();
        let mut output_type = self.output_type.clone();
        if output_type.is_empty() {
            output_type.clone_from(&original_type);
        }
        img.ext.clone_from(&output_type);

        let data = match output_type.as_str() {
            IMAGE_TYPE_GIF => {
                let c = Cursor::new(&img.buffer);
                to_gif(c, speed).context(ImagesSnafu {})?
            }
            IMAGE_TYPE_PNG => info.to_png(quality).context(ImagesSnafu {})?,
            IMAGE_TYPE_AVIF => info.to_avif(quality, speed).context(ImagesSnafu {})?,
            IMAGE_TYPE_WEBP => info.to_webp(quality).context(ImagesSnafu {})?,
            _ => {
                img.ext = IMAGE_TYPE_JPEG.to_string();
                info.to_mozjpeg(quality).context(ImagesSnafu {})?
            }
        };

        if img.ext != original_type || data.len() < original_size || original_size == 0 {
            img.buffer = data;
            if img.support_dssim() {
                let result = if img.ext == IMAGE_TYPE_AVIF {
                    avif_decode(&img.buffer).context(ImagesSnafu {})
                } else {
                    let c = Cursor::new(&img.buffer);
                    let format = ImageFormat::from_extension(&img.ext).unwrap_or(ImageFormat::Jpeg);
                    load(c, format).context(ImageSnafu {})
                };
                img.di = result.unwrap_or(DynamicImage::ImageRgba8(info.image));
            } else {
                img.di = DynamicImage::ImageRgba8(info.image);
            }
        } else {
            img.di = DynamicImage::ImageRgba8(info.image);
        }

        Ok(img)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BlurProcess, BrightenProcess, ContrastProcess, CropProcess, FlipProcess, GrayProcess,
        LoaderProcess, OptimProcess, PaddingProcess, ResizeProcess, RotateProcess, SharpenProcess,
        StripProcess, WatermarkProcess,
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
    fn test_exif_orientation() {
        use super::{apply_orientation, get_exif_orientation};

        // PNG has no EXIF → orientation 1 (no-op)
        let data = include_bytes!("../assets/rust-logo.png");
        assert_eq!(get_exif_orientation(data), 1);

        // Loading a PNG: buffer is preserved (orientation == 1)
        let img = ProcessImage::new(data.to_vec(), "png").unwrap();
        assert!(!img.buffer.is_empty());
        assert_eq!(img.di.width(), 144);
        assert_eq!(img.di.height(), 144);

        // apply_orientation is a no-op for orientation 1
        let orig = ProcessImage::new(data.to_vec(), "png").unwrap();
        let result = apply_orientation(orig.di.clone(), 1);
        assert_eq!(result.width(), orig.di.width());

        // Orientation 3 (180°): apply twice → back to original
        let rotated = apply_orientation(orig.di.clone(), 3);
        let back = apply_orientation(rotated, 3);
        assert_eq!(
            back.as_rgba8().unwrap().get_pixel(0, 0).0,
            orig.di.as_rgba8().unwrap().get_pixel(0, 0).0
        );

        // Orientation 6 (90° CW): apply four times → back to original
        let mut di = orig.di.clone();
        for _ in 0..4 {
            di = apply_orientation(di, 6);
        }
        assert_eq!(
            di.as_rgba8().unwrap().get_pixel(0, 0).0,
            orig.di.as_rgba8().unwrap().get_pixel(0, 0).0
        );
    }

    #[test]
    fn test_resize_process() {
        let p = new_process_image();
        let result = tokio_test::block_on(ResizeProcess::new(48, 0).process(p)).unwrap();
        assert_eq!(result.di.width(), 48);
        assert_eq!(result.di.height(), 48);
    }

    #[test]
    fn test_fit_process() {
        // source is 144×144

        // exceeds max: scale down to fit within 80×80
        let result =
            tokio_test::block_on(ResizeProcess::new_fit(80, 80).process(new_process_image()))
                .unwrap();
        assert_eq!(result.di.width(), 80);
        assert_eq!(result.di.height(), 80);

        // already fits: no-op
        let result =
            tokio_test::block_on(ResizeProcess::new_fit(200, 200).process(new_process_image()))
                .unwrap();
        assert_eq!(result.di.width(), 144);
        assert_eq!(result.di.height(), 144);

        // only width constrained
        let result =
            tokio_test::block_on(ResizeProcess::new_fit(72, 0).process(new_process_image()))
                .unwrap();
        assert_eq!(result.di.width(), 72);
        assert_eq!(result.di.height(), 72);

        // only height constrained
        let result =
            tokio_test::block_on(ResizeProcess::new_fit(0, 48).process(new_process_image()))
                .unwrap();
        assert_eq!(result.di.width(), 48);
        assert_eq!(result.di.height(), 48);

        // both zero: no-op
        let result =
            tokio_test::block_on(ResizeProcess::new_fit(0, 0).process(new_process_image()))
                .unwrap();
        assert_eq!(result.di.width(), 144);
    }

    #[test]
    fn test_gray_process() {
        let p = new_process_image();
        let result = tokio_test::block_on(GrayProcess::new().process(p)).unwrap();
        assert_eq!(result.di.width(), 144);
        assert_eq!(result.di.height(), 144);
    }

    #[test]
    fn test_flip_process() {
        let orig = new_process_image();
        let orig_img = orig.di.as_rgba8().unwrap().clone();

        // horizontal: top-left becomes top-right of original
        let flipped_h =
            tokio_test::block_on(FlipProcess::new("h").process(new_process_image())).unwrap();
        assert_eq!(flipped_h.di.width(), 144);
        assert_eq!(flipped_h.di.height(), 144);
        assert_eq!(
            flipped_h.di.as_rgba8().unwrap().get_pixel(0, 0).0,
            orig_img.get_pixel(143, 0).0
        );

        // vertical: top-left becomes bottom-left of original
        let flipped_v =
            tokio_test::block_on(FlipProcess::new("v").process(new_process_image())).unwrap();
        assert_eq!(flipped_v.di.width(), 144);
        assert_eq!(flipped_v.di.height(), 144);
        assert_eq!(
            flipped_v.di.as_rgba8().unwrap().get_pixel(0, 0).0,
            orig_img.get_pixel(0, 143).0
        );

        // "horizontal" and "vertical" are valid aliases for "h" and "v"
        let flipped_h2 =
            tokio_test::block_on(FlipProcess::new("horizontal").process(new_process_image()))
                .unwrap();
        assert_eq!(
            flipped_h2.di.as_rgba8().unwrap().get_pixel(0, 0).0,
            flipped_h.di.as_rgba8().unwrap().get_pixel(0, 0).0
        );
        let flipped_v2 =
            tokio_test::block_on(FlipProcess::new("vertical").process(new_process_image()))
                .unwrap();
        assert_eq!(
            flipped_v2.di.as_rgba8().unwrap().get_pixel(0, 0).0,
            flipped_v.di.as_rgba8().unwrap().get_pixel(0, 0).0
        );
    }

    #[test]
    fn test_rotate_process() {
        let orig = new_process_image();
        let orig_img = orig.di.as_rgba8().unwrap().clone();

        // 90°: top-left of result == bottom-left of original
        let r90 =
            tokio_test::block_on(RotateProcess::new(90).process(new_process_image())).unwrap();
        assert_eq!(r90.di.width(), 144);
        assert_eq!(r90.di.height(), 144);
        assert_eq!(
            r90.di.as_rgba8().unwrap().get_pixel(0, 0).0,
            orig_img.get_pixel(0, 143).0
        );

        // 180°: top-left of result == bottom-right of original
        let r180 =
            tokio_test::block_on(RotateProcess::new(180).process(new_process_image())).unwrap();
        assert_eq!(r180.di.width(), 144);
        assert_eq!(r180.di.height(), 144);
        assert_eq!(
            r180.di.as_rgba8().unwrap().get_pixel(0, 0).0,
            orig_img.get_pixel(143, 143).0
        );

        // 270°: top-left of result == top-right of original
        let r270 =
            tokio_test::block_on(RotateProcess::new(270).process(new_process_image())).unwrap();
        assert_eq!(r270.di.width(), 144);
        assert_eq!(r270.di.height(), 144);
        assert_eq!(
            r270.di.as_rgba8().unwrap().get_pixel(0, 0).0,
            orig_img.get_pixel(143, 0).0
        );

        // 0° and other values are no-ops
        let r0 = tokio_test::block_on(RotateProcess::new(0).process(new_process_image())).unwrap();
        assert_eq!(
            r0.di.as_rgba8().unwrap().get_pixel(0, 0).0,
            orig_img.get_pixel(0, 0).0
        );
        let r45 =
            tokio_test::block_on(RotateProcess::new(45).process(new_process_image())).unwrap();
        assert_eq!(r45.di.width(), 144);
    }

    #[test]
    fn test_brighten_process() {
        let p = new_process_image();
        let orig_pixel = p.di.as_rgba8().unwrap().get_pixel(72, 72).0;

        // Positive value brightens: each channel increases (clamped at 255)
        let brightened =
            tokio_test::block_on(BrightenProcess::new(50).process(new_process_image())).unwrap();
        assert_eq!(brightened.di.width(), 144);
        let b_pixel = brightened.di.as_rgba8().unwrap().get_pixel(72, 72).0;
        for i in 0..3 {
            assert!(b_pixel[i] >= orig_pixel[i]);
        }

        // Negative value darkens: each channel decreases (clamped at 0)
        let darkened =
            tokio_test::block_on(BrightenProcess::new(-50).process(new_process_image())).unwrap();
        let d_pixel = darkened.di.as_rgba8().unwrap().get_pixel(72, 72).0;
        for i in 0..3 {
            assert!(d_pixel[i] <= orig_pixel[i]);
        }

        // Zero is a no-op
        let noop =
            tokio_test::block_on(BrightenProcess::new(0).process(new_process_image())).unwrap();
        assert_eq!(noop.di.as_rgba8().unwrap().get_pixel(72, 72).0, orig_pixel);
    }

    #[test]
    fn test_contrast_process() {
        let p = new_process_image();
        assert_eq!(p.di.width(), 144);

        // Dimensions are always preserved
        let increased =
            tokio_test::block_on(ContrastProcess::new(30.0).process(new_process_image())).unwrap();
        assert_eq!(increased.di.width(), 144);
        assert_eq!(increased.di.height(), 144);

        let decreased =
            tokio_test::block_on(ContrastProcess::new(-30.0).process(new_process_image())).unwrap();
        assert_eq!(decreased.di.width(), 144);
        assert_eq!(decreased.di.height(), 144);
    }

    #[test]
    fn test_sharpen_process() {
        let result =
            tokio_test::block_on(SharpenProcess::new(1.0, 0).process(new_process_image())).unwrap();
        assert_eq!(result.di.width(), 144);
        assert_eq!(result.di.height(), 144);
        // Sharpening changes pixel values — result must differ from original somewhere
        let orig = new_process_image();
        let any_different = orig
            .di
            .as_rgba8()
            .unwrap()
            .pixels()
            .zip(result.di.as_rgba8().unwrap().pixels())
            .any(|(a, b)| a != b);
        assert!(any_different);
    }

    #[test]
    fn test_blur_process() {
        let result =
            tokio_test::block_on(BlurProcess::new(2.0).process(new_process_image())).unwrap();
        assert_eq!(result.di.width(), 144);
        assert_eq!(result.di.height(), 144);
        // Blurring changes pixel values
        let orig = new_process_image();
        let any_different = orig
            .di
            .as_rgba8()
            .unwrap()
            .pixels()
            .zip(result.di.as_rgba8().unwrap().pixels())
            .any(|(a, b)| a != b);
        assert!(any_different);
    }

    #[test]
    fn test_strip_process() {
        use crate::image_processing::strip_exif_bytes;

        // PNG has no EXIF: strip is a no-op, bytes are unchanged
        let data = include_bytes!("../assets/rust-logo.png").to_vec();
        let stripped = strip_exif_bytes(data.clone(), "png");
        assert_eq!(stripped.len(), data.len());

        // Unknown extension: bytes are returned unchanged
        let data = include_bytes!("../assets/rust-logo.png").to_vec();
        let stripped = strip_exif_bytes(data.clone(), "avif");
        assert_eq!(stripped.len(), data.len());

        // StripProcess on a PNG ProcessImage: buffer stays the same length
        let p = new_process_image();
        let original_buf_len = p.buffer.len();
        let result = tokio_test::block_on(StripProcess::new().process(p)).unwrap();
        assert_eq!(result.buffer.len(), original_buf_len);

        // StripProcess with empty buffer: no-op
        let mut empty = new_process_image();
        empty.buffer.clear();
        let result = tokio_test::block_on(StripProcess::new().process(empty)).unwrap();
        assert!(result.buffer.is_empty());
    }

    #[test]
    fn test_padding_process() {
        // Pad to 200x200: canvas expands, original (144x144) is centered
        let result =
            tokio_test::block_on(PaddingProcess::new(200, 200, "").process(new_process_image()))
                .unwrap();
        assert_eq!(result.di.width(), 200);
        assert_eq!(result.di.height(), 200);
        // Top-left corner is the fill color (transparent by default)
        assert_eq!(
            result.di.as_rgba8().unwrap().get_pixel(0, 0).0,
            [0, 0, 0, 0]
        );

        // With white fill color
        let result = tokio_test::block_on(
            PaddingProcess::new(200, 200, "#ffffff").process(new_process_image()),
        )
        .unwrap();
        assert_eq!(
            result.di.as_rgba8().unwrap().get_pixel(0, 0).0,
            [255, 255, 255, 255]
        );

        // Padding smaller than source is a no-op
        let result =
            tokio_test::block_on(PaddingProcess::new(100, 100, "").process(new_process_image()))
                .unwrap();
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
        assert_eq!(result.buffer.len(), 1463);
        assert_ne!(result.get_diff(), 0.0_f64);
        assert_ne!(result.get_diff(), -1.0_f64);

        let result =
            tokio_test::block_on(OptimProcess::new("avif", 70, 0).process(new_process_image()))
                .unwrap();
        assert_eq!(result.ext, "avif");
        assert_eq!(result.buffer.len(), 2367);
        assert_ne!(result.get_diff(), 0.0_f64);
        assert_ne!(result.get_diff(), -1.0_f64);

        // lossless webp (quality >= 100)
        let result =
            tokio_test::block_on(OptimProcess::new("webp", 100, 0).process(new_process_image()))
                .unwrap();
        assert_eq!(result.ext, "webp");
        assert_eq!(result.buffer.len(), 2764);
        assert_eq!(result.get_diff(), 0.0);

        // lossy webp
        let result =
            tokio_test::block_on(OptimProcess::new("webp", 80, 0).process(new_process_image()))
                .unwrap();
        assert_eq!(result.ext, "webp");
        assert_ne!(result.buffer.len(), 0);
        assert!(result.buffer.len() < 2764);
        assert!(result.get_diff() >= 0.0);

        let result =
            tokio_test::block_on(OptimProcess::new("jpeg", 70, 0).process(new_process_image()))
                .unwrap();
        assert_eq!(result.ext, "jpeg");
        assert_eq!(result.buffer.len(), 392);
        assert_ne!(result.get_diff(), 0.0_f64);
        assert_ne!(result.get_diff(), -1.0_f64);
    }
}
