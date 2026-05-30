use super::images::{avif_decode, jxl_decode, to_gif, ImageError, ImageInfo};
use base64::{engine::general_purpose, Engine as _};
use bytes::Bytes;
use dssim_core::Dssim;
use exif::{In, Reader, Tag};
use fast_image_resize::images::Image as FirImage;
use fast_image_resize::{FilterType as FirFilter, PixelType, ResizeAlg, ResizeOptions, Resizer};
use image::imageops::{
    crop, flip_horizontal, flip_vertical, overlay, rotate180, rotate270, rotate90,
};
use image::{load, DynamicImage, ImageFormat, RgbaImage};
use img_parts::ImageEXIF;
use rayon::prelude::*;
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
pub const PROCESS_HUE: &str = "hue";
pub const PROCESS_SATURATE: &str = "saturate";
pub const PROCESS_THUMBNAIL: &str = "thumbnail";
pub const PROCESS_INVERT: &str = "invert";
pub const PROCESS_OPACITY: &str = "opacity";
pub const PROCESS_GAMMA: &str = "gamma";
pub const PROCESS_BACKGROUND: &str = "background";
pub const PROCESS_NORMALIZE: &str = "normalize";
pub const PROCESS_TRIM: &str = "trim";

const IMAGE_TYPE_GIF: &str = "gif";
const IMAGE_TYPE_PNG: &str = "png";
const IMAGE_TYPE_AVIF: &str = "avif";
const IMAGE_TYPE_WEBP: &str = "webp";
const IMAGE_TYPE_JPEG: &str = "jpeg";
const IMAGE_TYPE_JXL: &str = "jxl";

/// Default perceptual-diff target (DSSIM ×1000) treated as "visually lossless".
/// Mirrors the CLI threshold where a diff above 1.0 is highlighted as lossy.
const AUTO_TARGET_DIFF: f64 = 1.0;
/// Quality search bounds for auto-quality tuning.
const AUTO_MIN_QUALITY: u8 = 30;
const AUTO_MAX_QUALITY: u8 = 95;

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

/// Auto-quality optim task: binary-search the lowest quality whose perceptual diff stays
/// within `target` (DSSIM ×1000). Encoded as ["optim", output_type, "auto", speed, target].
pub fn new_auto_quality_task(output_type: &str, speed: u8, target: f64) -> Vec<String> {
    vec![
        PROCESS_OPTIM.to_string(),
        output_type.to_string(),
        "auto".to_string(),
        speed.to_string(),
        target.to_string(),
    ]
}

/// Auto-format optim task: encode candidate formats at `quality` and keep the smallest
/// one within `target`. Encoded as ["optim", "auto", quality, speed, target].
pub fn new_auto_format_task(quality: u8, speed: u8, target: f64) -> Vec<String> {
    vec![
        PROCESS_OPTIM.to_string(),
        "auto".to_string(),
        quality.to_string(),
        speed.to_string(),
        target.to_string(),
    ]
}

