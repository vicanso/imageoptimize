use super::images::{
    avif_decode, avif_speed, gif_to_animated_webp, jpeg_to_jxl, jxl_decode, jxl_dimensions,
    strip_jxl_metadata, to_gif, ImageError, ImageInfo,
};
use base64::{engine::general_purpose, Engine as _};
use bytes::Bytes;
use dssim_core::{Dssim, DssimImage};
use exif::{In, Reader, Tag};
use fast_image_resize::images::{Image as FirImage, ImageRef as FirImageRef};
use fast_image_resize::{FilterType as FirFilter, PixelType, ResizeAlg, ResizeOptions, Resizer};
use image::imageops::overlay;
use image::metadata::Orientation;
use image::{load, DynamicImage, ImageDecoder, ImageFormat, ImageReader, RgbImage, RgbaImage};
use rayon::prelude::*;
use rgb::FromSlice;
use snafu::{ensure, ResultExt, Snafu};
use std::borrow::Cow;
use std::io::Cursor;
use std::sync::Arc;
#[cfg(feature = "network")]
use std::sync::OnceLock;
#[cfg(feature = "network")]
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
/// AVIF speed for quality-search probes. rav1e's fastest setting encodes ~5× faster than the
/// default yet scores within a few hundredths of DSSIM×1000 of slower settings at the same
/// quality, so it locates the target quality cheaply; the requested speed then only has to
/// confirm the boundary.
const AVIF_SEARCH_SPEED: u8 = 10;

#[cfg(feature = "network")]
static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

#[cfg(feature = "network")]
fn get_http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(reqwest::Client::new)
}

#[derive(Debug, Snafu)]
pub enum ImageProcessingError {
    #[snafu(display("Process image fail, message:{message}"))]
    ParamsInvalid { message: String },
    #[cfg(feature = "network")]
    #[snafu(display("{source}"))]
    Reqwest { source: reqwest::Error },
    #[cfg(feature = "network")]
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

/// Strip EXIF and XMP metadata (including GPS) from the encoded buffer without re-encoding.
/// Supports JPEG, PNG, WebP and JPEG XL. Other formats are returned unchanged.
pub fn new_strip_task() -> Vec<String> {
    vec![PROCESS_STRIP.to_string()]
}

/// Strip EXIF and XMP metadata — either can carry GPS coordinates — from raw image bytes
/// without re-encoding. `ext` is the format extension (`"jpeg"`, `"jpg"`, `"png"`, `"webp"`,
/// `"jxl"`). For JPEG XL the JPEG reconstruction data goes too (see `strip_jxl_metadata`).
/// Unsupported formats, and files carrying no such metadata, are returned unchanged.
pub fn strip_exif_bytes(data: Vec<u8>, ext: &str) -> Vec<u8> {
    let b = Bytes::from(data);
    let stripped: Option<Bytes> = match ext {
        "jpeg" | "jpg" => strip_jpeg_metadata(b.clone()),
        "png" => strip_png_metadata(b.clone()),
        "webp" => strip_webp_metadata(&b).map(Bytes::from),
        "jxl" => strip_jxl_metadata(&b).map(Bytes::from),
        _ => None,
    };
    stripped.unwrap_or(b).into()
}

/// Drop the APP1 segments holding EXIF, XMP or extended XMP. `None` when there are none.
fn strip_jpeg_metadata(data: Bytes) -> Option<Bytes> {
    use img_parts::jpeg::{markers, Jpeg};
    const PREFIXES: [&[u8]; 3] = [
        b"Exif\0\0",
        b"http://ns.adobe.com/xap/1.0/\0",
        b"http://ns.adobe.com/xmp/extension/\0",
    ];
    let mut img = Jpeg::from_bytes(data).ok()?;
    let count = img.segments().len();
    img.segments_mut().retain(|s| {
        s.marker() != markers::APP1 || !PREFIXES.iter().any(|p| s.contents().starts_with(p))
    });
    (img.segments().len() < count).then(|| img.encoder().bytes())
}

/// Drop the `eXIf` chunk and the text chunks holding XMP, or EXIF / XMP in ImageMagick's
/// "raw profile" form. Other text chunks are kept. `None` when there are none.
fn strip_png_metadata(data: Bytes) -> Option<Bytes> {
    const KEYWORDS: [&[u8]; 4] = [
        b"XML:com.adobe.xmp",
        b"Raw profile type exif",
        b"Raw profile type APP1",
        b"Raw profile type xmp",
    ];
    let mut img = img_parts::png::Png::from_bytes(data).ok()?;
    let count = img.chunks().len();
    img.chunks_mut().retain(|c| match &c.kind() {
        b"eXIf" => false,
        b"iTXt" | b"tEXt" | b"zTXt" => {
            // A text chunk starts with its NUL-terminated keyword.
            let keyword = c.contents().split(|&b| b == 0).next().unwrap_or_default();
            !KEYWORDS.contains(&keyword)
        }
        _ => true,
    });
    (img.chunks().len() < count).then(|| img.encoder().bytes())
}

/// Drop the `EXIF` and `XMP ` chunks of a WebP and clear their flags in the `VP8X` header.
/// The header itself stays: the alpha plane and animation frames of an extended file depend
/// on it. `None` when there are none.
fn strip_webp_metadata(data: &[u8]) -> Option<Vec<u8>> {
    const EXIF_FLAG: u8 = 0x08;
    const XMP_FLAG: u8 = 0x04;
    if data.len() < 12 || &data[..4] != b"RIFF" || &data[8..12] != b"WEBP" {
        return None;
    }
    let mut out = data[..12].to_vec();
    let mut rest = &data[12..];
    let mut removed = false;
    while rest.len() >= 8 {
        let size = u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
        // Chunks are padded to an even length.
        let end = 8usize
            .saturating_add(size)
            .saturating_add(size & 1)
            .min(rest.len());
        match &rest[..4] {
            b"EXIF" | b"XMP " => removed = true,
            id => {
                let start = out.len();
                out.extend_from_slice(&rest[..end]);
                if id == b"VP8X" && end > 8 {
                    out[start + 8] &= !(EXIF_FLAG | XMP_FLAG);
                }
            }
        }
        rest = &rest[end..];
    }
    if !removed {
        return None;
    }
    out.extend_from_slice(rest);
    let riff_size = u32::try_from(out.len() - 8).ok()?;
    out[4..8].copy_from_slice(&riff_size.to_le_bytes());
    Some(out)
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

/// Parse the optional positional param at `idx`: a missing or empty value yields `default`,
/// while a present but malformed value is an error rather than being silently replaced.
fn opt_param<T: std::str::FromStr>(params: &[String], idx: usize, default: T) -> Result<T> {
    match params.get(idx).map(|s| s.as_str()) {
        None | Some("") => Ok(default),
        Some(s) => s
            .parse::<T>()
            .map_err(|_| ImageProcessingError::ParamsInvalid {
                message: format!("invalid param '{s}'"),
            }),
    }
}

/// Parse the required integer param at `idx` (the caller checks it is present).
fn int_param<T>(params: &[String], idx: usize) -> Result<T>
where
    T: std::str::FromStr<Err = std::num::ParseIntError>,
{
    params[idx].parse::<T>().context(ParseIntSnafu {})
}

fn invalid<T>(message: String) -> Result<T> {
    ParamsInvalidSnafu { message }.fail()
}

/// Accept an output format `optim` can encode, or an empty one (keep the source format).
/// Anything else is rejected: it would otherwise be encoded as JPEG without a word.
fn check_output_type(output_type: &str, allow_gif: bool) -> Result<()> {
    match output_type {
        "" | "jpg" | IMAGE_TYPE_JPEG | IMAGE_TYPE_PNG | IMAGE_TYPE_WEBP | IMAGE_TYPE_AVIF
        | IMAGE_TYPE_JXL => Ok(()),
        IMAGE_TYPE_GIF if allow_gif => Ok(()),
        IMAGE_TYPE_GIF => invalid("gif has no quality setting to tune automatically".to_string()),
        other => invalid(format!(
            "unknown output format '{other}', expected jpeg, png, webp, avif, jxl or gif"
        )),
    }
}

/// Accept an empty color (the task's default) or `#rrggbb` / `#rrggbbaa`.
fn check_hex_color(color: &str) -> Result<()> {
    let hex = color.trim_start_matches('#');
    if color.is_empty()
        || (matches!(hex.len(), 6 | 8) && hex.chars().all(|c| c.is_ascii_hexdigit()))
    {
        Ok(())
    } else {
        invalid(format!(
            "invalid color '{color}', expected #rrggbb or #rrggbbaa"
        ))
    }
}

/// Largest encoded input `load` / `watermark` accept by default (HTTP bodies, files and
/// inline base64).
pub const DEFAULT_MAX_INPUT_BYTES: usize = 200 * 1024 * 1024;

/// Where `load` / `watermark` tasks may read image data from. The default allows HTTP(S),
/// `file://` and inline base64. Services that run user-supplied tasks should turn off
/// `allow_file` (arbitrary local file reads) and consider `allow_http` (requests to
/// internal hosts).
#[derive(Debug, Clone)]
pub struct LoadOptions {
    pub allow_http: bool,
    pub allow_file: bool,
    /// Maximum encoded input size in bytes.
    pub max_bytes: usize,
}

impl Default for LoadOptions {
    fn default() -> Self {
        LoadOptions {
            allow_http: true,
            allow_file: true,
            max_bytes: DEFAULT_MAX_INPUT_BYTES,
        }
    }
}

/// A validated pipeline step. [`run_with_image`] parses every string task into a `Task`
/// before running any of them, so a malformed step fails before an image is downloaded or
/// decoded. Use [`run_tasks`] to build a pipeline from typed tasks directly.
#[derive(Debug, Clone, PartialEq)]
pub enum Task {
    Load {
        data: String,
        ext: String,
    },
    Resize {
        width: u32,
        height: u32,
        fit: bool,
    },
    Gray,
    Flip {
        horizontal: bool,
    },
    Rotate {
        degrees: u16,
    },
    Brighten {
        value: i32,
    },
    Contrast {
        value: f32,
    },
    Sharpen {
        sigma: f32,
        threshold: i32,
    },
    Blur {
        sigma: f32,
    },
    Hue {
        shift: i32,
    },
    Saturate {
        factor: f32,
    },
    Thumbnail {
        width: u32,
        height: u32,
        smart: bool,
    },
    Invert,
    Opacity {
        factor: f32,
    },
    Gamma {
        gamma: f32,
    },
    Background {
        color: String,
    },
    Normalize {
        per_channel: bool,
    },
    Trim {
        tolerance: u8,
    },
    Strip,
    Padding {
        width: u32,
        height: u32,
        color: String,
    },
    Optim {
        output_type: String,
        quality: u8,
        speed: u8,
    },
    /// `output_type` empty = search formats; `quality` None = search quality.
    AutoOptim {
        output_type: String,
        quality: Option<u8>,
        speed: u8,
        target: f64,
    },
    Crop {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
    Watermark {
        url: String,
        position: WatermarkPosition,
        margin_left: i64,
        margin_top: i64,
    },
    Diff,
}

impl Task {
    /// Parse a string task such as `["resize", "100", "0"]`.
    pub fn parse(params: &[String]) -> Result<Task> {
        let he = ParamsInvalidSnafu {
            message: "params is invalid",
        };
        let Some((name, sub)) = params.split_first() else {
            return he.fail();
        };
        let task = match name.as_str() {
            PROCESS_LOAD => {
                ensure!(!sub.is_empty(), he);
                Task::Load {
                    data: sub[0].clone(),
                    ext: sub.get(1).cloned().unwrap_or_default(),
                }
            }
            PROCESS_RESIZE => {
                ensure!(sub.len() >= 2, he);
                Task::Resize {
                    width: int_param(sub, 0)?,
                    height: int_param(sub, 1)?,
                    fit: sub.get(2).is_some_and(|s| s == "fit"),
                }
            }
            PROCESS_GRAY => Task::Gray,
            PROCESS_FLIP => {
                let horizontal = match sub.first().map(|s| s.as_str()).unwrap_or("") {
                    "" | "h" | "horizontal" => true,
                    "v" | "vertical" => false,
                    other => {
                        return invalid(format!("flip direction must be h or v, got '{other}'"))
                    }
                };
                Task::Flip { horizontal }
            }
            PROCESS_ROTATE => {
                let degrees = opt_param::<u16>(sub, 0, 90)?;
                if degrees % 90 != 0 {
                    return invalid(format!(
                        "rotate degrees must be a multiple of 90, got {degrees}"
                    ));
                }
                Task::Rotate { degrees }
            }
            PROCESS_BRIGHTEN => Task::Brighten {
                value: opt_param(sub, 0, 0)?,
            },
            PROCESS_CONTRAST => Task::Contrast {
                value: opt_param(sub, 0, 0.0)?,
            },
            PROCESS_SHARPEN => Task::Sharpen {
                sigma: opt_param(sub, 0, 1.0)?,
                threshold: opt_param(sub, 1, 0)?,
            },
            PROCESS_BLUR => Task::Blur {
                sigma: opt_param(sub, 0, 1.0)?,
            },
            PROCESS_HUE => Task::Hue {
                shift: opt_param(sub, 0, 0)?,
            },
            PROCESS_SATURATE => Task::Saturate {
                factor: opt_param(sub, 0, 1.0)?,
            },
            PROCESS_THUMBNAIL => {
                ensure!(sub.len() >= 2, he);
                Task::Thumbnail {
                    width: int_param(sub, 0)?,
                    height: int_param(sub, 1)?,
                    smart: sub.get(2).is_some_and(|s| s == "smart"),
                }
            }
            PROCESS_INVERT => Task::Invert,
            PROCESS_OPACITY => Task::Opacity {
                factor: opt_param(sub, 0, 1.0)?,
            },
            PROCESS_GAMMA => Task::Gamma {
                gamma: opt_param(sub, 0, 1.0)?,
            },
            PROCESS_BACKGROUND => {
                let color = sub.first().cloned().unwrap_or_default();
                check_hex_color(&color)?;
                Task::Background { color }
            }
            PROCESS_NORMALIZE => {
                let per_channel = match sub.first().map(|s| s.as_str()).unwrap_or("") {
                    "" | "rgb" => true,
                    "luma" => false,
                    other => {
                        return invalid(format!(
                            "normalize mode must be rgb or luma, got '{other}'"
                        ))
                    }
                };
                Task::Normalize { per_channel }
            }
            PROCESS_TRIM => Task::Trim {
                tolerance: opt_param(sub, 0, 0)?,
            },
            PROCESS_STRIP => Task::Strip,
            PROCESS_PADDING => {
                ensure!(sub.len() >= 2, he);
                let color = sub.get(2).cloned().unwrap_or_default();
                check_hex_color(&color)?;
                Task::Padding {
                    width: int_param(sub, 0)?,
                    height: int_param(sub, 1)?,
                    color,
                }
            }
            PROCESS_OPTIM => {
                ensure!(sub.len() >= 3, he);
                let output_type = sub[0].to_ascii_lowercase();
                let quality_field = &sub[1];
                let speed = int_param::<u8>(sub, 2)?;
                let auto_format = output_type == "auto";
                let auto_quality = quality_field == "auto";
                if !auto_format {
                    // GIF has no quality to tune, so the auto modes can't target it.
                    check_output_type(&output_type, !auto_quality)?;
                }
                if auto_format || auto_quality {
                    Task::AutoOptim {
                        output_type: if auto_format {
                            String::new()
                        } else {
                            output_type
                        },
                        quality: if auto_quality {
                            None
                        } else {
                            Some(int_param(sub, 1)?)
                        },
                        speed,
                        // Optional 4th arg overrides the perceptual-diff target.
                        target: opt_param(sub, 3, AUTO_TARGET_DIFF)?,
                    }
                } else {
                    Task::Optim {
                        output_type,
                        quality: int_param(sub, 1)?,
                        speed,
                    }
                }
            }
            PROCESS_CROP => {
                ensure!(sub.len() >= 4, he);
                Task::Crop {
                    x: int_param(sub, 0)?,
                    y: int_param(sub, 1)?,
                    width: int_param(sub, 2)?,
                    height: int_param(sub, 3)?,
                }
            }
            PROCESS_WATERMARK => {
                ensure!(!sub.is_empty(), he);
                let url = decode(sub[0].as_str())
                    .context(FromUtfSnafu {})?
                    .to_string();
                let position = match sub.get(1).map(|s| s.as_str()).unwrap_or("") {
                    "" => WatermarkPosition::RightBottom,
                    name => match WatermarkPosition::from_name(name) {
                        Some(p) => p,
                        None => return invalid(format!("unknown watermark position '{name}'")),
                    },
                };
                Task::Watermark {
                    url,
                    position,
                    margin_left: if sub.len() > 2 { int_param(sub, 2)? } else { 0 },
                    margin_top: if sub.len() > 3 { int_param(sub, 3)? } else { 0 },
                }
            }
            PROCESS_DIFF => Task::Diff,
            // A misspelt task would otherwise be skipped without any sign it did nothing.
            other => return invalid(format!("unknown task '{other}'")),
        };
        Ok(task)
    }
}

pub async fn run_with_image(image: ProcessImage, tasks: Vec<Vec<String>>) -> Result<ProcessImage> {
    run_with_options(image, tasks, &LoadOptions::default()).await
}

/// Like [`run_with_image`], restricting where `load` / `watermark` tasks may read from.
/// Every task is parsed and validated before the first one runs.
pub async fn run_with_options(
    image: ProcessImage,
    tasks: Vec<Vec<String>>,
    options: &LoadOptions,
) -> Result<ProcessImage> {
    let tasks = tasks
        .iter()
        .filter(|t| !t.is_empty())
        .map(|t| Task::parse(t))
        .collect::<Result<Vec<_>>>()?;
    run_tasks(image, tasks, options).await
}

/// Run already-parsed tasks in order.
pub async fn run_tasks(
    mut image: ProcessImage,
    tasks: Vec<Task>,
    options: &LoadOptions,
) -> Result<ProcessImage> {
    // Only an explicit diff task needs the original RGBA snapshot; auto optimisation
    // scores candidates against its own encoder input.
    let needs_diff = tasks.iter().any(|t| matches!(t, Task::Diff));
    for task in tasks {
        image = match task {
            Task::Load { data, ext } => {
                let mut loader = LoaderProcess::new(&data, &ext);
                loader.keep_original = needs_diff;
                loader.options = options.clone();
                loader.process(image).await?
            }
            Task::Resize { width, height, fit } => {
                let proc = if fit {
                    ResizeProcess::new_fit(width, height)
                } else {
                    ResizeProcess::new(width, height)
                };
                proc.process(image).await?
            }
            Task::Gray => GrayProcess::new().process(image).await?,
            Task::Flip { horizontal } => {
                let direction = if horizontal { "h" } else { "v" };
                FlipProcess::new(direction).process(image).await?
            }
            Task::Rotate { degrees } => RotateProcess::new(degrees).process(image).await?,
            Task::Brighten { value } => BrightenProcess::new(value).process(image).await?,
            Task::Contrast { value } => ContrastProcess::new(value).process(image).await?,
            Task::Sharpen { sigma, threshold } => {
                SharpenProcess::new(sigma, threshold).process(image).await?
            }
            Task::Blur { sigma } => BlurProcess::new(sigma).process(image).await?,
            Task::Hue { shift } => HueProcess::new(shift).process(image).await?,
            Task::Saturate { factor } => SaturateProcess::new(factor).process(image).await?,
            Task::Thumbnail {
                width,
                height,
                smart,
            } => {
                let proc = if smart {
                    ThumbnailProcess::new_smart(width, height)
                } else {
                    ThumbnailProcess::new(width, height)
                };
                proc.process(image).await?
            }
            Task::Invert => InvertProcess::new().process(image).await?,
            Task::Opacity { factor } => OpacityProcess::new(factor).process(image).await?,
            Task::Gamma { gamma } => GammaProcess::new(gamma).process(image).await?,
            Task::Background { color } => BackgroundProcess::new(&color).process(image).await?,
            Task::Normalize { per_channel } => {
                NormalizeProcess::new(per_channel).process(image).await?
            }
            Task::Trim { tolerance } => TrimProcess::new(tolerance).process(image).await?,
            Task::Strip => StripProcess::new().process(image).await?,
            Task::Padding {
                width,
                height,
                color,
            } => {
                PaddingProcess::new(width, height, &color)
                    .process(image)
                    .await?
            }
            Task::Optim {
                output_type,
                quality,
                speed,
            } => {
                OptimProcess::new(&output_type, quality, speed)
                    .process(image)
                    .await?
            }
            Task::AutoOptim {
                output_type,
                quality,
                speed,
                target,
            } => {
                AutoOptimProcess::new(&output_type, quality, speed, target)
                    .process(image)
                    .await?
            }
            Task::Crop {
                x,
                y,
                width,
                height,
            } => CropProcess::new(x, y, width, height).process(image).await?,
            Task::Watermark {
                url,
                position,
                margin_left,
                margin_top,
            } => {
                let mut loader = LoaderProcess::new(&url, "");
                // The watermark itself is never diffed, so skip its RGBA snapshot.
                loader.keep_original = false;
                loader.options = options.clone();
                let watermark = loader.process(ProcessImage::default()).await?;
                WatermarkProcess::new(watermark.di, position, margin_left, margin_top)
                    .process(image)
                    .await?
            }
            Task::Diff => {
                image.diff = image.get_diff();
                image.original = None;
                image
            }
        };
    }
    Ok(image)
}

pub async fn run(tasks: Vec<Vec<String>>) -> Result<ProcessImage> {
    run_with_image(ProcessImage::default(), tasks).await
}

/// Read the EXIF orientation tag (1–8) from encoded image bytes; 1 (upright) when absent.
pub fn get_exif_orientation(data: &[u8]) -> u32 {
    Reader::new()
        .read_from_container(&mut Cursor::new(data))
        .ok()
        .and_then(|exif| exif.get_field(Tag::Orientation, In::PRIMARY).cloned())
        .and_then(|field| field.value.get_uint(0))
        .unwrap_or(1)
}

/// Apply an EXIF orientation (1–8). The channel layout is preserved — an opaque RGB8 photo
/// stays RGB8 — and flips / 180° turns happen in place without a copy.
fn apply_orientation(mut di: DynamicImage, orientation: u32) -> DynamicImage {
    if let Some(o) = u8::try_from(orientation)
        .ok()
        .and_then(Orientation::from_exif)
    {
        di.apply_orientation(o);
    }
    di
}

/// Largest decoded pixel buffer (as RGBA bytes) accepted from any input. Matches the `image`
/// crate's default allocation limit, and is also enforced for AVIF / JXL — whose decoders
/// don't apply it — so a tiny file declaring huge dimensions can't exhaust memory.
const MAX_DECODE_BYTES: u64 = 512 * 1024 * 1024;

fn ensure_decode_size(dimensions: Option<(u32, u32)>) -> Result<()> {
    if let Some((w, h)) = dimensions {
        let bytes = w as u64 * h as u64 * 4;
        if bytes > MAX_DECODE_BYTES {
            return invalid(format!(
                "image {w}x{h} exceeds the {MAX_DECODE_BYTES}-byte decode limit"
            ));
        }
    }
    Ok(())
}

fn avif_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let avif = avif_parse::read_avif(&mut &data[..]).ok()?;
    let meta = avif.primary_item_metadata().ok()?;
    Some((meta.max_frame_width.get(), meta.max_frame_height.get()))
}

/// Decode encoded bytes into pixels plus any embedded ICC profile, refusing images whose
/// decoded buffer would exceed [`MAX_DECODE_BYTES`] before allocating for them.
fn decode_image(data: &[u8], ext: &str) -> Result<(DynamicImage, Option<Vec<u8>>)> {
    let jxl_dims = jxl_dimensions(data);
    if ext == IMAGE_TYPE_JXL || jxl_dims.is_some() {
        ensure_decode_size(jxl_dims)?;
        return Ok((jxl_decode(data).context(ImagesSnafu {})?, None));
    }
    let format = image::guess_format(data).or_else(|_| {
        ImageFormat::from_extension(ext).ok_or(ImageProcessingError::ParamsInvalid {
            message: "Image format is not supported".to_string(),
        })
    })?;
    // `image`'s avif feature is encoder-only, so AVIF inputs must use the libaom-backed
    // decoder rather than image::load (which would error).
    if format == ImageFormat::Avif {
        ensure_decode_size(avif_dimensions(data))?;
        return Ok((avif_decode(data).context(ImagesSnafu {})?, None));
    }
    let mut decoder = ImageReader::with_format(Cursor::new(data), format)
        .into_decoder()
        .context(ImageSnafu {})?;
    // `into_decoder` skips the allocation check `ImageReader::decode` performs; redo it.
    image::Limits::default()
        .reserve(decoder.total_bytes())
        .context(ImageSnafu {})?;
    let icc = decoder.icc_profile().ok().flatten();
    let di = DynamicImage::from_decoder(decoder).context(ImageSnafu {})?;
    Ok((di, icc))
}

/// Convert pixels tagged with an embedded ICC profile (e.g. Display P3 from phone cameras)
/// to sRGB. None of the encoders carry the profile over, so without this, wide-gamut images
/// would display with dull, shifted colors. Profiles qcms can't apply to the pixel layout
/// (gray, CMYK, malformed) leave the image untouched.
fn icc_to_srgb(di: DynamicImage, icc: Option<&[u8]>) -> DynamicImage {
    let Some(input) = icc.and_then(|icc| qcms::Profile::new_from_slice(icc, false)) else {
        return di;
    };
    let mut output = qcms::Profile::new_sRGB();
    output.precache_output_transform();
    let has_alpha = di.color().has_alpha();
    let (ty, stride) = if has_alpha {
        (qcms::DataType::RGBA8, 4)
    } else {
        (qcms::DataType::RGB8, 3)
    };
    let Some(transform) = qcms::Transform::new(&input, &output, ty, qcms::Intent::default()) else {
        return di;
    };
    let mut di = match di {
        DynamicImage::ImageRgb8(_) | DynamicImage::ImageRgba8(_) => di,
        other if has_alpha => DynamicImage::ImageRgba8(other.into_rgba8()),
        other => DynamicImage::ImageRgb8(other.into_rgb8()),
    };
    let row = stride * di.width().max(1) as usize;
    let bytes = match &mut di {
        DynamicImage::ImageRgb8(img) => img.as_flat_samples_mut().samples,
        DynamicImage::ImageRgba8(img) => img.as_flat_samples_mut().samples,
        _ => unreachable!("normalised to RGB8 / RGBA8 above"),
    };
    // Transform whole rows in parallel bands.
    bytes
        .par_chunks_mut(row * 64)
        .for_each(|band| transform.apply(band));
    di
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

/// Layout-preserving SIMD resize: an RGB8 image stays RGB8 (no alpha channel
/// materialised), RGBA8 stays RGBA8 (alpha premultiplied during scaling), and any other
/// variant is normalised to RGBA8.
fn fir_resize_dynamic(di: DynamicImage, dst_w: u32, dst_h: u32, filter: FirFilter) -> DynamicImage {
    match di {
        DynamicImage::ImageRgb8(img) => {
            let (sw, sh) = (img.width(), img.height());
            let src = FirImage::from_vec_u8(sw, sh, img.into_raw(), PixelType::U8x3)
                .expect("rgb8 buffer matches U8x3 dimensions");
            let mut dst = FirImage::new(dst_w, dst_h, PixelType::U8x3);
            Resizer::new()
                .resize(
                    &src,
                    &mut dst,
                    &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(filter)),
                )
                .expect("source and destination are both U8x3");
            DynamicImage::ImageRgb8(
                RgbImage::from_raw(dst_w, dst_h, dst.into_vec())
                    .expect("fir output buffer matches dimensions"),
            )
        }
        DynamicImage::ImageRgba8(img) => {
            DynamicImage::ImageRgba8(fir_resize(img, dst_w, dst_h, filter))
        }
        other => DynamicImage::ImageRgba8(fir_resize(other.into_rgba8(), dst_w, dst_h, filter)),
    }
}

/// Resize `di` — or just the `(x, y, width, height)` region of it — reading its pixels in
/// place rather than taking an owned or cropped copy first. RGB8 stays RGB8 and RGBA8 stays
/// RGBA8 (alpha premultiplied during scaling); other variants are normalised first.
fn fir_resize_view(
    di: &DynamicImage,
    region: Option<(u32, u32, u32, u32)>,
    dst_w: u32,
    dst_h: u32,
    filter: FirFilter,
) -> DynamicImage {
    let normalised;
    let di = match di {
        DynamicImage::ImageRgb8(_) | DynamicImage::ImageRgba8(_) => di,
        other => {
            normalised = rgb_or_rgba(other.clone());
            &normalised
        }
    };
    let (bytes, stride) = pixel_bytes(di).expect("normalised to RGB8 or RGBA8 above");
    let pixel_type = if stride == 3 {
        PixelType::U8x3
    } else {
        PixelType::U8x4
    };
    let src = FirImageRef::new(di.width(), di.height(), bytes, pixel_type)
        .expect("buffer matches image dimensions");
    let mut dst = FirImage::new(dst_w, dst_h, pixel_type);
    let mut options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(filter));
    if let Some((x, y, w, h)) = region {
        options = options.crop(x as f64, y as f64, w as f64, h as f64);
    }
    Resizer::new()
        .resize(&src, &mut dst, &options)
        .expect("source and destination share a pixel type");
    let raw = dst.into_vec();
    if stride == 3 {
        DynamicImage::ImageRgb8(RgbImage::from_raw(dst_w, dst_h, raw).expect("rgb8 output size"))
    } else {
        DynamicImage::ImageRgba8(RgbaImage::from_raw(dst_w, dst_h, raw).expect("rgba8 output size"))
    }
}

/// Apply an in-place per-pixel RGB transform, preserving the image's channel layout:
/// RGB8 stays RGB8, RGBA8 keeps its (untouched) alpha, and other variants normalise to
/// RGBA8. The closure always receives the 3 RGB bytes, so fully opaque images avoid
/// carrying — and iterating over — a 4th channel.
fn map_rgb(di: DynamicImage, f: impl Fn(&mut [u8]) + Sync) -> DynamicImage {
    match di {
        DynamicImage::ImageRgb8(mut img) => {
            img.as_flat_samples_mut()
                .as_mut_slice()
                .par_chunks_mut(3)
                .for_each(&f);
            DynamicImage::ImageRgb8(img)
        }
        DynamicImage::ImageRgba8(mut img) => {
            img.as_flat_samples_mut()
                .as_mut_slice()
                .par_chunks_mut(4)
                .for_each(|p| f(&mut p[..3]));
            DynamicImage::ImageRgba8(img)
        }
        other => {
            let mut img = other.into_rgba8();
            img.as_flat_samples_mut()
                .as_mut_slice()
                .par_chunks_mut(4)
                .for_each(|p| f(&mut p[..3]));
            DynamicImage::ImageRgba8(img)
        }
    }
}

/// Borrow the raw pixel bytes and per-pixel stride (3 for RGB8, 4 for RGBA8) without
/// copying. Returns `None` for variants that are neither RGB8 nor RGBA8.
fn pixel_bytes(di: &DynamicImage) -> Option<(&[u8], usize)> {
    match di {
        DynamicImage::ImageRgb8(img) => Some((img.as_raw(), 3)),
        DynamicImage::ImageRgba8(img) => Some((img.as_raw(), 4)),
        _ => None,
    }
}

/// Per-channel (R, G, B) min/max over pixels that are not fully transparent. With `stride`
/// 3 (RGB) every pixel counts; with `stride` 4 (RGBA) pixels with alpha 0 are skipped.
fn channel_extents(bytes: &[u8], stride: usize) -> ([u8; 3], [u8; 3]) {
    bytes
        .par_chunks(stride)
        .filter(|p| stride < 4 || p[3] > 0)
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
        )
}