/// Full auto optim task: search both output format and quality for the smallest output
/// within `target`. Encoded as ["optim", "auto", "auto", speed, target].
pub fn new_auto_task(speed: u8, target: f64) -> Vec<String> {
    vec![
        PROCESS_OPTIM.to_string(),
        "auto".to_string(),
        "auto".to_string(),
        speed.to_string(),
        target.to_string(),
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

/// Rotate the hue of every pixel by `shift` degrees (-180..=180 or any integer; wraps around).
pub fn new_hue_task(shift: i32) -> Vec<String> {
    vec![PROCESS_HUE.to_string(), shift.to_string()]
}

/// Multiply the saturation of every pixel by `factor` (0.0 = grayscale, 1.0 = unchanged, >1.0 = boost).
pub fn new_saturate_task(factor: f32) -> Vec<String> {
    vec![PROCESS_SATURATE.to_string(), factor.to_string()]
}

/// Scale the image to cover `width × height` (fill mode), then center-crop to exactly that size.
pub fn new_thumbnail_task(width: u32, height: u32) -> Vec<String> {
    vec![
        PROCESS_THUMBNAIL.to_string(),
        width.to_string(),
        height.to_string(),
    ]
}

/// Like `new_thumbnail_task`, but content-aware: instead of center-cropping, the crop
/// window slides to the region with the most detail (luminance-gradient energy) with a
/// mild center bias. Keeps the subject in frame for off-center compositions.
pub fn new_smart_thumbnail_task(width: u32, height: u32) -> Vec<String> {
    vec![
        PROCESS_THUMBNAIL.to_string(),
        width.to_string(),
        height.to_string(),
        "smart".to_string(),
    ]
}

/// Invert RGB channels of every pixel; alpha is preserved.
pub fn new_invert_task() -> Vec<String> {
    vec![PROCESS_INVERT.to_string()]
}

/// Multiply every pixel's alpha by `factor` (0.0 = fully transparent, 1.0 = unchanged).
pub fn new_opacity_task(factor: f32) -> Vec<String> {
    vec![PROCESS_OPACITY.to_string(), factor.to_string()]
}

/// Apply gamma correction: `output = (input/255)^gamma * 255`; alpha is unaffected.
/// gamma = 1.0 is a no-op; gamma < 1.0 brightens midtones; gamma > 1.0 darkens.
pub fn new_gamma_task(gamma: f32) -> Vec<String> {
    vec![PROCESS_GAMMA.to_string(), gamma.to_string()]
}

/// Composite the image over a solid background color, flattening transparency.
/// `color` is a hex string (`#rrggbb` or `#rrggbbaa`); an empty string defaults to
/// opaque white. Useful before encoding to a format without alpha (JPEG/JXL) so
/// transparent areas take the background color instead of turning black.
pub fn new_background_task(color: &str) -> Vec<String> {
    vec![PROCESS_BACKGROUND.to_string(), color.to_string()]
}

/// Stretch the histogram so the darkest/brightest pixels map to 0/255 (auto-contrast).
/// `per_channel` true normalizes R/G/B independently (maximizes contrast but may shift
/// color balance); false derives a single mapping from luminance, preserving color.
pub fn new_normalize_task(per_channel: bool) -> Vec<String> {
    vec![
        PROCESS_NORMALIZE.to_string(),
        if per_channel { "rgb" } else { "luma" }.to_string(),
    ]
}

/// Auto-crop a uniform border. The top-left pixel is the reference color; outer rows
/// and columns whose pixels are all within `tolerance` (max per-channel RGBA
/// difference) of it are removed. A fully uniform image is left unchanged.
pub fn new_trim_task(tolerance: u8) -> Vec<String> {
    vec![PROCESS_TRIM.to_string(), tolerance.to_string()]
}

pub async fn run_with_image(
    mut image: ProcessImage,
    tasks: Vec<Vec<String>>,
) -> Result<ProcessImage> {
    let he = ParamsInvalidSnafu {
        message: "params is invalid",
    };
    let needs_diff = tasks.iter().any(|t| {
        let head = t.first().map(|s| s.as_str());
        if head == Some(PROCESS_DIFF) {
            return true;
        }
        // Auto optimisation scores candidates internally, so it also needs the
        // original RGBA snapshot kept even when no explicit diff task is present.
        if head == Some(PROCESS_OPTIM) {
            let auto_format = t.get(1).map(|s| s == "auto").unwrap_or(false);
            let auto_quality = t.get(2).map(|s| s == "auto").unwrap_or(false);
            return auto_format || auto_quality;
        }
        false
    });
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
                let mut loader = LoaderProcess::new(data, ext);
                loader.keep_original = needs_diff;
                image = loader.process(image).await?;
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
            PROCESS_HUE => {
                let shift = sub_params
                    .first()
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(0);
                image = HueProcess::new(shift).process(image).await?;
            }
            PROCESS_SATURATE => {
                let factor = sub_params
                    .first()
                    .and_then(|s| s.parse::<f32>().ok())
                    .unwrap_or(1.0);
                image = SaturateProcess::new(factor).process(image).await?;
            }
            PROCESS_THUMBNAIL => {
                ensure!(sub_params.len() >= 2, he);
                let width = sub_params[0].parse::<u32>().context(ParseIntSnafu {})?;
                let height = sub_params[1].parse::<u32>().context(ParseIntSnafu {})?;
                let smart = sub_params.get(2).map(|s| s == "smart").unwrap_or(false);
                let proc = if smart {
                    ThumbnailProcess::new_smart(width, height)
                } else {
                    ThumbnailProcess::new(width, height)
                };
                image = proc.process(image).await?;
            }
            PROCESS_INVERT => {
                image = InvertProcess::new().process(image).await?;
            }
            PROCESS_OPACITY => {
                let factor = sub_params
                    .first()
                    .and_then(|s| s.parse::<f32>().ok())
                    .unwrap_or(1.0);
                image = OpacityProcess::new(factor).process(image).await?;
            }
            PROCESS_GAMMA => {
                let gamma = sub_params
                    .first()
                    .and_then(|s| s.parse::<f32>().ok())
                    .unwrap_or(1.0);
                image = GammaProcess::new(gamma).process(image).await?;
            }
            PROCESS_BACKGROUND => {
                let color = sub_params.first().map(|s| s.as_str()).unwrap_or("");
                image = BackgroundProcess::new(color).process(image).await?;
            }
            PROCESS_NORMALIZE => {
                let per_channel = sub_params.first().map(|s| s != "luma").unwrap_or(true);
                image = NormalizeProcess::new(per_channel).process(image).await?;
            }
            PROCESS_TRIM => {
                let tolerance = sub_params
                    .first()
                    .and_then(|s| s.parse::<u8>().ok())
                    .unwrap_or(0);
                image = TrimProcess::new(tolerance).process(image).await?;
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
                let quality_field = &sub_params[1];
                let speed = sub_params[2].parse::<u8>().context(ParseIntSnafu {})?;
                let auto_format = output_type == "auto";
                let auto_quality = quality_field == "auto";
                if auto_format || auto_quality {
                    // Optional 4th arg overrides the perceptual-diff target.
                    let target = sub_params
                        .get(3)
                        .and_then(|s| s.parse::<f64>().ok())
                        .unwrap_or(AUTO_TARGET_DIFF);
                    let out = if auto_format {
                        ""
                    } else {
                        output_type.as_str()
                    };
                    let quality = if auto_quality {
                        None
                    } else {
                        Some(quality_field.parse::<u8>().context(ParseIntSnafu {})?)
                    };
                    image = AutoOptimProcess::new(out, quality, speed, target)
                        .process(image)
                        .await?;
                } else {
                    let quality = quality_field.parse::<u8>().context(ParseIntSnafu {})?;
                    image = OptimProcess::new(output_type, quality, speed)
                        .process(image)
                        .await?;
                }
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
                image.original = None;
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

/// SIMD-accelerated resize via `fast_image_resize` (NEON/AVX2/SSE4.1, scalar fallback on
/// older CPUs). Consumes the source buffer (moved into the resizer with no copy) and
/// returns the scaled RGBA image. Alpha is premultiplied during scaling (fir default),
/// which avoids color bleeding into fully transparent edges.
fn fir_resize(src: RgbaImage, dst_w: u32, dst_h: u32, filter: FirFilter) -> RgbaImage {
    let (sw, sh) = (src.width(), src.height());
    let src_img = FirImage::from_vec_u8(sw, sh, src.into_raw(), PixelType::U8x4)
        .expect("rgba8 buffer matches U8x4 dimensions");
    let mut dst_img = FirImage::new(dst_w, dst_h, PixelType::U8x4);
    Resizer::new()
        .resize(
            &src_img,
            &mut dst_img,
            &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(filter)),
        )
        .expect("source and destination are both U8x4");
    RgbaImage::from_raw(dst_w, dst_h, dst_img.into_vec())
        .expect("fir output buffer matches dimensions")
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
        Self::new_impl(data, ext, true)
    }

    fn new_impl(data: Vec<u8>, ext: &str, keep_original: bool) -> Result<Self> {
        let di = if ext == IMAGE_TYPE_JXL {
            jxl_decode(&data).context(ImagesSnafu {})?
        } else {
            let format = image::guess_format(&data).or_else(|_| {
                ImageFormat::from_extension(ext).ok_or(ImageProcessingError::ParamsInvalid {
                    message: "Image format is not supported".to_string(),
                })
            })?;
            load(Cursor::new(&data), format).context(ImageSnafu {})?
        };
        let orientation = get_exif_orientation(&data);
        let di = apply_orientation(di, orientation);
        let original_size = data.len();
        // Clear buffer when orientation was corrected so get_buffer() re-encodes
        // from the oriented di rather than returning bytes with the stale EXIF tag.
        let buffer = if orientation == 1 { data } else { vec![] };
        Ok(ProcessImage {
            original_size,
            original: if keep_original {
                Some(di.to_rgba8())
            } else {
                None
            },
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
        dssim_score(original, &self.di)
    }
}

/// Compute the DSSIM score (×1000) between an original RGBA snapshot and a candidate
/// image. Returns -1.0 when the dimensions differ (not comparable).
fn dssim_score(original: &RgbaImage, di: &DynamicImage) -> f64 {
    // 如果宽高不一致，则不比对
    if original.width() != di.width() || original.height() != di.height() {
        return -1.0;
    }
    let width = original.width() as usize;
    let height = original.height() as usize;
    let attr = Dssim::new();
    let gp1 = attr
        .create_image_rgba(original.as_raw().as_rgba(), width, height)
        .unwrap();
    let tmp;
    let current_rgba = match di {
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

/// Encode an `ImageInfo` to bytes for a quality-tunable format. GIF is not supported here
/// (it has no quality knob and cannot be perceptually scored).
fn encode_info(info: &ImageInfo, ext: &str, quality: u8, speed: u8) -> Result<Vec<u8>> {
    let encoded = match ext {
        IMAGE_TYPE_PNG => info.to_png(quality).context(ImagesSnafu {})?,
        IMAGE_TYPE_AVIF => info.to_avif(quality, speed).context(ImagesSnafu {})?,
        IMAGE_TYPE_WEBP => info.to_webp(quality).context(ImagesSnafu {})?,
        IMAGE_TYPE_JXL => info.to_jxl(quality).context(ImagesSnafu {})?,
        _ => info.to_mozjpeg(quality).context(ImagesSnafu {})?,
    };
    Ok(encoded)
}

/// Decode encoded bytes back to a `DynamicImage` so the lossy output can be scored.
fn decode_to_di(buffer: &[u8], ext: &str) -> Result<DynamicImage> {
    match ext {
        IMAGE_TYPE_AVIF => avif_decode(buffer).context(ImagesSnafu {}),
        IMAGE_TYPE_JXL => jxl_decode(buffer).context(ImagesSnafu {}),
        _ => {
            let format = ImageFormat::from_extension(ext).unwrap_or(ImageFormat::Jpeg);
            load(Cursor::new(buffer), format).context(ImageSnafu {})
        }
    }
}

/// True when any pixel is not fully opaque (so an alpha-capable format is required).
fn has_alpha(image: &RgbaImage) -> bool {
    image.as_raw().par_chunks(4).any(|p| p[3] < 255)
}

#[allow(async_fn_in_trait)]
pub trait Process {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage>;
}

/// Loader process loads the image data from http, file or base64.
pub struct LoaderProcess {
    data: String,
    ext: String,
    pub keep_original: bool,
}

impl LoaderProcess {
    pub fn new(data: &str, ext: &str) -> Self {
        LoaderProcess {
            data: data.to_string(),
            ext: ext.to_string(),
            keep_original: true,
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
        ProcessImage::new_impl(original_data, &ext, self.keep_original)
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

        let result = fir_resize(
            std::mem::take(&mut img.di).into_rgba8(),
            new_w,
            new_h,
            FirFilter::Lanczos3,
        );
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
        let di = std::mem::take(&mut img.di);
        let mut rgba = di.into_rgba8();
        rgba.as_flat_samples_mut()
            .as_mut_slice()
            .par_chunks_mut(4)
            .for_each(|p| {
                let luma =
                    (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32) as u8;
                p[0] = luma;
                p[1] = luma;
                p[2] = luma;
            });
        img.di = DynamicImage::ImageRgba8(rgba);
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
        let value = self.value;
        let di = std::mem::take(&mut img.di);
        let mut rgba = di.into_rgba8();
        rgba.as_flat_samples_mut()
            .as_mut_slice()
            .par_chunks_mut(4)
            .for_each(|p| {
                p[0] = (p[0] as i32 + value).clamp(0, 255) as u8;
                p[1] = (p[1] as i32 + value).clamp(0, 255) as u8;
                p[2] = (p[2] as i32 + value).clamp(0, 255) as u8;
            });
        img.di = DynamicImage::ImageRgba8(rgba);
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
        let factor = (128.0 + self.value) / 128.0;
        let di = std::mem::take(&mut img.di);
        let mut rgba = di.into_rgba8();
        rgba.as_flat_samples_mut()
            .as_mut_slice()
            .par_chunks_mut(4)
            .for_each(|p| {
                p[0] = ((p[0] as f32 - 128.0) * factor + 128.0).clamp(0.0, 255.0) as u8;
                p[1] = ((p[1] as f32 - 128.0) * factor + 128.0).clamp(0.0, 255.0) as u8;
                p[2] = ((p[2] as f32 - 128.0) * factor + 128.0).clamp(0.0, 255.0) as u8;
            });
        img.di = DynamicImage::ImageRgba8(rgba);
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
        let di = std::mem::take(&mut img.di);
        img.di = DynamicImage::ImageRgba8(parallel_unsharpen(
            di.into_rgba8(),
            self.sigma,
            self.threshold,
        ));
        img.buffer.clear();
        Ok(img)
    }
}

fn gaussian_kernel_1d(sigma: f32) -> Vec<f32> {
    let sigma = sigma.max(f32::EPSILON);
    let radius = (2.0 * sigma).ceil() as usize;
    let size = 2 * radius + 1;
    let mut kernel = vec![0.0f32; size];
    let sigma2 = 2.0 * sigma * sigma;
    for (i, k) in kernel.iter_mut().enumerate() {
        let x = i as f32 - radius as f32;
        *k = (-x * x / sigma2).exp();
    }
    let sum: f32 = kernel.iter().sum();
    kernel.iter_mut().for_each(|k| *k /= sum);
    kernel
}

fn convolve_rows(src: &[u8], dst: &mut [u8], w: u32, _h: u32, kernel: &[f32]) {
    let radius = (kernel.len() / 2) as i32;
    dst.par_chunks_mut(w as usize * 4)
        .enumerate()
        .for_each(|(y, row)| {
            for x in 0..w as i32 {
                let mut rgba = [0.0f32; 4];
                for (ki, &kv) in kernel.iter().enumerate() {
                    let sx = (x + ki as i32 - radius).clamp(0, w as i32 - 1) as u32;
                    let idx = (y as u32 * w + sx) as usize * 4;
                    rgba[0] += src[idx] as f32 * kv;
                    rgba[1] += src[idx + 1] as f32 * kv;
                    rgba[2] += src[idx + 2] as f32 * kv;
                    rgba[3] += src[idx + 3] as f32 * kv;
                }
                let di = x as usize * 4;
                row[di] = rgba[0].round().clamp(0.0, 255.0) as u8;
                row[di + 1] = rgba[1].round().clamp(0.0, 255.0) as u8;
                row[di + 2] = rgba[2].round().clamp(0.0, 255.0) as u8;
                row[di + 3] = rgba[3].round().clamp(0.0, 255.0) as u8;
            }
        });
}

fn convolve_cols(src: &[u8], dst: &mut [u8], w: u32, h: u32, kernel: &[f32]) {
    let radius = (kernel.len() / 2) as i32;
    dst.par_chunks_mut(w as usize * 4)
        .enumerate()
        .for_each(|(y, row)| {
            for x in 0..w {
                let mut rgba = [0.0f32; 4];
                for (ki, &kv) in kernel.iter().enumerate() {
                    let sy = (y as i32 + ki as i32 - radius).clamp(0, h as i32 - 1) as u32;
                    let idx = (sy * w + x) as usize * 4;
                    rgba[0] += src[idx] as f32 * kv;
                    rgba[1] += src[idx + 1] as f32 * kv;
                    rgba[2] += src[idx + 2] as f32 * kv;
                    rgba[3] += src[idx + 3] as f32 * kv;
                }
                let di = x as usize * 4;
                row[di] = rgba[0].round().clamp(0.0, 255.0) as u8;
                row[di + 1] = rgba[1].round().clamp(0.0, 255.0) as u8;
                row[di + 2] = rgba[2].round().clamp(0.0, 255.0) as u8;
                row[di + 3] = rgba[3].round().clamp(0.0, 255.0) as u8;
            }
        });
}

fn parallel_blur(rgba: &RgbaImage, sigma: f32) -> RgbaImage {
    let (w, h) = rgba.dimensions();
    let kernel = gaussian_kernel_1d(sigma);
    let mut temp = vec![0u8; (w * h * 4) as usize];
    let mut out = vec![0u8; (w * h * 4) as usize];
    convolve_rows(rgba.as_raw(), &mut temp, w, h, &kernel);
    convolve_cols(&temp, &mut out, w, h, &kernel);
    RgbaImage::from_raw(w, h, out).unwrap()
}

fn parallel_unsharpen(rgba: RgbaImage, sigma: f32, threshold: i32) -> RgbaImage {
    let (w, h) = rgba.dimensions();
    let blurred = parallel_blur(&rgba, sigma);
    let orig_raw = rgba.into_raw();
    let blur_raw = blurred.into_raw();
    let mut dst_raw = vec![0u8; orig_raw.len()];
    dst_raw
        .par_chunks_mut(4)
        .zip(orig_raw.par_chunks(4))
        .zip(blur_raw.par_chunks(4))
        .for_each(|((dst, orig), blur)| {
            for i in 0..3 {
                let diff = orig[i] as i32 - blur[i] as i32;
                dst[i] = if diff.abs() >= threshold {
                    (orig[i] as i32 + diff).clamp(0, 255) as u8
                } else {
                    orig[i]
                };
            }
            dst[3] = orig[3];
        });
    RgbaImage::from_raw(w, h, dst_raw).unwrap()
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
        let di = std::mem::take(&mut img.di);
        img.di = DynamicImage::ImageRgba8(parallel_blur(&di.into_rgba8(), self.sigma));
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

fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let h = if delta < f32::EPSILON {
        0.0
    } else if (max - r).abs() < f32::EPSILON {
        60.0 * (((g - b) / delta) % 6.0)
    } else if (max - g).abs() < f32::EPSILON {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let h = if h < 0.0 { h + 360.0 } else { h };
    let s = if max < f32::EPSILON { 0.0 } else { delta / max };
    (h, s, max)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let h = ((h % 360.0) + 360.0) % 360.0;
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = if h < 60.0 {
        (c, x, 0.0)
    } else if h < 120.0 {
        (x, c, 0.0)
    } else if h < 180.0 {
        (0.0, c, x)
    } else if h < 240.0 {
        (0.0, x, c)
    } else if h < 300.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    (
        ((r + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((g + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((b + m) * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

pub struct HueProcess {
    shift: i32,
}

impl HueProcess {
    pub fn new(shift: i32) -> Self {
        HueProcess { shift }
    }
}

impl Process for HueProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        let di = std::mem::take(&mut img.di);
        let mut rgba = di.into_rgba8();
        let shift = self.shift as f32;
        rgba.as_flat_samples_mut()
            .as_mut_slice()
            .par_chunks_mut(4)
            .for_each(|p| {
                let (h, s, v) = rgb_to_hsv(p[0], p[1], p[2]);
                let (nr, ng, nb) = hsv_to_rgb(h + shift, s, v);
                p[0] = nr;
                p[1] = ng;
                p[2] = nb;
            });
        img.di = DynamicImage::ImageRgba8(rgba);
        img.buffer.clear();
        Ok(img)
    }
}

pub struct SaturateProcess {
    factor: f32,
}

impl SaturateProcess {
    pub fn new(factor: f32) -> Self {
        SaturateProcess { factor }
    }
}

impl Process for SaturateProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        let di = std::mem::take(&mut img.di);
        let mut rgba = di.into_rgba8();
        let factor = self.factor.max(0.0);
        rgba.as_flat_samples_mut()
            .as_mut_slice()
            .par_chunks_mut(4)
            .for_each(|p| {
                let rf = p[0] as f32;
                let gf = p[1] as f32;
                let bf = p[2] as f32;
                let luma = 0.2126 * rf + 0.7152 * gf + 0.0722 * bf;
                p[0] = (luma + factor * (rf - luma)).clamp(0.0, 255.0) as u8;
                p[1] = (luma + factor * (gf - luma)).clamp(0.0, 255.0) as u8;
                p[2] = (luma + factor * (bf - luma)).clamp(0.0, 255.0) as u8;
            });
        img.di = DynamicImage::ImageRgba8(rgba);
        img.buffer.clear();
        Ok(img)
    }
}

/// Return the window start (length `win`) that maximizes summed energy × center bias.
fn best_window(energy: &[f32], win: usize) -> usize {
    let n = energy.len();
    if win >= n {
        return 0;
    }
    let mut prefix = vec![0f32; n + 1];
    for i in 0..n {
        prefix[i + 1] = prefix[i] + energy[i];
    }
    if prefix[n] <= f32::EPSILON {
        return (n - win) / 2; // flat energy: fall back to center
    }
    let img_center = n as f32 / 2.0;
    let max_dist = (n - win) as f32 / 2.0;
    let mut best = 0usize;
    let mut best_score = f32::MIN;
    for start in 0..=(n - win) {
        let e = prefix[start + win] - prefix[start];
        let win_center = start as f32 + win as f32 / 2.0;
        // Mild bias toward windows centered near the image center.
        let bias = if max_dist > 0.0 {
            1.0 - 0.3 * ((win_center - img_center).abs() / max_dist)
        } else {
            1.0
        };
        let score = e * bias;
        if score > best_score {
            best_score = score;
            best = start;
        }
    }
    best
}

/// Pick a content-aware crop offset (top-left) for a `crop_w × crop_h` window using a
/// luminance-gradient energy map plus a mild center bias. Cover-cropping leaves slack on
/// only one axis, so just that axis is searched; the other is centered. Energy is measured
/// on a downscaled copy for speed, and the chosen offset is mapped back to full resolution.
fn smart_crop_offset(di: &DynamicImage, crop_w: u32, crop_h: u32) -> (u32, u32) {
    let src_w = di.width();
    let src_h = di.height();
    let slack_x = src_w.saturating_sub(crop_w);
    let slack_y = src_h.saturating_sub(crop_h);
    if slack_x == 0 && slack_y == 0 {
        return (0, 0);
    }

    let max_side = src_w.max(src_h);
    let target = 256u32;
    let (sw, sh) = if max_side <= target {
        (src_w, src_h)
    } else {
        let s = target as f32 / max_side as f32;
        (
            ((src_w as f32 * s).round() as u32).max(1),
            ((src_h as f32 * s).round() as u32).max(1),
        )
    };
    let small = if sw == src_w && sh == src_h {
        di.to_rgba8()
    } else {
        fir_resize(di.to_rgba8(), sw, sh, FirFilter::Bilinear)
    };
    let raw = small.as_raw();
    let swu = sw as usize;
    let shu = sh as usize;

    let luma: Vec<f32> = (0..swu * shu)
        .map(|i| {
            let p = &raw[i * 4..i * 4 + 3];
            0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32
        })
        .collect();

    // |dL/dx| + |dL/dy| with replicated borders (central difference).
    let energy_at = |x: usize, y: usize| -> f32 {
        let xm = x.saturating_sub(1);
        let xp = (x + 1).min(swu - 1);
        let ym = y.saturating_sub(1);
        let yp = (y + 1).min(shu - 1);
        let gx = (luma[y * swu + xp] - luma[y * swu + xm]).abs();
        let gy = (luma[yp * swu + x] - luma[ym * swu + x]).abs();
        gx + gy
    };

    if slack_x >= slack_y {
        // Horizontal slide; vertical centered.
        let mut col = vec![0f32; swu];
        for y in 0..shu {
            for (x, c) in col.iter_mut().enumerate() {
                *c += energy_at(x, y);
            }
        }
        let ratio = sw as f32 / src_w as f32;
        let win = ((crop_w as f32 * ratio).round() as usize).clamp(1, swu.saturating_sub(1).max(1));
        let start = best_window(&col, win);
        let x_full = ((start as f32 / ratio).round() as u32).min(slack_x);
        (x_full, slack_y / 2)
    } else {
        // Vertical slide; horizontal centered.
        let mut row = vec![0f32; shu];
        for (y, r) in row.iter_mut().enumerate() {
            let mut acc = 0f32;
            for x in 0..swu {
                acc += energy_at(x, y);
            }
            *r = acc;
        }
        let ratio = sh as f32 / src_h as f32;
        let win = ((crop_h as f32 * ratio).round() as usize).clamp(1, shu.saturating_sub(1).max(1));
        let start = best_window(&row, win);
        let y_full = ((start as f32 / ratio).round() as u32).min(slack_y);
        (slack_x / 2, y_full)
    }
}

pub struct ThumbnailProcess {
    width: u32,
    height: u32,
    smart: bool,
}

impl ThumbnailProcess {
    pub fn new(width: u32, height: u32) -> Self {
        ThumbnailProcess {
            width,
            height,
            smart: false,
        }
    }
    /// Content-aware variant: the crop window is placed over the highest-energy region
    /// (with a center bias) instead of being centered.
    pub fn new_smart(width: u32, height: u32) -> Self {
        ThumbnailProcess {
            width,
            height,
            smart: true,
        }
    }
}

impl Process for ThumbnailProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        if self.width == 0 || self.height == 0 {
            return Ok(img);
        }
        let src_w = img.di.width();
        let src_h = img.di.height();
        let dst_w = self.width;
        let dst_h = self.height;

        // Crop source to target aspect ratio first, then resize to exact target size.
        // Faster than resize-to-cover then crop: Lanczos3 works on dst_w×dst_h output
        // pixels instead of the slightly larger scaled intermediate.
        let scale = (dst_w as f64 / src_w as f64).max(dst_h as f64 / src_h as f64);
        let crop_w = (dst_w as f64 / scale).round() as u32;
        let crop_h = (dst_h as f64 / scale).round() as u32;
        let (crop_x, crop_y) = if self.smart {
            smart_crop_offset(&img.di, crop_w, crop_h)
        } else {
            (
                src_w.saturating_sub(crop_w) / 2,
                src_h.saturating_sub(crop_h) / 2,
            )
        };

        let source = if crop_w == src_w && crop_h == src_h {
            std::mem::take(&mut img.di)
        } else {
            DynamicImage::ImageRgba8(crop(&mut img.di, crop_x, crop_y, crop_w, crop_h).to_image())
        };
        img.di = DynamicImage::ImageRgba8(fir_resize(
            source.into_rgba8(),
            dst_w,
            dst_h,
            FirFilter::Lanczos3,
        ));
        img.buffer.clear();
        Ok(img)
    }
}

#[derive(Default)]
pub struct InvertProcess;

impl InvertProcess {
    pub fn new() -> Self {
        InvertProcess
    }
}

impl Process for InvertProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        let di = std::mem::take(&mut img.di);
        let mut rgba = di.into_rgba8();
        rgba.as_flat_samples_mut()
            .as_mut_slice()
            .par_chunks_mut(4)
            .for_each(|p| {
                p[0] = 255 - p[0];
                p[1] = 255 - p[1];
                p[2] = 255 - p[2];
            });
        img.di = DynamicImage::ImageRgba8(rgba);
        img.buffer.clear();
        Ok(img)
    }
}

pub struct OpacityProcess {
    factor: f32,
}

impl OpacityProcess {
    pub fn new(factor: f32) -> Self {
        OpacityProcess {
            factor: factor.clamp(0.0, 1.0),
        }
    }
}

impl Process for OpacityProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        let di = std::mem::take(&mut img.di);
        let mut rgba = di.into_rgba8();
        let factor = self.factor;
        rgba.as_flat_samples_mut()
            .as_mut_slice()
            .par_chunks_mut(4)
            .for_each(|p| {
                p[3] = (p[3] as f32 * factor).round() as u8;
            });
        img.di = DynamicImage::ImageRgba8(rgba);
        img.buffer.clear();
        Ok(img)
    }
}

pub struct GammaProcess {
    gamma: f32,
}

impl GammaProcess {
    pub fn new(gamma: f32) -> Self {
        GammaProcess {
            gamma: gamma.max(f32::EPSILON),
        }
    }
}

impl Process for GammaProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        let gamma = self.gamma;
        let mut lut = [0u8; 256];
        for (i, v) in lut.iter_mut().enumerate() {
            *v = ((i as f32 / 255.0).powf(gamma) * 255.0)
                .round()
                .clamp(0.0, 255.0) as u8;
        }
        let di = std::mem::take(&mut img.di);
        let mut rgba = di.into_rgba8();
        rgba.as_flat_samples_mut()
            .as_mut_slice()
            .par_chunks_mut(4)
            .for_each(|p| {
                p[0] = lut[p[0] as usize];
                p[1] = lut[p[1] as usize];
                p[2] = lut[p[2] as usize];
            });
        img.di = DynamicImage::ImageRgba8(rgba);
        img.buffer.clear();
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

/// Background process flattens transparency by compositing the image over a solid
/// background color (the standard "source over" operator with straight alpha).
pub struct BackgroundProcess {
    color: image::Rgba<u8>,
}

impl BackgroundProcess {
    /// `color` is a hex string (`#rrggbb` / `#rrggbbaa`); empty defaults to opaque white.
    pub fn new(color: &str) -> Self {
        let color = if color.is_empty() {
            image::Rgba([255, 255, 255, 255])
        } else {
            parse_hex_color(color)
        };
        BackgroundProcess { color }
    }
}

impl Process for BackgroundProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        let [bg_r, bg_g, bg_b, bg_a] = self.color.0;
        let bg_af = bg_a as f32 / 255.0;
        let di = std::mem::take(&mut img.di);
        let mut rgba = di.into_rgba8();
        rgba.as_flat_samples_mut()
            .as_mut_slice()
            .par_chunks_mut(4)
            .for_each(|p| {
                // Fully opaque source pixels are unchanged by compositing.
                if p[3] == 255 {
                    return;
                }
                let sa = p[3] as f32 / 255.0;
                let out_a = sa + bg_af * (1.0 - sa);
                if out_a <= f32::EPSILON {
                    p[0] = 0;
                    p[1] = 0;
                    p[2] = 0;
                    p[3] = 0;
                    return;
                }
                let blend = |s: u8, b: u8| -> u8 {
                    ((s as f32 * sa + b as f32 * bg_af * (1.0 - sa)) / out_a)
                        .round()
                        .clamp(0.0, 255.0) as u8
                };
                p[0] = blend(p[0], bg_r);
                p[1] = blend(p[1], bg_g);
                p[2] = blend(p[2], bg_b);
                p[3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
            });
        img.di = DynamicImage::ImageRgba8(rgba);
        img.buffer.clear();
        Ok(img)
    }
}

/// Build a 256-entry lookup table that linearly maps `[lo, hi]` onto `[0, 255]`.
/// A degenerate or inverted range yields an identity table (no-op).
fn build_stretch_lut(lo: f32, hi: f32) -> [u8; 256] {
    let mut lut = [0u8; 256];
    if hi <= lo {
        for (i, v) in lut.iter_mut().enumerate() {
            *v = i as u8;
        }
        return lut;
    }
    let scale = 255.0 / (hi - lo);
    for (i, v) in lut.iter_mut().enumerate() {
        *v = ((i as f32 - lo) * scale).round().clamp(0.0, 255.0) as u8;
    }
    lut
}

/// Normalize process stretches the histogram to the full 0..=255 range (auto-contrast).
pub struct NormalizeProcess {
    per_channel: bool,
}

impl NormalizeProcess {
    pub fn new(per_channel: bool) -> Self {
        NormalizeProcess { per_channel }
    }
}

impl Process for NormalizeProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        let di = std::mem::take(&mut img.di);
        let mut rgba = di.into_rgba8();

        // Pass 1: measure the input range from opaque pixels only, so transparent
        // padding (often RGB 0) doesn't drag the range down. Build per-channel LUTs.
        let luts: [[u8; 256]; 3] = if self.per_channel {
            let (mins, maxs) = rgba
                .as_raw()
                .par_chunks(4)
                .filter(|p| p[3] > 0)
                .fold(
                    || ([255u8; 3], [0u8; 3]),
                    |(mut mn, mut mx), p| {
                        for c in 0..3 {
                            mn[c] = mn[c].min(p[c]);
                            mx[c] = mx[c].max(p[c]);
                        }
                        (mn, mx)
                    },
                )
                .reduce(
                    || ([255u8; 3], [0u8; 3]),
                    |(mut amn, mut amx), (bmn, bmx)| {
                        for c in 0..3 {
                            amn[c] = amn[c].min(bmn[c]);
                            amx[c] = amx[c].max(bmx[c]);
                        }
                        (amn, amx)
                    },
                );
            [
                build_stretch_lut(mins[0] as f32, maxs[0] as f32),
                build_stretch_lut(mins[1] as f32, maxs[1] as f32),
                build_stretch_lut(mins[2] as f32, maxs[2] as f32),
            ]
        } else {
            let (lo, hi) = rgba
                .as_raw()
                .par_chunks(4)
                .filter(|p| p[3] > 0)
                .fold(
                    || (255.0f32, 0.0f32),
                    |(mn, mx), p| {
                        let l = 0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32;
                        (mn.min(l), mx.max(l))
                    },
                )
                .reduce(
                    || (255.0f32, 0.0f32),
                    |(amn, amx), (bmn, bmx)| (amn.min(bmn), amx.max(bmx)),
                );
            let lut = build_stretch_lut(lo, hi);
            [lut, lut, lut]
        };

        // Pass 2: apply the LUTs in parallel; alpha is untouched.
        rgba.as_flat_samples_mut()
            .as_mut_slice()
            .par_chunks_mut(4)
            .for_each(|p| {
                p[0] = luts[0][p[0] as usize];
                p[1] = luts[1][p[1] as usize];
                p[2] = luts[2][p[2] as usize];
            });
        img.di = DynamicImage::ImageRgba8(rgba);
        img.buffer.clear();
        Ok(img)
    }
}

/// Trim process auto-crops a uniform border matching the top-left reference pixel.
pub struct TrimProcess {
    tolerance: u8,
}

impl TrimProcess {
    pub fn new(tolerance: u8) -> Self {
        TrimProcess { tolerance }
    }
}

impl Process for TrimProcess {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage> {
        let mut img = pi;
        let (w, h) = (img.di.width(), img.di.height());
        if w == 0 || h == 0 {
            return Ok(img);
        }
        let di = std::mem::take(&mut img.di);
        let rgba = di.into_rgba8();
        let raw = rgba.as_raw();
        let reference = [raw[0], raw[1], raw[2], raw[3]];
        let tol = self.tolerance as i32;
        let wu = w as usize;

        let is_border = |px: &[u8]| -> bool {
            (px[0] as i32 - reference[0] as i32).abs() <= tol
                && (px[1] as i32 - reference[1] as i32).abs() <= tol
                && (px[2] as i32 - reference[2] as i32).abs() <= tol
                && (px[3] as i32 - reference[3] as i32).abs() <= tol
        };

        // For each row, the first/last x holding a non-border pixel (None = all border).
        let rows: Vec<Option<(u32, u32)>> = (0..h as usize)
            .into_par_iter()
            .map(|y| {
                let mut lo: Option<u32> = None;
                let mut hi = 0u32;
                for x in 0..wu {
                    let idx = (y * wu + x) * 4;
                    if !is_border(&raw[idx..idx + 4]) {
                        if lo.is_none() {
                            lo = Some(x as u32);
                        }
                        hi = x as u32;
                    }
                }
                lo.map(|l| (l, hi))
            })
            .collect();

        let Some(min_y) = rows.iter().position(|r| r.is_some()) else {
            // Entire image matches the border color: nothing to trim.
            img.di = DynamicImage::ImageRgba8(rgba);
            return Ok(img);
        };
        let max_y = rows.iter().rposition(|r| r.is_some()).unwrap();
        let min_x = rows.iter().filter_map(|r| r.map(|(l, _)| l)).min().unwrap();
        let max_x = rows
            .iter()
            .filter_map(|r| r.map(|(_, hx)| hx))
            .max()
            .unwrap();

        let crop_w = max_x - min_x + 1;
        let crop_h = (max_y - min_y) as u32 + 1;
        if crop_w == w && crop_h == h {
            img.di = DynamicImage::ImageRgba8(rgba);
            return Ok(img);
        }
        let mut di = DynamicImage::ImageRgba8(rgba);
        let cropped = crop(&mut di, min_x, min_y as u32, crop_w, crop_h).to_image();
        img.di = DynamicImage::ImageRgba8(cropped);
        img.buffer.clear();
        Ok(img)
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

        // Resolve the actual output format (unknown types fall back to JPEG).
        let actual_ext = if matches!(
            output_type.as_str(),
            IMAGE_TYPE_GIF | IMAGE_TYPE_PNG | IMAGE_TYPE_AVIF | IMAGE_TYPE_WEBP | IMAGE_TYPE_JXL
        ) {
            output_type
        } else {
            IMAGE_TYPE_JPEG.to_string()
        };
        img.ext.clone_from(&actual_ext);

        // GIF encoding reads the original buffer; clone it so img.buffer is preserved
        // for the fallback case where the optimised GIF is not smaller than the source.
        let gif_buffer = if actual_ext == IMAGE_TYPE_GIF {
            img.buffer.clone()
        } else {
            Vec::new()
        };

        // Closure returns (encoded_bytes, info.image) so the pixel data is available
        // for restoring img.di after encoding.
        let do_encode = move || -> Result<(Vec<u8>, RgbaImage)> {
            let encoded = match actual_ext.as_str() {
                IMAGE_TYPE_GIF => to_gif(Cursor::new(gif_buffer), speed).context(ImagesSnafu {})?,
                IMAGE_TYPE_PNG => info.to_png(quality).context(ImagesSnafu {})?,
                IMAGE_TYPE_AVIF => info.to_avif(quality, speed).context(ImagesSnafu {})?,
                IMAGE_TYPE_WEBP => info.to_webp(quality).context(ImagesSnafu {})?,
                IMAGE_TYPE_JXL => info.to_jxl(quality).context(ImagesSnafu {})?,
                _ => info.to_mozjpeg(quality).context(ImagesSnafu {})?,
            };
            Ok((encoded, info.image))
        };

        // In the CLI (multi-thread Tokio runtime) run encoding in a blocking context so
        // async I/O for other concurrent tasks can proceed during CPU-heavy encoding.
        #[cfg(feature = "bin")]
        let (data, info_image) = tokio::task::block_in_place(do_encode)?;
        #[cfg(not(feature = "bin"))]
        let (data, info_image) = do_encode()?;

        if img.ext != original_type || data.len() < original_size || original_size == 0 {
            img.buffer = data;
            // Re-decode the encoded buffer so subsequent diff comparison sees the actual
            // lossy output. Skip when original is None (no diff task in pipeline) since
            // the round-trip — especially for AVIF — is expensive and serves no purpose.
            if img.support_dssim() && img.original.is_some() {
                let result = if matches!(img.ext.as_str(), IMAGE_TYPE_AVIF | IMAGE_TYPE_JXL) {
                    if img.ext == IMAGE_TYPE_AVIF {
                        avif_decode(&img.buffer).context(ImagesSnafu {})
                    } else {
                        jxl_decode(&img.buffer).context(ImagesSnafu {})
                    }
                } else {
                    let c = Cursor::new(&img.buffer);
                    let format = ImageFormat::from_extension(&img.ext).unwrap_or(ImageFormat::Jpeg);
                    load(c, format).context(ImageSnafu {})
                };
                img.di = result.unwrap_or(DynamicImage::ImageRgba8(info_image));
            } else {
                img.di = DynamicImage::ImageRgba8(info_image);
            }
        } else {
            img.di = DynamicImage::ImageRgba8(info_image);
        }

        Ok(img)
    }
}

/// One encoded candidate produced during auto optimisation.
struct Candidate {
    ext: String,
    buffer: Vec<u8>,
    di: DynamicImage,
    diff: f64,
}

/// Auto optimisation: searches output format and/or quality to meet a perceptual-diff
/// target while minimising file size. Requires the original RGBA snapshot for scoring,
/// so the pipeline keeps the original whenever an auto task is present.
pub struct AutoOptimProcess {
    /// Empty → search across candidate formats; otherwise a fixed output format.
    output_type: String,
    /// None → binary-search quality; Some(q) → fixed quality.
    quality: Option<u8>,
    speed: u8,
    target_diff: f64,
}

impl AutoOptimProcess {
    pub fn new(output_type: &str, quality: Option<u8>, speed: u8, target_diff: f64) -> Self {
        Self {
            output_type: output_type.to_string(),
            quality,
            speed,
            target_diff,
        }
    }

    /// Candidate formats: a fixed one, or alpha-aware defaults when searching formats.
    fn candidate_formats(&self, info: &ImageInfo) -> Vec<String> {
        if !self.output_type.is_empty() && self.output_type != "auto" {
            return vec![self.output_type.clone()];
        }
        // Alpha-capable set vs photo set; WebP/AVIF lead since they usually win on size.
        if has_alpha(&info.image) {
            vec![
                IMAGE_TYPE_WEBP.to_string(),
                IMAGE_TYPE_AVIF.to_string(),
                IMAGE_TYPE_PNG.to_string(),
            ]
        } else {
            vec![
                IMAGE_TYPE_WEBP.to_string(),
                IMAGE_TYPE_AVIF.to_string(),
                IMAGE_TYPE_JPEG.to_string(),
            ]
        }
    }

    fn encode_candidate(
        &self,
        info: &ImageInfo,
        original: Option<&RgbaImage>,
        ext: &str,
        quality: u8,
    ) -> Result<Candidate> {
        let buffer = encode_info(info, ext, quality, self.speed)?;
        let di = decode_to_di(&buffer, ext)?;
        let diff = match original {
            Some(o) => dssim_score(o, &di),
            None => -1.0,
        };
        Ok(Candidate {
            ext: ext.to_string(),
            buffer,
            di,
            diff,
        })
    }

    /// Binary-search the lowest quality whose diff is within the target; falls back to the
    /// maximum quality when even that cannot meet it.
    fn search_quality(
        &self,
        info: &ImageInfo,
        original: &RgbaImage,
        ext: &str,
    ) -> Result<Candidate> {
        let mut lo = AUTO_MIN_QUALITY;
        let mut hi = AUTO_MAX_QUALITY;
        let mut best: Option<Candidate> = None;
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            let cand = self.encode_candidate(info, Some(original), ext, mid)?;
            if cand.diff >= 0.0 && cand.diff <= self.target_diff {
                // Meets the target — record it and try an even lower quality for a smaller file.
                best = Some(cand);
                if mid == AUTO_MIN_QUALITY {
                    break;
                }
                hi = mid - 1;
            } else {
                lo = mid + 1;
            }
        }
        match best {
            Some(c) => Ok(c),
            None => self.encode_candidate(info, Some(original), ext, AUTO_MAX_QUALITY),
        }
    }

    fn best_candidate(&self, info: &ImageInfo, original: Option<&RgbaImage>) -> Result<Candidate> {
        let formats = self.candidate_formats(info);
        let mut candidates = Vec::with_capacity(formats.len());
        for ext in &formats {
            let cand = match (self.quality, original) {
                (None, Some(orig)) => self.search_quality(info, orig, ext)?,
                // No original to score against: fall back to a single high-quality encode.
                (None, None) => self.encode_candidate(info, None, ext, AUTO_MAX_QUALITY)?,
                (Some(q), _) => self.encode_candidate(info, original, ext, q)?,
            };
            candidates.push(cand);
        }
        Ok(pick_candidate(candidates, self.target_diff))
    }
}

/// Choose the winning candidate: prefer those within the diff target (smallest size wins);
/// if none qualifies, keep the highest-fidelity result (lowest diff).
fn pick_candidate(mut candidates: Vec<Candidate>, target: f64) -> Candidate {
    let within: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| c.diff >= 0.0 && c.diff <= target)
        .map(|(i, _)| i)
        .collect();
    let idx = if let Some(&i) = within.iter().min_by_key(|&&i| candidates[i].buffer.len()) {
        i
    } else {
        (0..candidates.len())
            .min_by(|&a, &b| {
                candidates[a]
                    .diff
                    .partial_cmp(&candidates[b].diff)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            // candidate_formats always yields at least one format.
            .unwrap_or(0)
    };
    candidates.swap_remove(idx)
}

impl Process for AutoOptimProcess {
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

        let cand = {
            let original = img.original.as_ref();
            // Encoding/decoding/scoring is CPU-heavy; run it in a blocking context under the
            // CLI's multi-thread runtime so other async tasks keep progressing.
            #[cfg(feature = "bin")]
            let c = tokio::task::block_in_place(|| self.best_candidate(&info, original))?;
            #[cfg(not(feature = "bin"))]
            let c = self.best_candidate(&info, original)?;
            c
        };

        img.ext = cand.ext;
        img.buffer = cand.buffer;
        img.diff = cand.diff;
        img.di = cand.di;
        Ok(img)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AutoOptimProcess, BackgroundProcess, BlurProcess, BrightenProcess, ContrastProcess,
        CropProcess, FlipProcess, GammaProcess, GrayProcess, HueProcess, InvertProcess,
        LoaderProcess, NormalizeProcess, OpacityProcess, OptimProcess, PaddingProcess,
        ResizeProcess, RotateProcess, SaturateProcess, SharpenProcess, StripProcess,
        ThumbnailProcess, TrimProcess, WatermarkProcess,
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
    fn test_background_process() {
        // The rust logo has a transparent background; locate one transparent pixel.
        let orig = new_process_image();
        let orig_rgba = orig.di.as_rgba8().unwrap();
        let transparent = orig_rgba.pixels().find(|p| p.0[3] == 0).map(|p| p.0);

        let result =
            tokio_test::block_on(BackgroundProcess::new("#ff0000").process(new_process_image()))
                .unwrap();
        assert_eq!(result.di.width(), 144);
        assert_eq!(result.di.height(), 144);

        // Flattening onto an opaque background makes every pixel fully opaque.
        let rgba = result.di.as_rgba8().unwrap();
        assert!(rgba.pixels().all(|p| p.0[3] == 255));

        // A formerly transparent pixel takes the exact background color.
        if transparent.is_some() {
            let p = rgba.pixels().find(|p| p.0 == [255, 0, 0, 255]).map(|p| p.0);
            assert_eq!(p, Some([255, 0, 0, 255]));
        }

        // Empty color defaults to opaque white.
        let white =
            tokio_test::block_on(BackgroundProcess::new("").process(new_process_image())).unwrap();
        assert!(white.di.as_rgba8().unwrap().pixels().all(|p| p.0[3] == 255));
    }

    #[test]
    fn test_build_stretch_lut() {
        // Maps [50, 200] onto [0, 255].
        let lut = super::build_stretch_lut(50.0, 200.0);
        assert_eq!(lut[50], 0);
        assert_eq!(lut[200], 255);
        assert_eq!(lut[125], 128); // (125-50)*255/150 = 127.5 → 128
        assert_eq!(lut[0], 0); // clamps below
        assert_eq!(lut[255], 255); // clamps above
                                   // Degenerate range is an identity map (no-op).
        let id = super::build_stretch_lut(10.0, 10.0);
        assert_eq!(id[0], 0);
        assert_eq!(id[123], 123);
        assert_eq!(id[255], 255);
    }

    #[test]
    fn test_normalize_process() {
        // Dimensions are preserved in both modes.
        let rgb =
            tokio_test::block_on(NormalizeProcess::new(true).process(new_process_image())).unwrap();
        assert_eq!(rgb.di.width(), 144);
        assert_eq!(rgb.di.height(), 144);

        let luma = tokio_test::block_on(NormalizeProcess::new(false).process(new_process_image()))
            .unwrap();
        assert_eq!(luma.di.width(), 144);
        assert_eq!(luma.di.height(), 144);
    }

    #[test]
    fn test_trim_process() {
        // Pad the logo with a transparent border (200×200), then trim it back off.
        let padded = tokio_test::block_on(
            PaddingProcess::new(200, 200, "#00000000").process(new_process_image()),
        )
        .unwrap();
        assert_eq!(padded.di.width(), 200);
        assert_eq!(padded.di.height(), 200);

        let trimmed = tokio_test::block_on(TrimProcess::new(0).process(padded)).unwrap();
        // The transparent border is removed, shrinking the canvas below 200 on both axes.
        assert!(trimmed.di.width() < 200);
        assert!(trimmed.di.height() < 200);
        assert!(trimmed.di.width() > 0 && trimmed.di.height() > 0);
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
        // Verify the pipeline runs without error; pixel-level change depends on image content
        // (logo pixels at 0/255 extremes clamp back to original after unsharp mask).
        // The underlying gaussian kernel is validated by test_blur_process.
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

    fn new_red_pixel() -> ProcessImage {
        use image::{DynamicImage, Rgba, RgbaImage};
        let mut img = RgbaImage::new(1, 1);
        img.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
        let di = DynamicImage::ImageRgba8(img);
        ProcessImage {
            original: Some(di.to_rgba8()),
            di,
            diff: -1.0,
            original_size: 0,
            buffer: vec![],
            ext: "png".to_string(),
        }
    }

    #[test]
    fn test_hue_process() {
        // Dimensions are always preserved
        let result = tokio_test::block_on(HueProcess::new(0).process(new_process_image())).unwrap();
        assert_eq!(result.di.width(), 144);
        assert_eq!(result.di.height(), 144);

        // Pure red (H=0°) + 120° → pure green (H=120°)
        let result = tokio_test::block_on(HueProcess::new(120).process(new_red_pixel())).unwrap();
        let [r, g, b, a] = result.di.as_rgba8().unwrap().get_pixel(0, 0).0;
        assert!(r < 5, "R should be ~0, got {r}");
        assert!(g > 250, "G should be ~255, got {g}");
        assert!(b < 5, "B should be ~0, got {b}");
        assert_eq!(a, 255);

        // Pure red + 240° → pure blue (H=240°)
        let result = tokio_test::block_on(HueProcess::new(240).process(new_red_pixel())).unwrap();
        let [r, g, b, _] = result.di.as_rgba8().unwrap().get_pixel(0, 0).0;
        assert!(r < 5, "R should be ~0, got {r}");
        assert!(g < 5, "G should be ~0, got {g}");
        assert!(b > 250, "B should be ~255, got {b}");

        // shift=0 is a no-op
        let result = tokio_test::block_on(HueProcess::new(0).process(new_red_pixel())).unwrap();
        assert_eq!(
            result.di.as_rgba8().unwrap().get_pixel(0, 0).0,
            [255, 0, 0, 255]
        );

        // Alpha is preserved even on transparent pixels
        use image::{DynamicImage, Rgba, RgbaImage};
        let mut img = RgbaImage::new(1, 1);
        img.put_pixel(0, 0, Rgba([255, 0, 0, 128]));
        let di = DynamicImage::ImageRgba8(img);
        let pi = ProcessImage {
            original: Some(di.to_rgba8()),
            di,
            diff: -1.0,
            original_size: 0,
            buffer: vec![],
            ext: "png".to_string(),
        };
        let result = tokio_test::block_on(HueProcess::new(90).process(pi)).unwrap();
        assert_eq!(result.di.as_rgba8().unwrap().get_pixel(0, 0).0[3], 128);
    }

    #[test]
    fn test_saturate_process() {
        // Dimensions are always preserved
        let result =
            tokio_test::block_on(SaturateProcess::new(1.0).process(new_process_image())).unwrap();
        assert_eq!(result.di.width(), 144);
        assert_eq!(result.di.height(), 144);

        // factor=0.0: red → gray at its Rec.709 luma value (0.2126*255 ≈ 54, truncated)
        let result =
            tokio_test::block_on(SaturateProcess::new(0.0).process(new_red_pixel())).unwrap();
        let [r, g, b, a] = result.di.as_rgba8().unwrap().get_pixel(0, 0).0;
        assert_eq!([r, g, b], [54, 54, 54]);
        assert_eq!(a, 255);

        // factor=1.0: red stays red
        let result =
            tokio_test::block_on(SaturateProcess::new(1.0).process(new_red_pixel())).unwrap();
        let [r, g, b, _] = result.di.as_rgba8().unwrap().get_pixel(0, 0).0;
        assert!(r > 250 && g < 5 && b < 5);

        // factor > 1.0 on already-saturated color: clamps to 1.0, red stays red
        let result =
            tokio_test::block_on(SaturateProcess::new(2.0).process(new_red_pixel())).unwrap();
        let [r, g, b, _] = result.di.as_rgba8().unwrap().get_pixel(0, 0).0;
        assert!(r > 250 && g < 5 && b < 5);
    }

    #[test]
    fn test_thumbnail_process() {
        // Same aspect ratio: 144×144 → 72×72
        let result =
            tokio_test::block_on(ThumbnailProcess::new(72, 72).process(new_process_image()))
                .unwrap();
        assert_eq!(result.di.width(), 72);
        assert_eq!(result.di.height(), 72);

        // Different aspect ratio: 144×144 → 72×36
        let result =
            tokio_test::block_on(ThumbnailProcess::new(72, 36).process(new_process_image()))
                .unwrap();
        assert_eq!(result.di.width(), 72);
        assert_eq!(result.di.height(), 36);

        // Larger than source: scales up to cover
        let result =
            tokio_test::block_on(ThumbnailProcess::new(200, 100).process(new_process_image()))
                .unwrap();
        assert_eq!(result.di.width(), 200);
        assert_eq!(result.di.height(), 100);

        // Zero dimension: no-op
        let result =
            tokio_test::block_on(ThumbnailProcess::new(0, 72).process(new_process_image()))
                .unwrap();
        assert_eq!(result.di.width(), 144);
        assert_eq!(result.di.height(), 144);
    }

    #[test]
    fn test_smart_crop_offset() {
        use image::{DynamicImage, Rgba, RgbaImage};

        // 100×40, flat black except high-frequency stripes in x ∈ [70, 90).
        let mut img = RgbaImage::from_pixel(100, 40, Rgba([0, 0, 0, 255]));
        for y in 0..40 {
            for x in 70..90 {
                let v = if x % 2 == 0 { 255 } else { 0 };
                img.put_pixel(x, y, Rgba([v, v, v, 255]));
            }
        }
        let di = DynamicImage::ImageRgba8(img);

        // Crop 40×40: horizontal slack 60, vertical slack 0. The window should slide
        // right toward the stripes rather than centering (center offset would be 30).
        let (cx, cy) = super::smart_crop_offset(&di, 40, 40);
        assert_eq!(cy, 0);
        assert!(
            cx > 30,
            "expected the crop to favor the detailed right side, got {cx}"
        );

        // Flat image: no energy anywhere → fall back to the centered offset.
        let flat =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(100, 40, Rgba([10, 10, 10, 255])));
        assert_eq!(super::smart_crop_offset(&flat, 40, 40), (30, 0));

        // No slack on either axis → (0, 0).
        assert_eq!(super::smart_crop_offset(&di, 100, 40), (0, 0));
    }

    #[test]
    fn test_smart_thumbnail_process() {
        // Smart mode preserves the exact target dimensions, like the centered variant.
        let result =
            tokio_test::block_on(ThumbnailProcess::new_smart(144, 72).process(new_process_image()))
                .unwrap();
        assert_eq!(result.di.width(), 144);
        assert_eq!(result.di.height(), 72);
    }

    #[test]
    fn test_invert_process() {
        // Dimensions preserved
        let result =
            tokio_test::block_on(InvertProcess::new().process(new_process_image())).unwrap();
        assert_eq!(result.di.width(), 144);
        assert_eq!(result.di.height(), 144);

        // Inverting twice returns to original
        let once = tokio_test::block_on(InvertProcess::new().process(new_process_image())).unwrap();
        let twice = tokio_test::block_on(InvertProcess::new().process(once)).unwrap();
        let orig = new_process_image();
        assert_eq!(
            twice.di.as_rgba8().unwrap().get_pixel(72, 72).0,
            orig.di.as_rgba8().unwrap().get_pixel(72, 72).0
        );

        // Red [255,0,0,255] inverts RGB to [0,255,255,255]; alpha unchanged
        let result = tokio_test::block_on(InvertProcess::new().process(new_red_pixel())).unwrap();
        assert_eq!(
            result.di.as_rgba8().unwrap().get_pixel(0, 0).0,
            [0, 255, 255, 255]
        );
    }

    #[test]
    fn test_opacity_process() {
        // Dimensions preserved
        let result =
            tokio_test::block_on(OpacityProcess::new(1.0).process(new_process_image())).unwrap();
        assert_eq!(result.di.width(), 144);
        assert_eq!(result.di.height(), 144);

        // factor=1.0: alpha unchanged
        let result =
            tokio_test::block_on(OpacityProcess::new(1.0).process(new_red_pixel())).unwrap();
        assert_eq!(result.di.as_rgba8().unwrap().get_pixel(0, 0).0[3], 255);

        // factor=0.0: fully transparent
        let result =
            tokio_test::block_on(OpacityProcess::new(0.0).process(new_red_pixel())).unwrap();
        assert_eq!(result.di.as_rgba8().unwrap().get_pixel(0, 0).0[3], 0);

        // factor=0.5: alpha halved (255 * 0.5 = 127 or 128 depending on rounding)
        let result =
            tokio_test::block_on(OpacityProcess::new(0.5).process(new_red_pixel())).unwrap();
        let a = result.di.as_rgba8().unwrap().get_pixel(0, 0).0[3];
        assert!(a >= 127 && a <= 128);

        // RGB channels are unaffected
        let result =
            tokio_test::block_on(OpacityProcess::new(0.0).process(new_red_pixel())).unwrap();
        let [r, g, b, _] = result.di.as_rgba8().unwrap().get_pixel(0, 0).0;
        assert_eq!([r, g, b], [255, 0, 0]);
    }

    #[test]
    fn test_gamma_process() {
        // Dimensions preserved
        let result =
            tokio_test::block_on(GammaProcess::new(1.0).process(new_process_image())).unwrap();
        assert_eq!(result.di.width(), 144);
        assert_eq!(result.di.height(), 144);

        // gamma=1.0: each channel maps to itself (LUT is identity)
        let result = tokio_test::block_on(GammaProcess::new(1.0).process(new_red_pixel())).unwrap();
        assert_eq!(
            result.di.as_rgba8().unwrap().get_pixel(0, 0).0,
            [255, 0, 0, 255]
        );

        // gamma=2.0: darkens midtones; (128/255)^2 * 255 ≈ 64
        let mid = {
            use image::{DynamicImage, Rgba, RgbaImage};
            let mut img = RgbaImage::new(1, 1);
            img.put_pixel(0, 0, Rgba([128, 128, 128, 200]));
            let di = DynamicImage::ImageRgba8(img);
            ProcessImage {
                original: Some(di.to_rgba8()),
                di,
                diff: -1.0,
                original_size: 0,
                buffer: vec![],
                ext: "png".to_string(),
            }
        };
        let result = tokio_test::block_on(GammaProcess::new(2.0).process(mid)).unwrap();
        let [r, g, b, a] = result.di.as_rgba8().unwrap().get_pixel(0, 0).0;
        assert!(r < 70, "gamma=2.0 should darken, got {r}");
        assert_eq!(r, g);
        assert_eq!(g, b);
        assert_eq!(a, 200, "alpha must be unaffected");

        // gamma=0.5: brightens midtones; sqrt(128/255)*255 ≈ 181
        let mid2 = {
            use image::{DynamicImage, Rgba, RgbaImage};
            let mut img = RgbaImage::new(1, 1);
            img.put_pixel(0, 0, Rgba([128, 128, 128, 255]));
            let di = DynamicImage::ImageRgba8(img);
            ProcessImage {
                original: Some(di.to_rgba8()),
                di,
                diff: -1.0,
                original_size: 0,
                buffer: vec![],
                ext: "png".to_string(),
            }
        };
        let result = tokio_test::block_on(GammaProcess::new(0.5).process(mid2)).unwrap();
        let r = result.di.as_rgba8().unwrap().get_pixel(0, 0).0[0];
        assert!(r > 175, "gamma=0.5 should brighten, got {r}");
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

        // lossy jxl
        let result =
            tokio_test::block_on(OptimProcess::new("jxl", 80, 0).process(new_process_image()))
                .unwrap();
        assert_eq!(result.ext, "jxl");
        assert_ne!(result.buffer.len(), 0);
        assert!(result.get_diff() >= 0.0);

        // lossless jxl (alpha is dropped — lossless applies to RGB channels only)
        let result =
            tokio_test::block_on(OptimProcess::new("jxl", 100, 0).process(new_process_image()))
                .unwrap();
        assert_eq!(result.ext, "jxl");
        assert_ne!(result.buffer.len(), 0);
    }

    #[test]
    fn test_auto_quality_process() {
        // Fixed format (webp), search quality to stay within a loose target.
        let target = 10.0;
        let result = tokio_test::block_on(
            AutoOptimProcess::new("webp", None, 0, target).process(new_process_image()),
        )
        .unwrap();
        assert_eq!(result.ext, "webp");
        assert_ne!(result.buffer.len(), 0);
        // Search must honour the target: the chosen output is within it (and scored).
        assert!(result.diff >= 0.0);
        assert!(result.diff <= target);

        // A tighter target should never produce a smaller file than a looser one.
        let loose = tokio_test::block_on(
            AutoOptimProcess::new("webp", None, 0, 20.0).process(new_process_image()),
        )
        .unwrap();
        let tight = tokio_test::block_on(
            AutoOptimProcess::new("webp", None, 0, 0.2).process(new_process_image()),
        )
        .unwrap();
        assert!(tight.buffer.len() >= loose.buffer.len());
    }

    #[test]
    fn test_auto_format_process() {
        // Format search at a fixed quality; loose target so every candidate qualifies and
        // the smallest one wins. rust-logo has alpha → candidates are webp/avif/png.
        let result = tokio_test::block_on(
            AutoOptimProcess::new("", Some(80), 0, 1000.0).process(new_process_image()),
        )
        .unwrap();
        assert!(["webp", "avif", "png"].contains(&result.ext.as_str()));
        assert_ne!(result.buffer.len(), 0);
        assert!(result.diff >= 0.0);
    }

    #[test]
    fn test_auto_full_process() {
        // Search both format and quality.
        let target = 10.0;
        let result = tokio_test::block_on(
            AutoOptimProcess::new("", None, 0, target).process(new_process_image()),
        )
        .unwrap();
        assert!(["webp", "avif", "png"].contains(&result.ext.as_str()));
        assert!(result.diff >= 0.0);
        assert!(result.diff <= target);
    }

    #[test]
    fn test_pick_candidate() {
        use super::{pick_candidate, Candidate};
        let mk = |ext: &str, size: usize, diff: f64| Candidate {
            ext: ext.to_string(),
            buffer: vec![0u8; size],
            di: image::DynamicImage::default(),
            diff,
        };
        // Both within target → smallest size wins.
        let win = pick_candidate(vec![mk("webp", 100, 0.5), mk("avif", 60, 0.9)], 1.0);
        assert_eq!(win.ext, "avif");
        // None within target → lowest diff (best fidelity) wins.
        let win = pick_candidate(vec![mk("webp", 50, 5.0), mk("avif", 200, 2.0)], 1.0);
        assert_eq!(win.ext, "avif");
    }
}