/// Luminance min/max over pixels that are not fully transparent (see [`channel_extents`]).
fn luma_extents(bytes: &[u8], stride: usize) -> (f32, f32) {
    bytes
        .par_chunks(stride)
        .filter(|p| stride < 4 || p[3] > 0)
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
        )
}

#[derive(Default, Clone)]
pub struct ProcessImage {
    /// Shared so cloning a decoded image per output target doesn't copy the snapshot.
    original: Option<Arc<RgbaImage>>,
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

    /// Like [`ProcessImage::new`] but without the original RGBA snapshot, saving a
    /// full-image copy and the lossy re-decode after encoding. A later `diff` task
    /// reports -1; auto optimisation is unaffected (it scores against its own input).
    pub fn new_without_original(data: Vec<u8>, ext: &str) -> Result<Self> {
        Self::new_impl(data, ext, false)
    }

    fn new_impl(data: Vec<u8>, ext: &str, keep_original: bool) -> Result<Self> {
        let (di, icc) = decode_image(&data, ext)?;
        let di = icc_to_srgb(di, icc.as_deref());
        let orientation = get_exif_orientation(&data);
        let di = apply_orientation(di, orientation);
        let original_size = data.len();
        // Clear buffer when orientation was corrected so get_buffer() re-encodes
        // from the oriented di rather than returning bytes with the stale EXIF tag.
        let buffer = if orientation == 1 { data } else { vec![] };
        Ok(ProcessImage {
            original_size,
            original: if keep_original {
                Some(Arc::new(di.to_rgba8()))
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
        if !self.buffer.is_empty() {
            return Ok(Cow::Borrowed(&self.buffer));
        }
        // The pixels changed since loading, so there are no encoded bytes: encode them in the
        // image's own format with default settings.
        if self.ext == IMAGE_TYPE_JXL {
            // Not an `image` format: it would otherwise fall through to JPEG below.
            const DEFAULT_JXL_QUALITY: u8 = 90;
            let info: ImageInfo = self.di.clone().into();
            let bytes = info.to_jxl(DEFAULT_JXL_QUALITY).context(ImagesSnafu {})?;
            return Ok(Cow::Owned(bytes));
        }
        let mut bytes: Vec<u8> = Vec::new();
        let format = ImageFormat::from_extension(&self.ext).unwrap_or(ImageFormat::Jpeg);
        self.di
            .write_to(&mut Cursor::new(&mut bytes), format)
            .context(ImageSnafu {})?;
        Ok(Cow::Owned(bytes))
    }
    pub fn get_size(&self) -> (u32, u32) {
        (self.di.width(), self.di.height())
    }
    /// Generate a Low-Quality Image Placeholder (LQIP): the image downscaled to `width`
    /// px (aspect preserved, never upscaled) and encoded as a tiny lossy WebP, returned as
    /// a self-contained `data:image/webp;base64,...` URI. Drop it into an `<img src>` or a
    /// CSS `background` for a blur-up placeholder while the full image loads — no JS needed.
    pub fn lqip_data_uri(&self, width: u32) -> Result<String> {
        // A low quality is intentional: the placeholder is shown scaled-up and blurred.
        const LQIP_QUALITY: u8 = 50;
        let (ow, oh) = (self.di.width(), self.di.height());
        let w = width.clamp(1, ow.max(1));
        let h = ((oh as u64 * w as u64) / ow.max(1) as u64).max(1) as u32;
        let info: ImageInfo = self.di.thumbnail_exact(w, h).into();
        let bytes = encode_info(&info, IMAGE_TYPE_WEBP, LQIP_QUALITY, 0)?;
        Ok(format!(
            "data:image/webp;base64,{}",
            general_purpose::STANDARD.encode(&bytes)
        ))
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

/// Holds the DSSIM context and the original's prepared image so a quality binary
/// search can score many candidates without re-preprocessing the original each time
/// (`create_image_rgba` builds multi-scale pyramids — the expensive half of a compare).
struct DiffScorer {
    attr: Dssim,
    /// `None` when dssim can't prepare the reference; every score is then -1.
    original: Option<DssimImage<f32>>,
    width: usize,
    height: usize,
}

impl DiffScorer {
    /// Prepare a scorer against the image an encoder is about to consume, so the score
    /// measures encoding loss only (not earlier resizes or color edits).
    fn from_dynamic(di: &DynamicImage) -> Self {
        match di {
            DynamicImage::ImageRgba8(img) => Self::new(img),
            other => Self::new(&other.to_rgba8()),
        }
    }

    fn new(original: &RgbaImage) -> Self {
        let width = original.width() as usize;
        let height = original.height() as usize;
        let attr = Dssim::new();
        let prepared = attr.create_image_rgba(original.as_raw().as_rgba(), width, height);
        DiffScorer {
            attr,
            original: prepared,
            width,
            height,
        }
    }

    /// DSSIM score (×1000) of a candidate against the prepared original.
    /// Returns -1.0 when the dimensions differ (not comparable).
    fn score(&self, di: &DynamicImage) -> f64 {
        let Some(original) = &self.original else {
            return -1.0;
        };
        // 如果宽高不一致，则不比对
        if self.width != di.width() as usize || self.height != di.height() as usize {
            return -1.0;
        }
        let tmp;
        let current_rgba = match di {
            DynamicImage::ImageRgba8(img) => img,
            other => {
                tmp = other.to_rgba8();
                &tmp
            }
        };
        let Some(gp2) =
            self.attr
                .create_image_rgba(current_rgba.as_raw().as_rgba(), self.width, self.height)
        else {
            return -1.0;
        };
        let (diff, _) = self.attr.compare(original, gp2);
        let value: f64 = diff.into();
        // 放大1千倍
        value * 1000.0
    }
}

/// Compute the DSSIM score (×1000) between an original RGBA snapshot and a candidate
/// image. Returns -1.0 when the dimensions differ (not comparable). For repeated
/// comparisons against the same original (quality search), build a `DiffScorer` once.
fn dssim_score(original: &RgbaImage, di: &DynamicImage) -> f64 {
    DiffScorer::new(original).score(di)
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

/// Run CPU-heavy work. Inside a multi-threaded Tokio runtime (the CLI) it goes through
/// `block_in_place` so other async tasks keep progressing; anywhere else — no runtime, or a
/// current-thread runtime, where `block_in_place` would panic — it simply runs inline.
#[cfg(feature = "bin")]
fn run_blocking<T>(f: impl FnOnce() -> T) -> T {
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

#[cfg(not(feature = "bin"))]
fn run_blocking<T>(f: impl FnOnce() -> T) -> T {
    f()
}

/// Pipeline step. The processors are CPU-bound; `async` only matters for loading over
/// HTTP. Library users on an async server should run a pipeline through their runtime's
/// blocking facility (e.g. `tokio::task::spawn_blocking` + `block_on`) so encoding doesn't
/// stall other tasks.
#[allow(async_fn_in_trait)]
pub trait Process {
    async fn process(&self, pi: ProcessImage) -> Result<ProcessImage>;
}

fn too_large<T>(max_bytes: usize) -> Result<T> {
    invalid(format!("image input exceeds the {max_bytes}-byte limit"))
}

/// Extension from a Content-Type value: `image/png; charset=binary` → `png`.
#[cfg(feature = "network")]
fn ext_from_content_type(content_type: &str) -> Option<String> {
    let mime = content_type.split(';').next()?.trim();
    let (_, subtype) = mime.split_once('/')?;
    Some(subtype.trim().to_ascii_lowercase())
}

/// Fetch image bytes over HTTP, returning the body and any extension inferred from
/// the Content-Type header. Error statuses fail instead of decoding the error page, and
/// the body is read incrementally so an oversized response stops at `max_bytes`.
#[cfg(feature = "network")]
async fn http_get(url: &str, max_bytes: usize) -> Result<(Vec<u8>, Option<String>)> {
    let mut resp = get_http_client()
        .get(url)
        .timeout(Duration::from_secs(5 * 60))
        .send()
        .await
        .context(ReqwestSnafu {})?
        .error_for_status()
        .context(ReqwestSnafu {})?;

    let ext = match resp.headers().get(reqwest::header::CONTENT_TYPE) {
        Some(value) => ext_from_content_type(value.to_str().context(HTTPHeaderToStrSnafu {})?),
        None => None,
    };
    if resp
        .content_length()
        .is_some_and(|len| len > max_bytes as u64)
    {
        return too_large(max_bytes);
    }
    let mut raw = Vec::new();
    while let Some(chunk) = resp.chunk().await.context(ReqwestSnafu {})? {
        if raw.len() + chunk.len() > max_bytes {
            return too_large(max_bytes);
        }
        raw.extend_from_slice(&chunk);
    }
    Ok((raw, ext))
}

/// Stub used when the `network` feature is disabled: HTTP URLs report a clear error.
#[cfg(not(feature = "network"))]
async fn http_get(_url: &str, _max_bytes: usize) -> Result<(Vec<u8>, Option<String>)> {
    Err(ImageProcessingError::ParamsInvalid {
        message: "HTTP image loading requires the `network` feature".to_string(),
    })
}

/// Loader process loads the image data from http, file or base64.
pub struct LoaderProcess {
    data: String,
    ext: String,
    pub keep_original: bool,
    /// Which sources are allowed and how large an input may be.
    pub options: LoadOptions,
}

impl LoaderProcess {
    pub fn new(data: &str, ext: &str) -> Self {
        LoaderProcess {
            data: data.to_string(),
            ext: ext.to_string(),
            keep_original: true,
            options: LoadOptions::default(),
        }
    }
    async fn fetch_data(&self) -> Result<ProcessImage> {
        let data = &self.data;
        let max_bytes = self.options.max_bytes;
        let mut ext = self.ext.clone();
        let from_http = data.starts_with("http://") || data.starts_with("https://");
        let original_data = if from_http {
            if !self.options.allow_http {
                return invalid("loading images over HTTP is disabled".to_string());
            }
            let (raw, detected_ext) = http_get(data, max_bytes).await?;
            if let Some(t) = detected_ext {
                ext = t;
            }
            raw
        } else if let Some(path) = data.strip_prefix("file://") {
            if !self.options.allow_file {
                return invalid("loading images from file:// is disabled".to_string());
            }
            let path = std::path::Path::new(path);
            if std::fs::metadata(path).context(IoSnafu)?.len() > max_bytes as u64 {
                return too_large(max_bytes);
            }
            ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            std::fs::read(path).context(IoSnafu)?
        } else {
            if data.len() / 4 * 3 > max_bytes {
                return too_large(max_bytes);
            }
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
        if src_w == 0 || src_h == 0 {
            return Ok(img);
        }

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
            // Extreme aspect ratios can round the short side to 0; keep at least 1px.
            (
                ((src_w as f64 * scale).round() as u32).max(1),
                ((src_h as f64 * scale).round() as u32).max(1),
            )
        } else {
            // u64 math: `src * target` overflows u32 for large images.
            let scaled = |src: u32, target: u32, base: u32| {
                ((src as u64 * target as u64 / base as u64).clamp(1, u32::MAX as u64)) as u32
            };
            let w = if self.width == 0 {
                scaled(src_w, self.height, src_h)
            } else {
                self.width
            };
            let h = if self.height == 0 {
                scaled(src_h, self.width, src_w)
            } else {
                self.height
            };
            (w, h)
        };

        img.di = fir_resize_dynamic(
            std::mem::take(&mut img.di),
            new_w,
            new_h,
            FirFilter::Lanczos3,
        );
        img.buffer.clear();
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
        img.di = map_rgb(std::mem::take(&mut img.di), |p| {
            let luma = (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32) as u8;
            p[0] = luma;
            p[1] = luma;
            p[2] = luma;
        });
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
        // In place, keeping the channel layout (no RGBA copy for opaque images).
        img.di.apply_orientation(if self.horizontal {
            Orientation::FlipHorizontal
        } else {
            Orientation::FlipVertical
        });
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
        let orientation = match self.degrees % 360 {
            90 => Orientation::Rotate90,
            180 => Orientation::Rotate180,
            270 => Orientation::Rotate270,
            _ => return Ok(img),
        };
        // Keeps the channel layout; 180° turns happen in place.
        img.di.apply_orientation(orientation);
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
        img.di = map_rgb(std::mem::take(&mut img.di), |p| {
            p[0] = (p[0] as i32 + value).clamp(0, 255) as u8;
            p[1] = (p[1] as i32 + value).clamp(0, 255) as u8;
            p[2] = (p[2] as i32 + value).clamp(0, 255) as u8;
        });
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
        img.di = map_rgb(std::mem::take(&mut img.di), |p| {
            p[0] = ((p[0] as f32 - 128.0) * factor + 128.0).clamp(0.0, 255.0) as u8;
            p[1] = ((p[1] as f32 - 128.0) * factor + 128.0).clamp(0.0, 255.0) as u8;
            p[2] = ((p[2] as f32 - 128.0) * factor + 128.0).clamp(0.0, 255.0) as u8;
        });
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
        let (sigma, threshold) = (self.sigma, self.threshold);
        img.di = map_raw(std::mem::take(&mut img.di), |bytes, w, h, stride| {
            parallel_unsharpen(bytes, w, h, stride, sigma, threshold)
        });
        img.buffer.clear();
        Ok(img)
    }
}

/// Normalise to RGB8 (opaque variants) or RGBA8 (variants with alpha), the two layouts the
/// raw-byte pixel loops handle.
fn rgb_or_rgba(di: DynamicImage) -> DynamicImage {
    match di {
        DynamicImage::ImageRgb8(_) | DynamicImage::ImageRgba8(_) => di,
        other if other.color().has_alpha() => DynamicImage::ImageRgba8(other.into_rgba8()),
        other => DynamicImage::ImageRgb8(other.into_rgb8()),
    }
}

/// Replace the pixels with `f(bytes, width, height, stride)`, keeping the channel layout:
/// opaque images are processed as 3-byte RGB, images with alpha as 4-byte RGBA.
fn map_raw(di: DynamicImage, f: impl FnOnce(&[u8], u32, u32, usize) -> Vec<u8>) -> DynamicImage {
    let di = rgb_or_rgba(di);
    let (w, h) = (di.width(), di.height());
    if w == 0 || h == 0 {
        return di;
    }
    match &di {
        DynamicImage::ImageRgb8(img) => DynamicImage::ImageRgb8(
            RgbImage::from_raw(w, h, f(img.as_raw(), w, h, 3)).expect("rgb8 output size"),
        ),
        DynamicImage::ImageRgba8(img) => DynamicImage::ImageRgba8(
            RgbaImage::from_raw(w, h, f(img.as_raw(), w, h, 4)).expect("rgba8 output size"),
        ),
        _ => unreachable!("normalised to RGB8 / RGBA8 above"),
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

fn convolve_rows(src: &[u8], dst: &mut [u8], w: u32, stride: usize, kernel: &[f32]) {
    let radius = (kernel.len() / 2) as i32;
    let row_len = w as usize * stride;
    dst.par_chunks_mut(row_len)
        .zip(src.par_chunks(row_len))
        .for_each(|(row, src_row)| {
            for x in 0..w as i32 {
                let mut acc = [0.0f32; 4];
                for (ki, &kv) in kernel.iter().enumerate() {
                    let sx = (x + ki as i32 - radius).clamp(0, w as i32 - 1) as usize;
                    let px = &src_row[sx * stride..sx * stride + stride];
                    for c in 0..stride {
                        acc[c] += px[c] as f32 * kv;
                    }
                }
                let di = x as usize * stride;
                for c in 0..stride {
                    row[di + c] = acc[c].round().clamp(0.0, 255.0) as u8;
                }
            }
        });
}

fn convolve_cols(src: &[u8], dst: &mut [u8], w: u32, h: u32, stride: usize, kernel: &[f32]) {
    let radius = (kernel.len() / 2) as i32;
    let row_len = w as usize * stride;
    dst.par_chunks_mut(row_len)
        .enumerate()
        .for_each(|(y, row)| {
            for x in 0..w as usize {
                let mut acc = [0.0f32; 4];
                for (ki, &kv) in kernel.iter().enumerate() {
                    let sy = (y as i32 + ki as i32 - radius).clamp(0, h as i32 - 1) as usize;
                    let idx = (sy * w as usize + x) * stride;
                    for c in 0..stride {
                        acc[c] += src[idx + c] as f32 * kv;
                    }
                }
                let di = x * stride;
                for c in 0..stride {
                    row[di + c] = acc[c].round().clamp(0.0, 255.0) as u8;
                }
            }
        });
}

/// Separable Gaussian blur over raw RGB (`stride` 3) or RGBA (`stride` 4) bytes.
fn parallel_blur(bytes: &[u8], w: u32, h: u32, stride: usize, sigma: f32) -> Vec<u8> {
    let kernel = gaussian_kernel_1d(sigma);
    let mut temp = vec![0u8; bytes.len()];
    let mut out = vec![0u8; bytes.len()];
    convolve_rows(bytes, &mut temp, w, stride, &kernel);
    convolve_cols(&temp, &mut out, w, h, stride, &kernel);
    out
}

/// Unsharp mask over raw RGB / RGBA bytes; alpha (when present) is copied unchanged.
fn parallel_unsharpen(
    bytes: &[u8],
    w: u32,
    h: u32,
    stride: usize,
    sigma: f32,
    threshold: i32,
) -> Vec<u8> {
    let blurred = parallel_blur(bytes, w, h, stride, sigma);
    let mut dst = vec![0u8; bytes.len()];
    dst.par_chunks_mut(stride)
        .zip(bytes.par_chunks(stride))
        .zip(blurred.par_chunks(stride))
        .for_each(|((dst, orig), blur)| {
            for i in 0..3 {
                let diff = orig[i] as i32 - blur[i] as i32;
                dst[i] = if diff.abs() >= threshold {
                    (orig[i] as i32 + diff).clamp(0, 255) as u8
                } else {
                    orig[i]
                };
            }
            if stride == 4 {
                dst[3] = orig[3];
            }
        });
    dst
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
        let sigma = self.sigma;
        img.di = map_raw(std::mem::take(&mut img.di), |bytes, w, h, stride| {
            parallel_blur(bytes, w, h, stride, sigma)
        });
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
        let shift = self.shift as f32;
        img.di = map_rgb(std::mem::take(&mut img.di), |p| {
            let (h, s, v) = rgb_to_hsv(p[0], p[1], p[2]);
            let (nr, ng, nb) = hsv_to_rgb(h + shift, s, v);
            p[0] = nr;
            p[1] = ng;
            p[2] = nb;
        });
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
        let factor = self.factor.max(0.0);
        img.di = map_rgb(std::mem::take(&mut img.di), |p| {
            let rf = p[0] as f32;
            let gf = p[1] as f32;
            let bf = p[2] as f32;
            let luma = 0.2126 * rf + 0.7152 * gf + 0.0722 * bf;
            p[0] = (luma + factor * (rf - luma)).clamp(0.0, 255.0) as u8;
            p[1] = (luma + factor * (gf - luma)).clamp(0.0, 255.0) as u8;
            p[2] = (luma + factor * (bf - luma)).clamp(0.0, 255.0) as u8;
        });
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
    // Measure on a small copy scaled straight from the borrowed pixels (no full-size copy).
    let small = fir_resize_view(di, None, sw, sh, FirFilter::Bilinear);
    let (raw, stride) = pixel_bytes(&small).expect("fir_resize_view yields RGB8 or RGBA8");
    let swu = sw as usize;
    let shu = sh as usize;

    let luma: Vec<f32> = raw
        .chunks_exact(stride)
        .map(|p| 0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32)
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
        if src_w == 0 || src_h == 0 {
            return Ok(img);
        }
        let dst_w = self.width;
        let dst_h = self.height;

        // Crop source to target aspect ratio first, then resize to exact target size.
        // Faster than resize-to-cover then crop: Lanczos3 works on dst_w×dst_h output
        // pixels instead of the slightly larger scaled intermediate.
        let scale = (dst_w as f64 / src_w as f64).max(dst_h as f64 / src_h as f64);
        // Extreme aspect-ratio changes can round the crop window to 0; keep at least 1px.
        let crop_w = ((dst_w as f64 / scale).round() as u32).clamp(1, src_w);
        let crop_h = ((dst_h as f64 / scale).round() as u32).clamp(1, src_h);
        let (crop_x, crop_y) = if self.smart {
            smart_crop_offset(&img.di, crop_w, crop_h)
        } else {
            (
                src_w.saturating_sub(crop_w) / 2,
                src_h.saturating_sub(crop_h) / 2,
            )
        };

        // Resize the crop window directly from the source pixels: no cropped copy, and the
        // channel layout is kept.
        let region =
            (crop_w != src_w || crop_h != src_h).then_some((crop_x, crop_y, crop_w, crop_h));
        img.di = fir_resize_view(&img.di, region, dst_w, dst_h, FirFilter::Lanczos3);
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
        img.di = map_rgb(std::mem::take(&mut img.di), |p| {
            p[0] = 255 - p[0];
            p[1] = 255 - p[1];
            p[2] = 255 - p[2];
        });
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
        img.di = map_rgb(std::mem::take(&mut img.di), |p| {
            p[0] = lut[p[0] as usize];
            p[1] = lut[p[1] as usize];
            p[2] = lut[p[2] as usize];
        });
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
        // Nothing to flatten in an image without an alpha channel.
        if !img.di.color().has_alpha() {
            return Ok(img);
        }
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
        // An opaque background leaves every pixel opaque: drop the now-useless alpha plane.
        img.di = if bg_a == 255 {
            DynamicImage::ImageRgb8(DynamicImage::ImageRgba8(rgba).into_rgb8())
        } else {
            DynamicImage::ImageRgba8(rgba)
        };
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
        let mut di = std::mem::take(&mut img.di);
        // Read directly from RGB8/RGBA8; rare other variants normalise to RGBA8 first.
        if pixel_bytes(&di).is_none() {
            di = DynamicImage::ImageRgba8(di.into_rgba8());
        }
        let (bytes, stride) = pixel_bytes(&di).expect("normalised to RGB8 or RGBA8 above");

        // Pass 1: measure the input range from opaque pixels only, so transparent
        // padding (often RGB 0) doesn't drag the range down. Build per-channel LUTs.
        let luts: [[u8; 256]; 3] = if self.per_channel {
            let (mins, maxs) = channel_extents(bytes, stride);
            [
                build_stretch_lut(mins[0] as f32, maxs[0] as f32),
                build_stretch_lut(mins[1] as f32, maxs[1] as f32),
                build_stretch_lut(mins[2] as f32, maxs[2] as f32),
            ]
        } else {
            let (lo, hi) = luma_extents(bytes, stride);
            let lut = build_stretch_lut(lo, hi);
            [lut, lut, lut]
        };

        // Pass 2: apply the LUTs in parallel, preserving the channel layout; alpha untouched.
        img.di = map_rgb(di, |p| {
            p[0] = luts[0][p[0] as usize];
            p[1] = luts[1][p[1] as usize];
            p[2] = luts[2][p[2] as usize];
        });
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
        // Scan the pixels in their own layout (RGB for opaque images) instead of an RGBA copy.
        let di = rgb_or_rgba(std::mem::take(&mut img.di));
        let (raw, stride) = pixel_bytes(&di).expect("normalised to RGB8 or RGBA8");
        let reference = &raw[..stride];
        let tol = self.tolerance as i32;
        let wu = w as usize;

        let is_border = |px: &[u8]| -> bool {
            px.iter()
                .zip(reference)
                .all(|(&p, &r)| (p as i32 - r as i32).abs() <= tol)
        };

        // For each row, the first/last x holding a non-border pixel (None = all border).
        let rows: Vec<Option<(u32, u32)>> = (0..h as usize)
            .into_par_iter()
            .map(|y| {
                let mut lo: Option<u32> = None;
                let mut hi = 0u32;
                for x in 0..wu {
                    let idx = (y * wu + x) * stride;
                    if !is_border(&raw[idx..idx + stride]) {
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
            img.di = di;
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
            img.di = di;
            return Ok(img);
        }
        img.di = di.crop_imm(min_x, min_y as u32, crop_w, crop_h);
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

        let x = ((dst_w - src_w) / 2) as i64;
        let y = ((dst_h - src_h) / 2) as i64;
        img.di = match &img.di {
            // Opaque image on an opaque fill: the result is opaque, so stay RGB8.
            DynamicImage::ImageRgb8(src) if self.color.0[3] == 255 => {
                let [r, g, b, _] = self.color.0;
                let mut canvas = RgbImage::from_pixel(dst_w, dst_h, image::Rgb([r, g, b]));
                overlay(&mut canvas, src, x, y);
                DynamicImage::ImageRgb8(canvas)
            }
            src => {
                let mut canvas = RgbaImage::from_pixel(dst_w, dst_h, self.color);
                overlay(&mut canvas, src, x, y);
                DynamicImage::ImageRgba8(canvas)
            }
        };
        img.buffer.clear();
        Ok(img)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

impl WatermarkPosition {
    /// Parse a position name (`leftTop`, `top`, … `rightBottom`); `None` if unknown.
    pub fn from_name(value: &str) -> Option<Self> {
        Some(match value {
            "leftTop" => WatermarkPosition::LeftTop,
            "top" => WatermarkPosition::Top,
            "rightTop" => WatermarkPosition::RightTop,
            "left" => WatermarkPosition::Left,
            "center" => WatermarkPosition::Center,
            "right" => WatermarkPosition::Right,
            "leftBottom" => WatermarkPosition::LeftBottom,
            "bottom" => WatermarkPosition::Bottom,
            "rightBottom" => WatermarkPosition::RightBottom,
            _ => return None,
        })
    }
}

impl From<&str> for WatermarkPosition {
    /// Unknown names fall back to `RightBottom`.
    fn from(value: &str) -> Self {
        WatermarkPosition::from_name(value).unwrap_or(WatermarkPosition::RightBottom)
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
        // `crop_imm` keeps the channel layout and clamps to the image bounds, so a region
        // outside the image yields an empty result.
        let result = img.di.crop_imm(self.x, self.y, self.width, self.height);
        ensure!(
            result.width() > 0 && result.height() > 0,
            ParamsInvalidSnafu {
                message: "crop region is outside the image",
            }
        );
        img.di = result;
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

        // Move img.di into ImageInfo without forcing RGBA: opaque images keep their
        // RGB8 layout so the encoders below can skip the alpha plane.
        let di = std::mem::take(&mut img.di);
        let info: ImageInfo = di.into();

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

        // An untouched GIF source still holds its original (possibly animated) bytes; after
        // any transform the buffer is cleared and only the current pixels in `info` remain.
        let gif_source = (original_type == IMAGE_TYPE_GIF && !img.buffer.is_empty())
            .then_some(img.buffer.as_slice());
        // Likewise an untouched JPEG: a lossless JXL request recompresses its DCT data
        // directly — smaller than the source and bit-exact — rather than re-encoding pixels,
        // which would come out larger than the JPEG.
        let jpeg_source = (quality >= 100 && img.buffer.starts_with(&[0xff, 0xd8, 0xff]))
            .then_some(img.buffer.as_slice());

        // Closure returns (encoded_bytes, info.image) so the pixel data is available
        // for restoring img.di after encoding.
        let do_encode = || -> Result<(Vec<u8>, DynamicImage)> {
            let encoded = match actual_ext.as_str() {
                IMAGE_TYPE_GIF => match gif_source {
                    Some(buf) => to_gif(Cursor::new(buf), speed),
                    None => info.to_gif(speed),
                }
                .context(ImagesSnafu {})?,
                IMAGE_TYPE_PNG => info.to_png(quality).context(ImagesSnafu {})?,
                IMAGE_TYPE_AVIF => info.to_avif(quality, speed).context(ImagesSnafu {})?,
                IMAGE_TYPE_WEBP => {
                    // Keep an animated GIF animated instead of flattening it to one frame.
                    let animated = match gif_source {
                        Some(buf) => gif_to_animated_webp(Cursor::new(buf), quality)
                            .context(ImagesSnafu {})?,
                        None => None,
                    };
                    match animated {
                        Some(data) => data,
                        None => info.to_webp(quality).context(ImagesSnafu {})?,
                    }
                }
                IMAGE_TYPE_JXL => {
                    // libjxl rejects a few JPEG flavours (e.g. arithmetic-coded); those fall
                    // back to the pixel encoder.
                    match jpeg_source.and_then(|buf| jpeg_to_jxl(buf).ok()) {
                        Some(data) => data,
                        None => info.to_jxl(quality).context(ImagesSnafu {})?,
                    }
                }
                _ => info.to_mozjpeg(quality).context(ImagesSnafu {})?,
            };
            Ok((encoded, info.image))
        };

        // CPU-heavy: under the CLI's multi-thread runtime this yields the worker thread so
        // async I/O for other concurrent tasks can proceed.
        let (data, info_image) = run_blocking(do_encode)?;

        if img.ext != original_type || data.len() < original_size || original_size == 0 {
            img.buffer = data;
            // Re-decode the encoded buffer so subsequent diff comparison sees the actual
            // lossy output. Skip when original is None (no diff task in pipeline) since
            // the round-trip — especially for AVIF — is expensive and serves no purpose.
            if img.support_dssim() && img.original.is_some() {
                img.di = decode_to_di(&img.buffer, &img.ext).unwrap_or(info_image);
            } else {
                img.di = info_image;
            }
        } else {
            img.di = info_image;
        }

        Ok(img)
    }
}

/// One encoded candidate produced during auto optimisation.
struct Candidate {
    ext: String,
    quality: u8,
    buffer: Vec<u8>,
    di: DynamicImage,
    diff: f64,
}

/// Auto optimisation: searches output format and/or quality to meet a perceptual-diff
/// target while minimising file size. Candidates are scored against the image being
/// encoded (not the loaded original), so the target bounds encoding loss alone and the
/// search still works after a resize, crop or color edit.
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
        if info.has_alpha() {
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
        scorer: &DiffScorer,
        ext: &str,
        quality: u8,
        speed: u8,
    ) -> Result<Candidate> {
        let buffer = encode_info(info, ext, quality, speed)?;
        let di = decode_to_di(&buffer, ext)?;
        let diff = scorer.score(&di);
        Ok(Candidate {
            ext: ext.to_string(),
            quality,
            buffer,
            di,
            diff,
        })
    }

    /// Search the lowest quality whose diff is within the target; falls back to the maximum
    /// quality when even that cannot meet it.
    fn search_quality(
        &self,
        info: &ImageInfo,
        scorer: &DiffScorer,
        ext: &str,
    ) -> Result<Candidate> {
        let target = self.target_diff;
        let mut probe = |q| self.encode_candidate(info, scorer, ext, q, self.speed);
        // Slow AVIF encodes dominate the search: locate the quality with fast-preset probes,
        // then pin down the exact boundary at the requested speed starting from there.
        if ext == IMAGE_TYPE_AVIF && avif_speed(self.speed) < AVIF_SEARCH_SPEED {
            let mut fast = |q| self.encode_candidate(info, scorer, ext, q, AVIF_SEARCH_SPEED);
            let guess = bisect_quality(&mut fast, target, AUTO_MIN_QUALITY - 1, None)?;
            return gallop_quality(&mut probe, target, guess.quality);
        }
        bisect_quality(&mut probe, target, AUTO_MIN_QUALITY - 1, None)
    }

    fn best_candidate(&self, info: &ImageInfo) -> Result<Candidate> {
        let formats = self.candidate_formats(info);
        // Prepare the reference DSSIM pyramids once and reuse them across every format and
        // every quality probe (instead of re-preprocessing on each comparison).
        let scorer = DiffScorer::from_dynamic(&info.image);
        // Formats are independent searches, so run them concurrently (order is preserved).
        let candidates = formats
            .par_iter()
            .map(|ext| match self.quality {
                None => self.search_quality(info, &scorer, ext),
                Some(q) => self.encode_candidate(info, &scorer, ext, q, self.speed),
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(pick_candidate(candidates, self.target_diff))
    }
}

fn meets_target(candidate: &Candidate, target: f64) -> bool {
    candidate.diff >= 0.0 && candidate.diff <= target
}

/// Bisect for the lowest quality meeting `target`, between `fail` (a quality known to miss
/// it, or `AUTO_MIN_QUALITY - 1` when untested) and `pass` (a candidate known to meet it, or
/// `None` for the untested top of the range). Assumes the diff falls as quality rises. When
/// nothing meets the target, the `AUTO_MAX_QUALITY` probe is returned.
fn bisect_quality(
    probe: &mut impl FnMut(u8) -> Result<Candidate>,
    target: f64,
    mut fail: u8,
    mut pass: Option<Candidate>,
) -> Result<Candidate> {
    // Climbing all the way up probes the maximum quality; keep it rather than encode it twice.
    let mut max_probe = None;
    loop {
        let hi = pass.as_ref().map_or(AUTO_MAX_QUALITY + 1, |c| c.quality);
        if hi - fail <= 1 {
            break;
        }
        let mid = fail + (hi - fail) / 2;
        let cand = probe(mid)?;
        if meets_target(&cand, target) {
            pass = Some(cand);
        } else {
            if mid == AUTO_MAX_QUALITY {
                max_probe = Some(cand);
            }
            fail = mid;
        }
    }
    match pass.or(max_probe) {
        Some(c) => Ok(c),
        None => probe(AUTO_MAX_QUALITY),
    }
}

/// Like [`bisect_quality`] over the whole range, but starting from a predicted quality: probe
/// it, stride away in doubling steps until the target boundary is bracketed, then bisect the
/// bracket. An accurate prediction settles in two or three probes instead of ~7.
fn gallop_quality(
    probe: &mut impl FnMut(u8) -> Result<Candidate>,
    target: f64,
    start: u8,
) -> Result<Candidate> {
    let first = probe(start.clamp(AUTO_MIN_QUALITY, AUTO_MAX_QUALITY))?;
    let mut stride = 1u8;
    if meets_target(&first, target) {
        // Step down until a quality misses the target.
        let mut pass = first;
        while pass.quality > AUTO_MIN_QUALITY {
            let q = pass.quality.saturating_sub(stride).max(AUTO_MIN_QUALITY);
            let cand = probe(q)?;
            if !meets_target(&cand, target) {
                return bisect_quality(probe, target, q, Some(pass));
            }
            pass = cand;
            stride = stride.saturating_mul(2);
        }
        Ok(pass)
    } else {
        // Step up until one meets it.
        let mut fail = first;
        while fail.quality < AUTO_MAX_QUALITY {
            let q = fail.quality.saturating_add(stride).min(AUTO_MAX_QUALITY);
            let cand = probe(q)?;
            if meets_target(&cand, target) {
                return bisect_quality(probe, target, fail.quality, Some(cand));
            }
            fail = cand;
            stride = stride.saturating_mul(2);
        }
        // Even the maximum quality misses the target: keep that probe.
        Ok(fail)
    }
}

/// Choose the winning candidate: prefer those within the diff target (smallest size wins);
/// if none qualifies, keep the highest-fidelity result (lowest diff).
fn pick_candidate(mut candidates: Vec<Candidate>, target: f64) -> Candidate {
    let within: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| meets_target(c, target))
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

        // Move img.di into ImageInfo without forcing RGBA: opaque images keep their
        // RGB8 layout so format-candidate encoding can skip the alpha plane.
        let di = std::mem::take(&mut img.di);
        let info: ImageInfo = di.into();

        // Encoding/decoding/scoring is CPU-heavy; see `run_blocking`.
        let cand = run_blocking(|| self.best_candidate(&info))?;

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
    fn test_lqip_data_uri() {
        use base64::Engine as _;
        let pi = new_process_image(); // 144x144 source
        let uri = pi.lqip_data_uri(24).unwrap();
        assert!(uri.starts_with("data:image/webp;base64,"));
        let b64 = uri.strip_prefix("data:image/webp;base64,").unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        // RIFF....WEBP container header
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WEBP");
        // Downscaled to the requested width (aspect preserved) and kept tiny.
        let decoded = super::decode_to_di(&bytes, super::IMAGE_TYPE_WEBP).unwrap();
        assert_eq!(decoded.width(), 24);
        assert_eq!(decoded.height(), 24);
        assert!(
            bytes.len() < 2000,
            "LQIP should be tiny, got {}",
            bytes.len()
        );
    }

    /// Hits a public URL, so it is opt-in: `cargo test -- --ignored`.
    #[test]
    #[ignore = "requires network access"]
    #[cfg(feature = "network")]
    fn test_load_process_http() {
        let p = LoaderProcess::new(
            "https://www.baidu.com/img/PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png",
            "",
        );
        let result = tokio_test::block_on(p.fetch_data()).unwrap();
        assert_ne!(result.buffer.len(), 0);
        assert_eq!(result.ext, "png");
    }

    #[test]
    #[cfg(feature = "network")]
    fn test_ext_from_content_type() {
        use super::ext_from_content_type;
        assert_eq!(ext_from_content_type("image/png").as_deref(), Some("png"));
        assert_eq!(
            ext_from_content_type("image/PNG; charset=binary").as_deref(),
            Some("png")
        );
        assert_eq!(ext_from_content_type("nonsense"), None);
    }

    #[test]
    fn test_load_options() {
        use super::LoadOptions;
        let file = format!(
            "file://{}/assets/rust-logo.png",
            std::env::current_dir().unwrap().to_string_lossy()
        );
        let mut p = LoaderProcess::new(&file, "");
        p.options = LoadOptions {
            allow_file: false,
            ..Default::default()
        };
        assert!(tokio_test::block_on(p.fetch_data()).is_err());

        p.options = LoadOptions {
            max_bytes: 100,
            ..Default::default()
        };
        assert!(tokio_test::block_on(p.fetch_data()).is_err());

        let data = include_bytes!("../assets/rust-logo.png");
        let mut p = LoaderProcess::new(&general_purpose::STANDARD.encode(data), "png");
        p.options.max_bytes = 100;
        assert!(tokio_test::block_on(p.fetch_data()).is_err());

        let mut p = LoaderProcess::new("http://127.0.0.1:1/a.png", "");
        p.options.allow_http = false;
        let err = tokio_test::block_on(p.fetch_data()).err().unwrap();
        assert!(err.to_string().contains("disabled"), "{err}");
    }

    #[test]
    fn test_load_process() {
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

        // Flattening onto an opaque background makes every pixel opaque, so the alpha
        // plane is dropped entirely.
        assert!(matches!(result.di, image::DynamicImage::ImageRgb8(_)));
        let rgba = result.di.to_rgba8();
        assert!(rgba.pixels().all(|p| p.0[3] == 255));

        // A formerly transparent pixel takes the exact background color.
        if transparent.is_some() {
            let p = rgba.pixels().find(|p| p.0 == [255, 0, 0, 255]).map(|p| p.0);
            assert_eq!(p, Some([255, 0, 0, 255]));
        }

        // Empty color defaults to opaque white.
        let white =
            tokio_test::block_on(BackgroundProcess::new("").process(new_process_image())).unwrap();
        assert!(white.di.to_rgba8().pixels().all(|p| p.0[3] == 255));

        // A semi-transparent background keeps the alpha channel.
        let tinted =
            tokio_test::block_on(BackgroundProcess::new("#ff000080").process(new_process_image()))
                .unwrap();
        assert!(matches!(tinted.di, image::DynamicImage::ImageRgba8(_)));
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
    #[cfg(feature = "jxl")]
    fn test_optim_jpeg_to_jxl() {
        let info: crate::ImageInfo = new_process_image().di.into();
        let jpeg = info.to_mozjpeg(90).unwrap();
        let jpeg_image = || ProcessImage::new(jpeg.clone(), "jpeg").unwrap();
        // libjxl's JPEG reconstruction box marks a lossless recompression.
        let transcoded = |buf: &[u8]| buf.windows(4).any(|w| w == b"jbrd");

        // Lossless request on an untouched JPEG: its DCT data is recompressed directly.
        let out =
            tokio_test::block_on(OptimProcess::new("jxl", 100, 0).process(jpeg_image())).unwrap();
        assert_eq!(out.ext, "jxl");
        assert!(transcoded(&out.buffer));
        assert_eq!(out.get_size(), (144, 144));
        assert!(out.get_diff() >= 0.0);

        // Stripping metadata drops the reconstruction data along with the EXIF.
        let stripped = tokio_test::block_on(StripProcess::new().process(out)).unwrap();
        assert!(!transcoded(&stripped.buffer));
        assert!(crate::jxl_decode(&stripped.buffer).is_ok());

        // Lossy quality: the pixel encoder.
        let out =
            tokio_test::block_on(OptimProcess::new("jxl", 80, 0).process(jpeg_image())).unwrap();
        assert!(!out.buffer.is_empty() && !transcoded(&out.buffer));

        // After a pixel edit the JPEG bytes are stale: the pixel encoder again.
        let resized =
            tokio_test::block_on(ResizeProcess::new(72, 0).process(jpeg_image())).unwrap();
        let out = tokio_test::block_on(OptimProcess::new("jxl", 100, 0).process(resized)).unwrap();
        assert!(!transcoded(&out.buffer));
        assert_eq!(out.get_size(), (72, 72));
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

    /// A minimal EXIF payload: little-endian TIFF header with an empty IFD.
    const TEST_EXIF: &[u8] = b"II*\0\x08\0\0\0\0\0\0\0\0\0";
    const TEST_XMP: &[u8] = b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>";

    #[test]
    fn test_strip_jpeg_exif_and_xmp() {
        use crate::image_processing::strip_exif_bytes;
        use img_parts::jpeg::{markers, Jpeg, JpegSegment};
        use img_parts::{Bytes, ImageEXIF};

        let info: crate::ImageInfo = new_process_image().di.into();
        let mut jpeg = Jpeg::from_bytes(info.to_mozjpeg(90).unwrap().into()).unwrap();
        jpeg.set_exif(Some(Bytes::from_static(TEST_EXIF)));
        let xmp = [b"http://ns.adobe.com/xap/1.0/\0".as_slice(), TEST_XMP].concat();
        jpeg.segments_mut()
            .insert(1, JpegSegment::new_with_contents(markers::APP1, xmp.into()));
        let tagged = jpeg.encoder().bytes().to_vec();

        let stripped = strip_exif_bytes(tagged.clone(), "jpeg");
        assert!(stripped.len() < tagged.len());
        let parts = Jpeg::from_bytes(stripped.clone().into()).unwrap();
        assert!(parts.exif().is_none());
        assert!(parts.segments().iter().all(|s| s.marker() != markers::APP1));
        let decoded = image::load_from_memory(&stripped).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (144, 144));
        // Nothing left to strip: the bytes come back untouched.
        assert_eq!(strip_exif_bytes(stripped.clone(), "jpg"), stripped);
    }

    #[test]
    fn test_strip_png_exif_and_xmp() {
        use crate::image_processing::strip_exif_bytes;
        use img_parts::png::{Png, PngChunk};
        use img_parts::Bytes;

        let data = include_bytes!("../assets/rust-logo.png");
        let mut png = Png::from_bytes(Bytes::from_static(data)).unwrap();
        let text = |keyword: &str, body: &[u8]| [keyword.as_bytes(), b"\0", body].concat();
        for (kind, contents) in [
            (*b"eXIf", TEST_EXIF.to_vec()),
            // iTXt: keyword, NUL, compression flag + method, empty language and translation.
            (
                *b"iTXt",
                text("XML:com.adobe.xmp", &[b"\0\0\0\0", TEST_XMP].concat()),
            ),
            // ImageMagick's hex-dump form of an EXIF block.
            (
                *b"tEXt",
                text("Raw profile type exif", b"\nexif\n       0\n"),
            ),
            (*b"tEXt", text("Comment", b"kept")),
        ] {
            png.chunks_mut()
                .insert(1, PngChunk::new(kind, contents.into()));
        }
        let tagged = png.encoder().bytes().to_vec();

        let stripped = strip_exif_bytes(tagged.clone(), "png");
        assert!(stripped.len() < tagged.len());
        let parts = Png::from_bytes(stripped.clone().into()).unwrap();
        assert!(parts.chunk_by_type(*b"eXIf").is_none());
        assert!(parts.chunk_by_type(*b"iTXt").is_none());
        // Only the unrelated text chunk survives.
        let texts: Vec<_> = parts.chunks_by_type(*b"tEXt").collect();
        assert_eq!(texts.len(), 1);
        assert!(texts[0].contents().starts_with(b"Comment\0"));
        assert_eq!(
            image::load_from_memory(&stripped).unwrap().to_rgba8(),
            image::load_from_memory(data).unwrap().to_rgba8()
        );
    }

    #[test]
    fn test_strip_webp_keeps_alpha() {
        use crate::image_processing::strip_exif_bytes;
        use img_parts::riff::{RiffChunk, RiffContent};
        use img_parts::webp::WebP;
        use img_parts::Bytes;

        // Lossy WebP with alpha: an extended (VP8X) file with ALPH + VP8 chunks.
        let info: crate::ImageInfo = new_process_image().di.into();
        let plain = info.to_webp(80).unwrap();
        let mut webp = WebP::from_bytes(plain.clone().into()).unwrap();
        assert!(webp.has_chunk(*b"VP8X") && webp.has_chunk(*b"ALPH"));
        for (id, data) in [(*b"EXIF", TEST_EXIF), (*b"XMP ", TEST_XMP)] {
            webp.chunks_mut().push(RiffChunk::new(
                id,
                RiffContent::Data(Bytes::from_static(data)),
            ));
        }
        let tagged = webp.encoder().bytes().to_vec();

        let stripped = strip_exif_bytes(tagged.clone(), "webp");
        assert!(stripped.len() < tagged.len());
        let parts = WebP::from_bytes(stripped.clone().into()).unwrap();
        assert!(!parts.has_chunk(*b"EXIF") && !parts.has_chunk(*b"XMP "));
        // The extended header stays — the alpha plane depends on it — and the image is
        // pixel-for-pixel what it was before the metadata was added.
        assert!(parts.has_chunk(*b"VP8X") && parts.has_chunk(*b"ALPH"));
        let decoded = image::load_from_memory(&stripped).unwrap().to_rgba8();
        assert!(decoded.pixels().any(|p| p.0[3] < 255), "alpha was lost");
        assert_eq!(decoded, image::load_from_memory(&plain).unwrap().to_rgba8());
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
            original: Some(std::sync::Arc::new(di.to_rgba8())),
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
            original: Some(std::sync::Arc::new(di.to_rgba8())),
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
        assert!((127..=128).contains(&a));

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
                original: Some(std::sync::Arc::new(di.to_rgba8())),
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
                original: Some(std::sync::Arc::new(di.to_rgba8())),
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
        // Byte counts are not asserted exactly: they change with encoder releases.
        let source_len = include_bytes!("../assets/rust-logo.png").len();
        assert_eq!(result.ext, "png");
        assert!(!result.buffer.is_empty() && result.buffer.len() < source_len);
        assert_ne!(result.get_diff(), 0.0_f64);
        assert_ne!(result.get_diff(), -1.0_f64);

        let result =
            tokio_test::block_on(OptimProcess::new("avif", 70, 0).process(new_process_image()))
                .unwrap();
        assert_eq!(result.ext, "avif");
        assert!(!result.buffer.is_empty());
        assert_ne!(result.get_diff(), 0.0_f64);
        assert_ne!(result.get_diff(), -1.0_f64);

        // lossless webp (quality >= 100)
        let lossless =
            tokio_test::block_on(OptimProcess::new("webp", 100, 0).process(new_process_image()))
                .unwrap();
        assert_eq!(lossless.ext, "webp");
        assert!(!lossless.buffer.is_empty());
        assert_eq!(lossless.get_diff(), 0.0);

        // lossy webp
        let result =
            tokio_test::block_on(OptimProcess::new("webp", 80, 0).process(new_process_image()))
                .unwrap();
        assert_eq!(result.ext, "webp");
        assert_ne!(result.buffer.len(), 0);
        // A lossy VP8 bitstream. Sizes aren't compared: on this flat logo libwebp's lossless
        // output is the smaller file.
        assert!(result.buffer.windows(4).any(|w| w == b"VP8 "));
        assert!(result.get_diff() >= 0.0);

        let result =
            tokio_test::block_on(OptimProcess::new("jpeg", 70, 0).process(new_process_image()))
                .unwrap();
        assert_eq!(result.ext, "jpeg");
        assert!(!result.buffer.is_empty());
        assert_ne!(result.get_diff(), 0.0_f64);
        assert_ne!(result.get_diff(), -1.0_f64);

        // lossy jxl
        #[cfg(feature = "jxl")]
        {
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
    fn test_quality_search() {
        use super::{
            bisect_quality, gallop_quality, Candidate, AUTO_MAX_QUALITY, AUTO_MIN_QUALITY,
        };
        // Synthetic monotone curve: diff = (100 - q) / 10, so a target of 3.0 is first met
        // at quality 70. Returns the quality found and the qualities probed.
        let search = |target: f64, start: Option<u8>| {
            let mut probes = Vec::new();
            let mut probe = |q: u8| -> super::Result<Candidate> {
                probes.push(q);
                Ok(Candidate {
                    ext: String::new(),
                    quality: q,
                    buffer: Vec::new(),
                    di: image::DynamicImage::default(),
                    diff: f64::from(100 - q) / 10.0,
                })
            };
            let found = match start {
                None => bisect_quality(&mut probe, target, AUTO_MIN_QUALITY - 1, None),
                Some(s) => gallop_quality(&mut probe, target, s),
            };
            (found.unwrap().quality, probes)
        };

        // Whole-range bisection: the plain binary search over 30..=95.
        let (q, probes) = search(3.0, None);
        assert_eq!(q, 70);
        assert_eq!(probes[..2], [62, 79]);
        assert!(probes.len() <= 7, "{probes:?}");
        // From an exact prediction two probes settle it: 70 meets the target, 69 misses.
        assert_eq!(search(3.0, Some(70)), (70, vec![70, 69]));
        // Predictions off in either direction still land on the boundary.
        for start in [30, 50, 68, 69, 71, 72, 90, 95] {
            assert_eq!(search(3.0, Some(start)).0, 70, "start {start}");
        }
        // Unreachable target: the maximum quality's probe, encoded once.
        let (q, probes) = search(0.1, None);
        assert_eq!(q, AUTO_MAX_QUALITY);
        assert_eq!(probes.iter().filter(|&&p| p == AUTO_MAX_QUALITY).count(), 1);
        assert_eq!(search(0.1, Some(60)).0, AUTO_MAX_QUALITY);
        // A target every quality meets: the minimum.
        assert_eq!(search(100.0, None).0, AUTO_MIN_QUALITY);
        assert_eq!(search(100.0, Some(80)).0, AUTO_MIN_QUALITY);
    }

    #[test]
    fn test_auto_quality_avif() {
        // AVIF at a slow speed searches with fast probes, then confirms at the given speed:
        // the result must still honour the target.
        let target = 10.0;
        let result = tokio_test::block_on(
            AutoOptimProcess::new("avif", None, 4, target).process(new_process_image()),
        )
        .unwrap();
        assert_eq!(result.ext, "avif");
        assert!(
            result.diff >= 0.0 && result.diff <= target,
            "{}",
            result.diff
        );
    }

    #[test]
    fn test_pick_candidate() {
        use super::{pick_candidate, Candidate};
        let mk = |ext: &str, size: usize, diff: f64| Candidate {
            ext: ext.to_string(),
            quality: 80,
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

    #[test]
    fn test_auto_quality_after_resize() {
        // The loaded original is 144×144 but the encoder sees 72×72; scoring must use the
        // encoder input, otherwise every probe is "not comparable" and the search silently
        // degrades to the maximum quality.
        let resized =
            tokio_test::block_on(ResizeProcess::new(72, 72).process(new_process_image())).unwrap();
        let target = 10.0;
        let result =
            tokio_test::block_on(AutoOptimProcess::new("webp", None, 0, target).process(resized))
                .unwrap();
        assert!(result.diff >= 0.0, "diff was not computed: {}", result.diff);
        assert!(result.diff <= target);
    }

    #[test]
    fn test_get_buffer_without_encoded_bytes() {
        // After a pixel edit there is no encoded buffer: get_buffer encodes the pixels in the
        // image's own format.
        let info: crate::ImageInfo = new_process_image().di.into();

        // A JPEG that gained transparency still encodes (JPEG has no alpha: it is flattened).
        let jpeg = ProcessImage::new(info.to_mozjpeg(90).unwrap(), "jpeg").unwrap();
        let padded = tokio_test::block_on(PaddingProcess::new(200, 200, "").process(jpeg)).unwrap();
        assert!(padded.di.color().has_alpha());
        let buf = padded.get_buffer().unwrap();
        assert_eq!(image::guess_format(&buf).unwrap(), image::ImageFormat::Jpeg);
        let decoded = image::load_from_memory(&buf).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (200, 200));

        // A JXL stays a JXL (it used to come back as JPEG bytes under the "jxl" extension).
        #[cfg(feature = "jxl")]
        {
            let jxl = ProcessImage::new(info.to_jxl(90).unwrap(), "jxl").unwrap();
            let resized = tokio_test::block_on(ResizeProcess::new(72, 0).process(jxl)).unwrap();
            let buf = resized.get_buffer().unwrap();
            assert_eq!(super::jxl_dimensions(&buf), Some((72, 72)));
        }
    }

    #[test]
    fn test_new_without_original() {
        let data = include_bytes!("../assets/rust-logo.png");
        let img = ProcessImage::new_without_original(data.to_vec(), "png").unwrap();
        assert!(img.original.is_none());
        assert_eq!(img.get_size(), (144, 144));
        assert_eq!(img.get_diff(), -1.0);
    }

    #[test]
    fn test_run_rejects_invalid_tasks() {
        use super::run_with_image;
        let run = |tasks: Vec<Vec<&str>>| {
            let tasks = tasks
                .into_iter()
                .map(|t| t.into_iter().map(String::from).collect())
                .collect();
            tokio_test::block_on(run_with_image(new_process_image(), tasks))
        };
        // Missing load source used to panic with an index out of bounds.
        assert!(run(vec![vec!["load"]]).is_err());
        // Misspelt task names are reported instead of silently skipped.
        assert!(run(vec![vec!["resise", "10", "10"]]).is_err());
        // Present-but-malformed optional params are errors, not silent defaults.
        assert!(run(vec![vec!["rotate", "abc"]]).is_err());
        assert!(run(vec![vec!["blur", "x"]]).is_err());
        assert!(run(vec![vec!["optim", "webp", "auto", "0", "nope"]]).is_err());
        assert!(run(vec![vec!["rotate", "45"]]).is_err());
        // Missing or empty optional params still fall back to their defaults.
        assert_eq!(run(vec![vec!["rotate"]]).unwrap().get_size(), (144, 144));
        assert_eq!(run(vec![vec!["blur", ""]]).unwrap().get_size(), (144, 144));
    }

    #[test]
    fn test_extreme_aspect_ratio_never_zero() {
        use image::{DynamicImage, RgbImage};
        let wide = || ProcessImage {
            di: DynamicImage::ImageRgb8(RgbImage::new(4000, 3)),
            ..Default::default()
        };
        let fit = tokio_test::block_on(ResizeProcess::new_fit(100, 0).process(wide())).unwrap();
        assert_eq!(fit.get_size(), (100, 1));
        let exact = tokio_test::block_on(ResizeProcess::new(1, 0).process(wide())).unwrap();
        assert_eq!(exact.get_size(), (1, 1));
        let thumb = tokio_test::block_on(ThumbnailProcess::new(10, 100).process(wide())).unwrap();
        assert_eq!(thumb.get_size(), (10, 100));
    }

    #[test]
    fn test_crop_outside_image() {
        let result =
            tokio_test::block_on(CropProcess::new(500, 500, 10, 10).process(new_process_image()));
        assert!(result.is_err());
    }

    #[test]
    fn test_opaque_rgb_fast_path() {
        use image::DynamicImage;
        // A fully opaque RGB8 image stays RGB8 through point ops and resize — no alpha
        // channel is materialised, so the buffer stays 25% smaller.
        let rgb = || {
            let img = image::RgbImage::from_fn(6, 4, |x, y| {
                image::Rgb([(x * 40) as u8, (y * 50) as u8, 90])
            });
            ProcessImage {
                di: DynamicImage::ImageRgb8(img),
                ..Default::default()
            }
        };
        let is_rgb = |pi: &ProcessImage| matches!(pi.di, DynamicImage::ImageRgb8(_));

        assert!(is_rgb(
            &tokio_test::block_on(BrightenProcess::new(20).process(rgb())).unwrap()
        ));
        assert!(is_rgb(
            &tokio_test::block_on(GrayProcess::new().process(rgb())).unwrap()
        ));
        assert!(is_rgb(
            &tokio_test::block_on(GammaProcess::new(1.5).process(rgb())).unwrap()
        ));
        assert!(is_rgb(
            &tokio_test::block_on(InvertProcess::new().process(rgb())).unwrap()
        ));
        assert!(is_rgb(
            &tokio_test::block_on(NormalizeProcess::new(true).process(rgb())).unwrap()
        ));
        let resized = tokio_test::block_on(ResizeProcess::new(3, 2).process(rgb())).unwrap();
        assert!(is_rgb(&resized));
        assert_eq!((resized.di.width(), resized.di.height()), (3, 2));

        // Geometry and convolution ops keep the RGB layout too.
        let rotated = tokio_test::block_on(RotateProcess::new(90).process(rgb())).unwrap();
        assert!(is_rgb(&rotated));
        assert_eq!(rotated.get_size(), (4, 6));
        assert!(is_rgb(
            &tokio_test::block_on(FlipProcess::new("v").process(rgb())).unwrap()
        ));
        let cropped = tokio_test::block_on(CropProcess::new(1, 1, 3, 2).process(rgb())).unwrap();
        assert!(is_rgb(&cropped));
        assert_eq!(cropped.get_size(), (3, 2));
        let thumb = tokio_test::block_on(ThumbnailProcess::new(2, 2).process(rgb())).unwrap();
        assert!(is_rgb(&thumb));
        assert_eq!(thumb.get_size(), (2, 2));
        let smart = tokio_test::block_on(ThumbnailProcess::new_smart(2, 2).process(rgb())).unwrap();
        assert!(is_rgb(&smart));
        assert!(is_rgb(
            &tokio_test::block_on(BlurProcess::new(1.0).process(rgb())).unwrap()
        ));
        assert!(is_rgb(
            &tokio_test::block_on(SharpenProcess::new(1.0, 0).process(rgb())).unwrap()
        ));
        assert!(is_rgb(
            &tokio_test::block_on(TrimProcess::new(0).process(rgb())).unwrap()
        ));
        let padded =
            tokio_test::block_on(PaddingProcess::new(8, 8, "#ffffff").process(rgb())).unwrap();
        assert!(is_rgb(&padded));
        assert_eq!(padded.get_size(), (8, 8));
        assert!(is_rgb(
            &tokio_test::block_on(BackgroundProcess::new("#ffffff").process(rgb())).unwrap()
        ));
        assert!(is_rgb(&ProcessImage {
            di: super::apply_orientation(rgb().di, 6),
            ..Default::default()
        }));

        // An image with transparency keeps its alpha channel (rust-logo is RGBA8).
        let out =
            tokio_test::block_on(BrightenProcess::new(20).process(new_process_image())).unwrap();
        assert!(matches!(out.di, DynamicImage::ImageRgba8(_)));
    }

    #[test]
    fn test_blur_matches_between_layouts() {
        use image::DynamicImage;
        // The RGB and RGBA convolution paths must produce the same color channels.
        let rgb = image::RgbImage::from_fn(9, 7, |x, y| {
            image::Rgb([(x * 30) as u8, (y * 35) as u8, ((x + y) * 12) as u8])
        });
        let rgba = DynamicImage::ImageRgb8(rgb.clone()).to_rgba8();
        let blur = |di: DynamicImage| {
            tokio_test::block_on(BlurProcess::new(1.5).process(ProcessImage {
                di,
                ..Default::default()
            }))
            .unwrap()
            .di
            .to_rgb8()
        };
        assert_eq!(
            blur(DynamicImage::ImageRgb8(rgb)),
            blur(DynamicImage::ImageRgba8(rgba))
        );
    }

    #[test]
    fn test_gif_output_after_transform() {
        use super::run_with_image;
        use image::{codecs::gif::GifEncoder, Delay, Frame, Rgba, RgbaImage};
        let task = |t: &[&str]| t.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // A GIF source that was resized used to fail: its original buffer is gone.
        let frame = |v: u8| {
            Frame::from_parts(
                RgbaImage::from_pixel(40, 30, Rgba([v, 40, 90, 255])),
                0,
                0,
                Delay::from_numer_denom_ms(80, 1),
            )
        };
        let mut gif = Vec::new();
        GifEncoder::new(&mut gif)
            .encode_frames([frame(10), frame(220)])
            .unwrap();
        let resized = tokio_test::block_on(run_with_image(
            ProcessImage::new(gif.clone(), "gif").unwrap(),
            vec![
                task(&["resize", "20", "0"]),
                task(&["optim", "gif", "80", "0"]),
            ],
        ))
        .unwrap();
        assert_eq!(resized.ext, "gif");
        let decoded = image::load_from_memory(&resized.get_buffer().unwrap()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (20, 15));

        // Converting a non-GIF source to GIF used to fail the same way.
        let png = tokio_test::block_on(run_with_image(
            new_process_image(),
            vec![task(&["optim", "gif", "80", "0"])],
        ))
        .unwrap();
        assert_eq!(png.ext, "gif");
        assert!(image::load_from_memory(&png.get_buffer().unwrap()).is_ok());

        // An untouched animated GIF converts to an animated WebP, keeping its frames.
        let webp = tokio_test::block_on(run_with_image(
            ProcessImage::new(gif, "gif").unwrap(),
            vec![task(&["optim", "webp", "80", "0"])],
        ))
        .unwrap();
        assert_eq!(webp.ext, "webp");
        let anim = webp::AnimDecoder::new(&webp.get_buffer().unwrap())
            .decode()
            .unwrap();
        assert_eq!(anim.len(), 2);
    }

    /// Minimal ICC v2 display profile: sRGB primaries (D50) with a *linear* tone curve.
    fn linear_rgb_icc() -> Vec<u8> {
        let s15 = |v: f64| ((v * 65536.0).round() as i32).to_be_bytes();
        let xyz = |x: f64, y: f64, z: f64| {
            let mut t = b"XYZ \0\0\0\0".to_vec();
            for v in [x, y, z] {
                t.extend(s15(v));
            }
            t
        };
        // curv with one entry: gamma 1.0 as u8Fixed8, padded to 4 bytes.
        let curv = || b"curv\0\0\0\0\0\0\0\x01\x01\x00\0\0".to_vec();
        let tags: Vec<(&[u8; 4], Vec<u8>)> = vec![
            (b"wtpt", xyz(0.9642, 1.0, 0.8249)),
            (b"rXYZ", xyz(0.4361, 0.2225, 0.0139)),
            (b"gXYZ", xyz(0.3851, 0.7169, 0.0971)),
            (b"bXYZ", xyz(0.1431, 0.0606, 0.7141)),
            (b"rTRC", curv()),
            (b"gTRC", curv()),
            (b"bTRC", curv()),
        ];
        let mut offset = 128 + 4 + 12 * tags.len();
        let mut table = (tags.len() as u32).to_be_bytes().to_vec();
        let mut data = Vec::new();
        for (sig, body) in &tags {
            table.extend(*sig);
            table.extend((offset as u32).to_be_bytes());
            table.extend((body.len() as u32).to_be_bytes());
            offset += body.len();
            data.extend(body);
        }
        let mut header = vec![0u8; 128];
        header[0..4].copy_from_slice(&(offset as u32).to_be_bytes());
        header[8..12].copy_from_slice(&[2, 0x10, 0, 0]);
        header[12..16].copy_from_slice(b"mntr");
        header[16..20].copy_from_slice(b"RGB ");
        header[20..24].copy_from_slice(b"XYZ ");
        header[36..40].copy_from_slice(b"acsp");
        header[68..72].copy_from_slice(&s15(0.9642));
        header[72..76].copy_from_slice(&s15(1.0));
        header[76..80].copy_from_slice(&s15(0.8249));
        [header, table, data].concat()
    }

    #[test]
    fn test_icc_profile_converted_to_srgb() {
        use img_parts::{png::Png, Bytes, ImageICC};
        // Mid-gray stored in linear light: in sRGB it must be re-encoded much brighter.
        let di = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            4,
            4,
            image::Rgb([128, 128, 128]),
        ));
        let mut png = Vec::new();
        di.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let plain = ProcessImage::new(png.clone(), "png").unwrap();
        assert_eq!(plain.di.to_rgb8().get_pixel(0, 0).0, [128, 128, 128]);

        let mut tagged = Png::from_bytes(Bytes::from(png)).unwrap();
        tagged.set_icc_profile(Some(Bytes::from(linear_rgb_icc())));
        let tagged = ProcessImage::new(tagged.encoder().bytes().to_vec(), "png").unwrap();
        let [r, g, b] = tagged.di.to_rgb8().get_pixel(0, 0).0;
        assert!(
            r > 180 && r == g && g == b,
            "expected ~188 gray, got {r},{g},{b}"
        );
    }

    #[test]
    fn test_decode_size_limit() {
        // PNG whose header declares 60000×60000 (14.4 GB as RGBA) with no pixel data.
        let crc32 = |data: &[u8]| {
            let mut crc = 0xffff_ffffu32;
            for &byte in data {
                crc ^= byte as u32;
                for _ in 0..8 {
                    crc = if crc & 1 == 1 {
                        (crc >> 1) ^ 0xedb8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            !crc
        };
        let chunk = |kind: &[u8; 4], body: &[u8]| {
            let mut c = (body.len() as u32).to_be_bytes().to_vec();
            let typed = [kind.as_slice(), body].concat();
            c.extend(&typed);
            c.extend(crc32(&typed).to_be_bytes());
            c
        };
        let ihdr = [
            60000u32.to_be_bytes().as_slice(),
            60000u32.to_be_bytes().as_slice(),
            &[8, 6, 0, 0, 0],
        ]
        .concat();
        let png = [
            b"\x89PNG\r\n\x1a\n".to_vec(),
            chunk(b"IHDR", &ihdr),
            chunk(b"IEND", &[]),
        ]
        .concat();
        assert!(ProcessImage::new(png, "png").is_err());

        // Bare JXL codestream header declaring 100000×100000: rejected before decoding.
        let mut bits: Vec<bool> = Vec::new();
        let mut push = |v: u32, n: usize| (0..n).for_each(|i| bits.push((v >> i) & 1 == 1));
        push(0, 1); // div8 = false
        push(3, 2); // U32 selector → Bits(30)
        push(99_999, 30); // height - 1
        push(1, 3); // ratio 1:1
        let mut jxl = vec![0xff, 0x0a];
        jxl.extend(bits.chunks(8).map(|byte| {
            byte.iter()
                .enumerate()
                .fold(0u8, |acc, (i, &b)| acc | ((b as u8) << i))
        }));
        assert_eq!(super::jxl_dimensions(&jxl), Some((100_000, 100_000)));
        let err = ProcessImage::new(jxl, "jxl").err().unwrap();
        assert!(err.to_string().contains("decode limit"), "{err}");
    }

    #[test]
    fn test_tasks_validated_before_running() {
        use super::{run_with_image, Task};
        let task = |t: &[&str]| t.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // The load would fail with an IO error; the malformed resize after it must be
        // reported first, proving nothing ran.
        let err = tokio_test::block_on(run_with_image(
            ProcessImage::default(),
            vec![
                task(&["load", "file:///definitely/missing.png"]),
                task(&["resize", "abc", "1"]),
            ],
        ))
        .err()
        .unwrap();
        assert!(
            matches!(err, super::ImageProcessingError::ParseInt { .. }),
            "{err}"
        );

        assert!(Task::parse(&task(&["background", "#12345"])).is_err());
        assert!(Task::parse(&task(&["flip", "diagonal"])).is_err());
        assert!(Task::parse(&task(&["normalize", "hsv"])).is_err());
        assert!(Task::parse(&task(&["watermark", "file:///w.png", "middle"])).is_err());

        // A misspelt output format used to be encoded as JPEG without a word.
        let err = Task::parse(&task(&["optim", "wepb", "80", "0"]))
            .err()
            .unwrap();
        assert!(err.to_string().contains("wepb"), "{err}");
        // GIF has no quality to search.
        assert!(Task::parse(&task(&["optim", "gif", "auto", "0"])).is_err());
        assert!(Task::parse(&task(&["optim", "gif", "80", "0"])).is_ok());
        // Empty keeps the source format; names are case-insensitive.
        assert!(Task::parse(&task(&["optim", "", "80", "0"])).is_ok());
        assert_eq!(
            Task::parse(&task(&["optim", "WebP", "80", "0"])).unwrap(),
            Task::Optim {
                output_type: "webp".to_string(),
                quality: 80,
                speed: 0,
            }
        );
        assert_eq!(
            Task::parse(&task(&["optim", "JPG", "auto", "0"])).unwrap(),
            Task::AutoOptim {
                output_type: "jpg".to_string(),
                quality: None,
                speed: 0,
                target: 1.0,
            }
        );
        assert_eq!(
            Task::parse(&task(&["optim", "auto", "auto", "3"])).unwrap(),
            Task::AutoOptim {
                output_type: String::new(),
                quality: None,
                speed: 3,
                target: 1.0,
            }
        );
    }

    #[test]
    fn test_avif_input_decode() {
        // C1: `image`'s avif feature is encoder-only, so AVIF source inputs must route
        // to the libaom decoder. Encode to AVIF, then load the bytes back as a source.
        let avif =
            tokio_test::block_on(OptimProcess::new("avif", 80, 3).process(new_process_image()))
                .unwrap();
        assert_eq!(avif.ext, "avif");
        assert!(!avif.buffer.is_empty());

        let reloaded = ProcessImage::new(avif.buffer, "avif").unwrap();
        assert!(reloaded.di.width() > 0 && reloaded.di.height() > 0);
    }

    #[test]
    fn test_opaque_encode_roundtrips() {
        use image::DynamicImage;
        // A1: opaque RGB8 images encode without an alpha plane (webp/avif/jpeg) and the
        // bytes still decode back to the original dimensions.
        let opaque = || {
            let img = image::RgbImage::from_fn(16, 12, |x, y| {
                image::Rgb([(x * 15) as u8, (y * 20) as u8, 120])
            });
            ProcessImage {
                di: DynamicImage::ImageRgb8(img),
                ext: "png".to_string(),
                ..Default::default()
            }
        };
        for fmt in ["webp", "avif", "jpeg"] {
            let out =
                tokio_test::block_on(OptimProcess::new(fmt, 80, 3).process(opaque())).unwrap();
            assert_eq!(out.ext, fmt);
            assert!(!out.buffer.is_empty(), "{fmt} produced an empty buffer");
            let back = super::decode_to_di(&out.buffer, fmt).unwrap();
            assert_eq!(
                (back.width(), back.height()),
                (16, 12),
                "{fmt} roundtrip dimensions"
            );
        }
    }
}
