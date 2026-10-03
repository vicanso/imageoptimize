#!/usr/bin/env cargo run

// Copyright 2025 Tree xie.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use clap::{Parser, ValueEnum};
use glob::{glob_with, MatchOptions, Pattern};
use imageoptimize::{
    get_exif_orientation, run_with_image, strip_exif_bytes, ImageProcessingError, ProcessImage,
};
use nu_ansi_term::Color::{LightCyan, LightGreen, LightRed, LightYellow};
use snafu::{ResultExt, Snafu};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};
use tokio::fs;
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio::task::JoinSet;

#[derive(Debug, Snafu)]
enum Error {
    #[snafu(display("Optimize image fail, message:{source}"))]
    Optimize { source: ImageProcessingError },
    #[snafu(display("Create directory fail, message:{source}"))]
    CreateDir { source: std::io::Error },
    #[snafu(display("Write file fail, message:{source}"))]
    WriteFile { source: std::io::Error },
    #[snafu(display("{message}"))]
    Common { message: String },
}

type Result<T, E = Error> = std::result::Result<T, E>;

fn parse_resize(s: &str) -> std::result::Result<(u32, u32), String> {
    let (ws, hs) = s
        .split_once('x')
        .ok_or_else(|| format!("expected WxH format (e.g. 1920x1080), got '{s}'"))?;
    let w = ws
        .parse::<u32>()
        .map_err(|_| format!("invalid width '{ws}'"))?;
    let h = hs
        .parse::<u32>()
        .map_err(|_| format!("invalid height '{hs}'"))?;
    if w == 0 && h == 0 {
        return Err("at least one of width or height must be non-zero".to_string());
    }
    Ok((w, h))
}

/// Parse a comma-separated list of positive integers (`what` names the unit for errors).
fn parse_u32_list(s: &str, what: &str, example: &str) -> std::result::Result<Vec<u32>, String> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let v = part
            .parse::<u32>()
            .map_err(|_| format!("invalid {what} '{part}'"))?;
        if v == 0 {
            return Err(format!("{what}s must be greater than 0"));
        }
        out.push(v);
    }
    if out.is_empty() {
        return Err(format!("expected at least one {what}, e.g. {example}"));
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// A responsive variant: either a width descriptor (`Nw`, for fluid images) or a density
/// descriptor (`Nx`, for fixed-display-size images). Both carry the pixel width to render.
#[derive(Debug, Clone, Copy)]
enum Variant {
    Width(u32),
    Density { px: u32, density: u32 },
}

impl Variant {
    /// Pixel width to resize the source to.
    fn pixel_width(&self) -> u32 {
        match self {
            Variant::Width(w) => *w,
            Variant::Density { px, .. } => *px,
        }
    }
    /// The srcset descriptor, e.g. `640w` or `2x`.
    fn descriptor(&self) -> String {
        match self {
            Variant::Width(w) => format!("{w}w"),
            Variant::Density { density, .. } => format!("{density}x"),
        }
    }
    /// Value substituted for the `{x}` pattern token (density, or 1 for width variants).
    fn density(&self) -> u32 {
        match self {
            Variant::Width(_) => 1,
            Variant::Density { density, .. } => *density,
        }
    }
}

/// Insert a variant into a target path via the pattern: `{name}` = stem, `{w}` = pixel
/// width, `{x}` = density, `{ext}` = extension.
fn srcset_path(target: &str, pattern: &str, variant: Variant) -> String {
    let p = Path::new(target);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    let name = pattern
        .replace("{name}", stem)
        .replace("{w}", &variant.pixel_width().to_string())
        .replace("{x}", &variant.density().to_string())
        .replace("{ext}", ext);
    p.with_file_name(name).to_string_lossy().into_owned()
}

static IMAGE_JPEG: &str = "jpeg";
static IMAGE_PNG: &str = "png";
static IMAGE_AVIF: &str = "avif";
static IMAGE_WEBP: &str = "webp";
static IMAGE_JXL: &str = "jxl";

#[derive(ValueEnum, Clone, Debug, PartialEq)]
enum ImageFormat {
    #[value(name = "jpeg")]
    Jpeg,
    #[value(name = "jpg")]
    Jpg,
    #[value(name = "png")]
    Png,
    #[value(name = "webp")]
    Webp,
}

impl ImageFormat {
    fn extensions(&self) -> Vec<&'static str> {
        match self {
            ImageFormat::Jpeg => vec!["jpeg"],
            ImageFormat::Jpg => vec!["jpg"],
            ImageFormat::Png => vec!["png"],
            ImageFormat::Webp => vec!["webp"],
        }
    }
}

/// Map a matched source path into the output tree by swapping only the leading `source`
/// directory for `output`; a deeper path component that repeats the source name is left
/// alone.
fn output_path(path: &Path, source: &str, output: &str) -> PathBuf {
    if source == output {
        return path.to_path_buf();
    }
    // glob may drop a leading "./" from the pattern, so try the bare form as well.
    let bare = source.strip_prefix("./").unwrap_or(source);
    match path
        .strip_prefix(source)
        .or_else(|_| path.strip_prefix(bare))
    {
        Ok(rel) => Path::new(output).join(rel),
        Err(_) => PathBuf::from(path.to_string_lossy().replacen(source, output, 1)),
    }
}

fn modified(path: impl AsRef<Path>) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Pixel width of a source as displayed, i.e. after its EXIF orientation is applied, read
/// from the file header without decoding.
fn source_width(file: &str) -> Option<u32> {
    let (w, h) = image::ImageReader::open(file)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    // EXIF sits near the start of JPEG/PNG files; a prefix avoids reading whole photos.
    let mut head = Vec::new();
    std::fs::File::open(file)
        .ok()?
        .take(1024 * 1024)
        .read_to_end(&mut head)
        .ok()?;
    Some(if get_exif_orientation(&head) >= 5 {
        h
    } else {
        w
    })
}

/// How `--incremental` locates the outputs a source produces.
#[derive(Clone, Copy)]
enum IncrementalMode<'a> {
    /// The target path is written as-is.
    Fixed,
    /// The encoder picks the extension, so any candidate format counts.
    AutoFormat,
    /// One file per variant narrower than the source.
    Srcset {
        variants: &'a [Variant],
        pattern: &'a str,
    },
}

/// True when every output `target` stands for already exists and is newer than `file`.
fn is_up_to_date(file: &str, target: &str, mode: IncrementalMode) -> bool {
    let Some(src_mtime) = modified(file) else {
        return false;
    };
    let fresh = |path: &Path| modified(path).is_some_and(|t| t > src_mtime);
    match mode {
        IncrementalMode::Fixed => fresh(Path::new(target)),
        IncrementalMode::AutoFormat => [IMAGE_WEBP, IMAGE_AVIF, IMAGE_JPEG, IMAGE_PNG]
            .iter()
            .any(|ext| fresh(&Path::new(target).with_extension(ext))),
        IncrementalMode::Srcset { variants, pattern } => {
            let Some(src_w) = source_width(file) else {
                return false;
            };
            variants
                .iter()
                .filter(|v| v.pixel_width() < src_w)
                .all(|&v| fresh(Path::new(&srcset_path(target, pattern, v))))
        }
    }
}

#[derive(ValueEnum, Clone, Debug, PartialEq)]
enum ConvertFormat {
    #[value(name = "jpeg-avif")]
    JpegAvif,
    #[value(name = "jpeg-webp")]
    JpegWebp,
    #[value(name = "png-avif")]
    PngAvif,
    #[value(name = "png-webp")]
    PngWebp,
    #[value(name = "jpeg-jxl")]
    JpegJxl,
    #[value(name = "png-jxl")]
    PngJxl,
    #[value(name = "disable")]
    Disable,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Source image path
    #[arg(short, long)]
    source: Option<String>,
    /// Source image path as positional argument
    #[arg(help = "source image path")]
    source_arg: Option<String>,
    /// Output image path
    #[arg(long)]
    output: Option<String>,
    /// Filter by image formats
    #[arg(
        short,
        long,
        value_enum,
        help = "Filter by image formats (jpeg, jpg, png, webp; extensions match case-insensitively). Default: jpeg,jpg,png"
    )]
    format: Option<Vec<ImageFormat>>,

    /// Write optimized files back to the source directory (used when --output is not given)
    #[arg(short, long)]
    overwrite: bool,

    /// Convert to format
    #[arg(
        long,
        value_enum,
        help = "Convert to format (jpeg-avif, jpeg-webp, png-avif, png-webp, jpeg-jxl, png-jxl). Default: jpeg-avif, jpeg-webp, png-avif, png-webp (jxl is opt-in)"
    )]
    convert: Option<Vec<ConvertFormat>>,

    /// PNG quality (0-99 quantizes to a palette of at most 256 colors, >=100 lossless)
    #[arg(long, default_value = "90")]
    png_quality: u8,

    /// JPEG quality
    #[arg(long, default_value = "80")]
    jpeg_quality: u8,

    /// AVIF quality
    #[arg(long, default_value = "80")]
    avif_quality: u8,

    /// WebP quality (0-99 lossy, >=100 lossless)
    #[arg(long, default_value = "80")]
    webp_quality: u8,

    /// JXL quality (0-99 lossy, >=100 lossless). Applies to `--convert jpeg-jxl / png-jxl`
    /// output, which needs the `jxl` build feature (enabled by default). At >=100 a JPEG
    /// source is recompressed losslessly: ~20% smaller, and the JPEG can be rebuilt bit-exact.
    #[arg(long, default_value = "80")]
    jxl_quality: u8,

    /// Encode at maximum fidelity (forces every per-format quality to 100). WebP and PNG
    /// become truly lossless; AVIF is visually near-lossless only (the rav1e encoder has no
    /// bit-exact mode); JPEG is max-quality lossy (the format has no lossless mode).
    /// Overrides the per-format quality flags. Cannot be combined with --auto-quality /
    /// --auto-format.
    #[arg(long)]
    lossless: bool,

    /// Number of parallel threads (default: number of logical CPUs)
    #[arg(short, long)]
    threads: Option<usize>,

    /// Preview changes without writing any files
    #[arg(long)]
    dry_run: bool,

    /// Skip files smaller than this size (in KB)
    #[arg(long)]
    min_size: Option<u64>,

    /// Exclude files matching these glob patterns (repeatable)
    #[arg(long)]
    exclude: Option<Vec<String>>,

    /// Suppress per-file output; print only the final summary
    #[arg(short = 'q', long)]
    quiet: bool,

    /// Resize images to fit within WxH before encoding (e.g. 1920x1080). Images smaller than
    /// the given dimensions are left untouched. Use 0 to leave a dimension unconstrained.
    #[arg(long, value_name = "WxH", value_parser = parse_resize)]
    resize: Option<(u32, u32)>,

    /// Strip EXIF and XMP metadata (including GPS location) from output files without
    /// re-encoding
    #[arg(long)]
    strip_exif: bool,

    /// AVIF encoder speed (0 = slowest/best quality, 10 = fastest/lower quality)
    #[arg(long, default_value = "4", value_name = "N")]
    avif_speed: u8,

    /// Skip images whose every output file is already newer than the source (only with --output)
    #[arg(long)]
    incremental: bool,

    /// Skip the DSSIM diff metric. Avoids re-decoding AVIF/JXL output just to score it,
    /// noticeably faster for those formats. The DIFF column is left blank.
    #[arg(long)]
    no_diff: bool,

    /// Generate one output per width for responsive images (srcset `Nw` descriptors, for
    /// fluid images). Comma-separated, e.g. 320,640,1280. Widths >= the source width are
    /// skipped (no upscaling). When set, --resize is ignored. Mutually exclusive with
    /// --densities.
    #[arg(long, value_name = "W1,W2,...")]
    widths: Option<String>,

    /// Generate one output per pixel density for fixed-size images (srcset `Nx`
    /// descriptors). Comma-separated multipliers, e.g. 1,2,3. Requires --base-width; each
    /// output is base-width × density pixels. Densities whose width >= the source are
    /// skipped (no upscaling). Mutually exclusive with --widths.
    #[arg(long, value_name = "D1,D2,...")]
    densities: Option<String>,

    /// The 1x display width (CSS px) for --densities; outputs are base-width × density.
    #[arg(long, value_name = "W")]
    base_width: Option<u32>,

    /// Filename pattern for variants: {name} = stem, {w} = pixel width, {x} = density,
    /// {ext} = extension. Defaults to {name}-{w}w.{ext} (widths) or {name}@{x}x.{ext}
    /// (densities).
    #[arg(long)]
    srcset_pattern: Option<String>,

    /// Print a ready-to-paste responsive <source srcset> snippet per source (with --widths
    /// or --densities)
    #[arg(long)]
    emit_html: bool,

    /// Emit a tiny base64 Low-Quality Image Placeholder (a `data:` URI) per source for
    /// blur-up / progressive loading. Printed as a list, or as an HTML comment under each
    /// `--emit-html` snippet. Pairs with `--widths` / `--densities`.
    #[arg(long)]
    lqip: bool,

    /// Placeholder width in pixels for `--lqip` (height follows the aspect ratio).
    #[arg(long, default_value = "32", value_name = "N")]
    lqip_width: u32,

    /// Auto-tune quality per output: binary-search the lowest quality whose perceptual
    /// diff stays within --target-diff. Ignores the per-format quality flags.
    #[arg(long)]
    auto_quality: bool,

    /// Auto-pick the output format: encode each source once as the smallest of webp/avif
    /// plus a lossless fallback (png for images with transparency, otherwise jpeg), each
    /// quality-tuned to --target-diff. Produces one output per source and ignores --convert.
    #[arg(long)]
    auto_format: bool,

    /// Perceptual-diff target (DSSIM ×1000) for --auto-quality / --auto-format. Lower =
    /// higher fidelity. 1.0 is roughly visually lossless.
    #[arg(long, default_value = "1.0", value_name = "N")]
    target_diff: f64,
}

fn relative(path: &str, base: &Path) -> String {
    Path::new(path)
        .strip_prefix(base)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

#[derive(Debug)]
struct ImageOptimizeParams {
    file: String,
    target: String,
}

#[derive(Debug, Clone)]
struct ImageQualities {
    avif: u8,
    avif_speed: u8,
    webp: u8,
    png: u8,
    jpeg: u8,
    jxl: u8,
    /// Perceptual-diff target (DSSIM ×1000) used when auto-quality is enabled.
    target_diff: f64,
}

/// Per-encode behaviour flags. `is_variant` marks a resized srcset derivative, which
/// always writes its output rather than falling back to keeping the original.
#[derive(Debug, Clone, Copy)]
struct EncodeFlags {
    dry_run: bool,
    strip_exif: bool,
    no_diff: bool,
    is_variant: bool,
    /// Search per-format quality to hit `ImageQualities::target_diff` instead of using
    /// the fixed per-format quality.
    auto_quality: bool,
    /// Search both output format and quality, writing the smallest result; the output
    /// extension is chosen by the encoder rather than the target path.
    auto_format: bool,
}

/// What encoding one output produced.
#[derive(Debug)]
struct Encoded {
    /// Size of the output in bytes (the source's size when the original was kept).
    size: usize,
    original_size: usize,
    /// DSSIM ×1000; negative when it was not computed.
    diff: f64,
    /// The output path already existed.
    existed: bool,
    /// The original was kept: re-encoding it did not make it smaller.
    skipped: bool,
    /// The path actually written when it is not the requested target (auto-format picks
    /// the extension).
    path: Option<String>,
}

/// One output of a source file: where it was to go, how long it took and how it went.
struct Outcome {
    target: String,
    /// Set for srcset / density outputs.
    variant: Option<Variant>,
    millis: u128,
    result: Result<Encoded>,
}

impl Outcome {
    /// An output that was never attempted because its source could not be prepared.
    fn failed(target: String, variant: Option<Variant>, message: &str) -> Self {
        Outcome {
            target,
            variant,
            millis: 0,
            result: Err(Error::Common {
                message: message.to_string(),
            }),
        }
    }
}

/// Megabytes of DSSIM working memory always allowed (see [`DiffBudget`]). Enough for three
/// 8.8 MP comparisons side by side, so few threads or small images are never held back.
const DIFF_BUDGET_MIN_MB: u32 = 4096;
/// What each worker thread adds to the budget once that exceeds the minimum, so that on
/// many-core machines scoring keeps pace with the encodes it follows.
const DIFF_MB_PER_THREAD: u32 = 256;

/// Bounds the memory held by the DSSIM comparisons running at the same time. dssim builds
/// multi-scale float pyramids of both images — about 150 bytes per pixel, 1.25 GB for an
/// 8.8 MP photo — so a dozen large images scored at once would take many gigabytes (and 64
/// threads' worth, more than most machines have). Each comparison reserves its estimate
/// from the budget first and waits when it doesn't fit. Measured with 12 threads over 8.8 MP
/// photos: peak memory drops from 9.4 GB to 5.6 GB with no change in run time.
struct DiffBudget {
    megabytes: Semaphore,
    total: u32,
}

impl DiffBudget {
    const BYTES_PER_PIXEL: u64 = 150;

    fn new(megabytes: u32) -> Self {
        DiffBudget {
            megabytes: Semaphore::new(megabytes as usize),
            total: megabytes,
        }
    }

    /// The budget for a run with `threads` worker threads.
    fn for_threads(threads: usize) -> Self {
        let per_thread = u32::try_from(threads)
            .unwrap_or(u32::MAX)
            .saturating_mul(DIFF_MB_PER_THREAD);
        Self::new(per_thread.max(DIFF_BUDGET_MIN_MB))
    }

    /// Megabytes a `width` × `height` comparison reserves: its estimate, capped at the whole
    /// budget so that one comparison always fits, however large the image.
    fn cost(&self, (width, height): (u32, u32)) -> u32 {
        let bytes = u64::from(width) * u64::from(height) * Self::BYTES_PER_PIXEL;
        bytes.div_ceil(1024 * 1024).clamp(1, u64::from(self.total)) as u32
    }

    /// Wait until a comparison of an image this size fits in the budget.
    async fn reserve(&self, size: (u32, u32)) -> SemaphorePermit<'_> {
        self.megabytes
            .acquire_many(self.cost(size))
            .await
            .expect("the semaphore is never closed")
    }
}

/// Settings shared by every per-source task.
struct Job {
    qualities: ImageQualities,
    /// Fit-resize applied before encoding in normal mode.
    resize: Option<(u32, u32)>,
    /// srcset / density variants; empty in normal mode.
    variants: Vec<Variant>,
    srcset_pattern: String,
    /// Flags for normal-mode outputs; variants derive theirs in [`Job::variant_flags`].
    flags: EncodeFlags,
    /// Keep the original RGBA snapshot, which only feeds the explicit diff task.
    keep_original: bool,
    diff_budget: DiffBudget,
    /// Placeholder width when `--lqip` is set.
    lqip_width: Option<u32>,
}

impl Job {
    fn variant_flags(&self) -> EncodeFlags {
        EncodeFlags {
            is_variant: true,
            // srcset variants are per-format derivatives; never auto-pick their format.
            auto_format: false,
            ..self.flags
        }
    }
}

/// Decode and EXIF-orient a source file exactly once, applying the optional resize.
/// The returned `ProcessImage` is cloned per target instead of re-decoding the source.
/// `keep_original` retains the original RGBA snapshot so each output can compute its own
/// diff; without it the snapshot copy and the post-encode re-decode are both skipped.
async fn load_base(
    file: &str,
    resize: Option<(u32, u32)>,
    keep_original: bool,
) -> Result<ProcessImage> {
    let bytes = fs::read(file).await.context(WriteFileSnafu)?;
    let ext = Path::new(file)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let base = tokio::task::block_in_place(|| {
        if keep_original {
            ProcessImage::new(bytes, ext)
        } else {
            ProcessImage::new_without_original(bytes, ext)
        }
    })
    .context(OptimizeSnafu)?;
    match resize {
        Some((max_w, max_h)) => run_with_image(
            base,
            vec![vec![
                "resize".to_string(),
                max_w.to_string(),
                max_h.to_string(),
                "fit".to_string(),
            ]],
        )
        .await
        .context(OptimizeSnafu),
        None => Ok(base),
    }
}

/// Encode an already-decoded `base` image to a single target format and write it out.
async fn encode_target(
    base: ProcessImage,
    file: &str,
    target: &str,
    job: &Job,
    flags: EncodeFlags,
) -> Result<Encoded> {
    let qualities = &job.qualities;
    // Lowercased so `IMG_0001.PNG` is encoded as PNG rather than falling through to JPEG.
    let placeholder_type = target
        .split('.')
        .next_back()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let placeholder_type = placeholder_type.as_str();

    // Build the optim task:
    //  - auto-format: search both format and quality (the encoder picks the extension);
    //  - auto-quality: search quality for the fixed target format;
    //  - otherwise: fixed format + fixed quality.
    let optim = if flags.auto_format {
        vec![
            "optim".to_string(),
            "auto".to_string(),
            "auto".to_string(),
            qualities.avif_speed.to_string(),
            qualities.target_diff.to_string(),
        ]
    } else {
        let quality = match placeholder_type {
            "avif" => qualities.avif,
            "webp" => qualities.webp,
            "png" => qualities.png,
            "jxl" => qualities.jxl,
            _ => qualities.jpeg,
        };
        let speed = if placeholder_type == "avif" {
            qualities.avif_speed
        } else {
            0
        };
        if flags.auto_quality {
            vec![
                "optim".to_string(),
                placeholder_type.to_string(),
                "auto".to_string(),
                speed.to_string(),
                qualities.target_diff.to_string(),
            ]
        } else {
            vec![
                "optim".to_string(),
                placeholder_type.to_string(),
                quality.to_string(),
                speed.to_string(),
            ]
        }
    };
    let mut img = run_with_image(base, vec![optim])
        .await
        .context(OptimizeSnafu)?;

    let mut tasks = Vec::new();
    // Auto modes score their chosen output internally, so a separate diff task is redundant.
    let auto = flags.auto_quality || flags.auto_format;
    let diff = !flags.no_diff && !auto;
    if diff {
        tasks.push(vec!["diff".to_string()]);
    }
    if flags.strip_exif {
        tasks.push(vec!["strip".to_string()]);
    }
    if !tasks.is_empty() {
        // A real comparison needs the original snapshot, which variants never keep. It is
        // memory-hungry, so it waits for room in the budget; encoding above does not.
        let _room = if diff && !flags.is_variant {
            Some(job.diff_budget.reserve(img.get_size()).await)
        } else {
            None
        };
        img = run_with_image(img, tasks).await.context(OptimizeSnafu)?;
    }

    // Auto-format may pick a different extension than the placeholder target carried, so
    // the real write path is derived from the encoder's chosen format.
    let (write_target, out_path) = if flags.auto_format {
        let p = Path::new(target)
            .with_extension(&img.ext)
            .to_string_lossy()
            .into_owned();
        (p.clone(), Some(p))
    } else {
        (target.to_string(), None)
    };
    let out_ext = if flags.auto_format {
        img.ext.as_str()
    } else {
        placeholder_type
    };

    let existed = fs::try_exists(&write_target).await.unwrap_or(false);
    let buf = img.get_buffer().context(OptimizeSnafu)?;
    let size = buf.len();

    let src_ext = Path::new(file)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let src_ext = src_ext.as_str();
    // A width variant is a distinct derivative, never a replacement for the source,
    // so it always writes its encoded output (never falls back to the original).
    let same_format = src_ext == out_ext;
    if !flags.is_variant && same_format && size >= img.original_size {
        // Read the source back only when it has to be copied out or stripped.
        let original = if flags.strip_exif || file != write_target {
            Some(fs::read(file).await.context(WriteFileSnafu)?)
        } else {
            None
        };
        // Stripping EXIF also drops the orientation tag, so a rotated source would display
        // sideways; for those fall through and write the re-encoded (already upright) output.
        let rotated = flags.strip_exif
            && original
                .as_deref()
                .is_some_and(|b| get_exif_orientation(b) != 1);
        if !rotated {
            if let (Some(original), false) = (original, flags.dry_run) {
                let original_len = original.len();
                let bytes_to_write = if flags.strip_exif {
                    strip_exif_bytes(original, src_ext)
                } else {
                    original
                };
                // In place, only rewrite when stripping actually removed metadata.
                if file != write_target || bytes_to_write.len() != original_len {
                    if let Some(parent) = Path::new(&write_target).parent() {
                        fs::create_dir_all(parent).await.context(CreateDirSnafu)?;
                    }
                    fs::write(&write_target, bytes_to_write)
                        .await
                        .context(WriteFileSnafu)?;
                }
            }
            return Ok(Encoded {
                size: img.original_size,
                original_size: img.original_size,
                diff: img.diff,
                existed,
                skipped: true,
                path: out_path,
            });
        }
    }

    if !flags.dry_run {
        if let Some(parent) = Path::new(&write_target).parent() {
            fs::create_dir_all(parent).await.context(CreateDirSnafu)?;
        }
        fs::write(&write_target, buf)
            .await
            .context(WriteFileSnafu)?;
    }
    Ok(Encoded {
        size,
        original_size: img.original_size,
        diff: img.diff,
        existed,
        skipped: false,
        path: out_path,
    })
}

/// Encode `image` to each of `targets`, timing every output. The decoded image is moved
/// into the final encode and cloned for the rest.
async fn encode_all(
    image: ProcessImage,
    file: &str,
    targets: Vec<String>,
    variant: Option<Variant>,
    job: &Job,
    flags: EncodeFlags,
) -> Vec<Outcome> {
    let mut image = Some(image);
    let last = targets.len().saturating_sub(1);
    let mut outcomes = Vec::with_capacity(targets.len());
    for (i, target) in targets.into_iter().enumerate() {
        let img = if i == last {
            image.take().unwrap()
        } else {
            image.as_ref().unwrap().clone()
        };
        let start = Instant::now();
        let result = encode_target(img, file, &target, job, flags).await;
        outcomes.push(Outcome {
            target,
            variant,
            millis: start.elapsed().as_millis(),
            result,
        });
    }
    outcomes
}

/// Process one source file: decode it once, then encode every output it produces. Returns
/// the outcomes and, with `--lqip`, the placeholder data URI.
async fn process_source(
    file: &str,
    targets: Vec<String>,
    job: &Job,
) -> (Vec<Outcome>, Option<String>) {
    if job.variants.is_empty() {
        encode_formats(file, targets, job).await
    } else {
        encode_variants(file, targets, job).await
    }
}

/// Normal mode: decode + resize once, then encode each target format.
async fn encode_formats(
    file: &str,
    targets: Vec<String>,
    job: &Job,
) -> (Vec<Outcome>, Option<String>) {
    let base = match load_base(file, job.resize, job.keep_original).await {
        Ok(base) => base,
        Err(e) => {
            let message = e.to_string();
            let failed = targets
                .into_iter()
                .map(|target| Outcome::failed(target, None, &message))
                .collect();
            return (failed, None);
        }
    };
    let lqip = job.lqip_width.and_then(|w| base.lqip_data_uri(w).ok());
    let outcomes = encode_all(base, file, targets, None, job, job.flags).await;
    (outcomes, lqip)
}

/// srcset / density mode: decode once (no resize here), then every variant × format.
async fn encode_variants(
    file: &str,
    targets: Vec<String>,
    job: &Job,
) -> (Vec<Outcome>, Option<String>) {
    let variant_paths = |variant: Variant| -> Vec<String> {
        targets
            .iter()
            .map(|target| srcset_path(target, &job.srcset_pattern, variant))
            .collect()
    };
    let base = match load_base(file, None, false).await {
        Ok(base) => base,
        Err(e) => {
            let message = e.to_string();
            let failed = targets
                .iter()
                .flat_map(|target| {
                    job.variants.iter().map(|&variant| {
                        let path = srcset_path(target, &job.srcset_pattern, variant);
                        Outcome::failed(path, Some(variant), &message)
                    })
                })
                .collect();
            return (failed, None);
        }
    };
    let lqip = job.lqip_width.and_then(|w| base.lqip_data_uri(w).ok());
    let src_w = base.get_size().0;
    let mut outcomes = Vec::new();
    for &variant in &job.variants {
        let w = variant.pixel_width();
        if w >= src_w {
            continue; // never upscale
        }
        // Resize the decoded image to this width once, reused for all formats.
        let resize = vec![vec!["resize".to_string(), w.to_string(), "0".to_string()]];
        match run_with_image(base.clone(), resize).await {
            Ok(resized) => {
                let encoded = encode_all(
                    resized,
                    file,
                    variant_paths(variant),
                    Some(variant),
                    job,
                    job.variant_flags(),
                )
                .await;
                outcomes.extend(encoded);
            }
            Err(e) => {
                let message = e.to_string();
                outcomes.extend(
                    variant_paths(variant)
                        .into_iter()
                        .map(|path| Outcome::failed(path, Some(variant), &message)),
                );
            }
        }
    }
    (outcomes, lqip)
}

/// Report a usage error and exit.
fn usage_error(message: &str) -> ! {
    println!("imageoptimize: {message}");
    std::process::exit(1);
}

/// The responsive variants asked for by `--widths` (`Nw`) or `--densities` × `--base-width`
/// (`Nx`); empty when neither is given.
fn parse_variants(
    widths: Option<&str>,
    densities: Option<&str>,
    base_width: Option<u32>,
) -> std::result::Result<Vec<Variant>, String> {
    if let Some(s) = widths {
        let widths =
            parse_u32_list(s, "width", "320,640,1280").map_err(|e| format!("--widths {e}"))?;
        return Ok(widths.into_iter().map(Variant::Width).collect());
    }
    let Some(s) = densities else {
        return Ok(Vec::new());
    };
    let base_width = base_width.ok_or("--densities requires --base-width")?;
    let densities =
        parse_u32_list(s, "density", "1,2,3").map_err(|e| format!("--densities {e}"))?;
    Ok(densities
        .into_iter()
        .map(|density| Variant::Density {
            px: base_width.saturating_mul(density),
            density,
        })
        .collect())
}

/// The extra output formats each source type is converted to, from `--convert`.
fn convert_targets(formats: &[ConvertFormat]) -> HashMap<&'static str, Vec<&'static str>> {
    let mut targets: HashMap<&'static str, Vec<&'static str>> = HashMap::new();
    for item in formats {
        let (source, target) = match item {
            ConvertFormat::JpegAvif => (IMAGE_JPEG, IMAGE_AVIF),
            ConvertFormat::JpegWebp => (IMAGE_JPEG, IMAGE_WEBP),
            ConvertFormat::PngAvif => (IMAGE_PNG, IMAGE_AVIF),
            ConvertFormat::PngWebp => (IMAGE_PNG, IMAGE_WEBP),
            ConvertFormat::JpegJxl => (IMAGE_JPEG, IMAGE_JXL),
            ConvertFormat::PngJxl => (IMAGE_PNG, IMAGE_JXL),
            ConvertFormat::Disable => continue,
        };
        targets.entry(source).or_default().push(target);
    }
    targets
}

/// Walk the tree matched by `pattern` once and return the image files to process: those
/// with one of `extensions`, at least `min_size` bytes, and not excluded.
fn find_sources(
    pattern: &str,
    extensions: &[&str],
    min_size: Option<u64>,
    exclude: &[Pattern],
) -> Vec<PathBuf> {
    let options = MatchOptions {
        case_sensitive: false,
        ..MatchOptions::new()
    };
    let entries = match glob_with(pattern, options) {
        Ok(entries) => Some(entries),
        Err(e) => {
            println!("{}", LightRed.paint(format!("Error reading path: {e}")));
            None
        }
    };
    let mut sources = Vec::new();
    for entry in entries.into_iter().flatten() {
        let path = match entry {
            Ok(path) => path,
            Err(e) => {
                println!("{}", LightRed.paint(format!("Error reading path: {e}")));
                continue;
            }
        };
        // Lowercased so camera exports like `IMG_0001.JPG` are found too.
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !extensions.contains(&ext.as_str()) || !path.is_file() {
            continue;
        }
        if let Some(min_bytes) = min_size {
            let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if file_size < min_bytes {
                continue;
            }
        }
        let path_str = path.to_string_lossy();
        if exclude.iter().any(|p| p.matches(&path_str)) {
            continue;
        }
        sources.push(path);
    }
    sources
}

/// The output paths one source produces: its `--convert` formats, then the same-format
/// target. Auto-format emits a single best-format output per source, so the conversion
/// matrix is skipped and only the placeholder target is queued.
fn output_targets(
    path: &Path,
    source: &str,
    output: &str,
    convert: &HashMap<&'static str, Vec<&'static str>>,
    auto_format: bool,
) -> Vec<String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let image_type = match ext.as_str() {
        "png" => IMAGE_PNG,
        "webp" => IMAGE_WEBP,
        _ => IMAGE_JPEG,
    };
    let target = output_path(path, source, output);
    let mut targets = Vec::new();
    if !auto_format {
        for ext in convert.get(image_type).into_iter().flatten() {
            targets.push(target.with_extension(ext));
        }
    }
    targets.push(target);
    targets
        .into_iter()
        .map(|t| t.to_string_lossy().into_owned())
        .collect()
}

const KB: usize = 1024;
const MB: usize = KB * 1024;
// Column widths of the per-output table: fixed upper bounds.
const PCT_COL: usize = 4; // "100%"
const DIFF_COL: usize = 6; // "(0.00)"
const SIZE_COL: usize = 5; // "999kb"
const TIME_COL: usize = 6; // "1000ms"

fn print_table_header() {
    println!(
        "{:>PCT_COL$}  {:>DIFF_COL$}  {:>SIZE_COL$}  {:>TIME_COL$}  FILE",
        "PCT", "DIFF", "SIZE", "TIME",
    );
    println!(
        "{}  {}  {}  {}  ----",
        "-".repeat(PCT_COL),
        "-".repeat(DIFF_COL),
        "-".repeat(SIZE_COL),
        "-".repeat(TIME_COL),
    );
}

/// Print one output's row. Failures are printed even in quiet mode.
fn print_row(target: &str, millis: u128, result: &Result<Encoded>, base: &Path, quiet: bool) {
    let encoded = match result {
        Ok(encoded) => encoded,
        Err(e) => {
            println!(
                "{}",
                LightRed.paint(format!("{}: {e}", relative(target, base)))
            );
            return;
        }
    };
    if quiet {
        return;
    }
    let duration = if millis < 1000 {
        format!("{millis}ms")
    } else {
        format!("{:.1}s", millis as f64 / 1000.0)
    };
    if encoded.skipped {
        println!(
            "{:>PCT_COL$}  {:>DIFF_COL$}  {:>SIZE_COL$}  {:>TIME_COL$}  {} {}",
            LightYellow.paint("SKIP"),
            "",
            "",
            duration,
            relative(target, base),
            LightYellow.paint("(-)"),
        );
        return;
    }
    let size = encoded.size;
    let size_str = if size >= MB {
        format!("{}mb", size / MB)
    } else if size >= KB {
        format!("{}kb", size / KB)
    } else {
        format!("{size}b")
    };
    // diff < 0 means "not computed" (--no-diff, after a resize, or GIF).
    let diff = encoded.diff;
    let diff_inner = if diff < 0.0 {
        format!("{:>4}", "—")
    } else {
        let diff_num = format!("{diff:>4.2}");
        if diff > 1.0 {
            LightYellow.paint(&diff_num).to_string()
        } else {
            LightGreen.paint(&diff_num).to_string()
        }
    };
    let percent = (size * 100).checked_div(encoded.original_size).unwrap_or(0);
    let status = if encoded.existed {
        LightYellow.paint("(U)").to_string()
    } else {
        LightGreen.paint("(N)").to_string()
    };
    println!(
        "{:>PCT_COL$}  ({diff_inner})  {:>SIZE_COL$}  {:>TIME_COL$}  {} {status}",
        format!("{percent}%"),
        size_str,
        duration,
        relative(target, base),
    );
}

/// Size for the summary lines, with one decimal.
fn format_size(size: usize) -> String {
    if size >= MB {
        format!("{:.1}mb", size as f64 / MB as f64)
    } else if size >= KB {
        format!("{:.1}kb", size as f64 / KB as f64)
    } else {
        format!("{size}b")
    }
}

/// Totals over every output of the run.
#[derive(Default)]
struct Summary {
    /// Bytes before / after for the outputs that replace their source.
    original: usize,
    optimized: usize,
    count: usize,
    /// Sources kept as they were.
    skipped: usize,
    /// Failed outputs of any kind.
    errors: usize,
    /// srcset / density derivatives, tallied separately from the savings.
    variants: usize,
    variant_bytes: usize,
}

impl Summary {
    fn error_note(&self) -> String {
        if self.errors > 0 {
            format!(", {} failed", LightRed.paint(format!("{}", self.errors)))
        } else {
            String::new()
        }
    }

    fn print_variants(&self, dry_run: bool) {
        if self.variants == 0 && self.errors == 0 {
            return;
        }
        println!();
        let verb = if dry_run {
            "Would generate"
        } else {
            "Generated"
        };
        println!(
            "{}",
            LightCyan.paint(format!(
                "{verb} {} variant{} ({} total){}",
                self.variants,
                if self.variants == 1 { "" } else { "s" },
                format_size(self.variant_bytes),
                self.error_note(),
            ))
        );
    }

    fn print_savings(&self, dry_run: bool) {
        if self.count == 0 && self.skipped == 0 && self.errors == 0 {
            return;
        }
        let saved = self.original.saturating_sub(self.optimized);
        let saved_pct = (saved * 100).checked_div(self.original).unwrap_or(0);
        let skipped_note = if self.skipped > 0 {
            format!(
                ", {} unchanged",
                LightYellow.paint(format!("{}", self.skipped))
            )
        } else {
            String::new()
        };
        println!();
        let verb = if dry_run {
            "Would optimize"
        } else {
            "Optimized"
        };
        println!(
            "{}",
            LightCyan.paint(format!(
                "{verb} {} file{}: {} → {}, saved {} ({saved_pct}%){skipped_note}{}",
                self.count,
                if self.count == 1 { "" } else { "s" },
                format_size(self.original),
                format_size(self.optimized),
                LightGreen.paint(format_size(saved)),
                self.error_note(),
            ))
        );
    }
}

/// `--emit-html`: one `<source srcset>` line per format for each source, with its LQIP as
/// a comment when there is one. `html` maps a source to (variant path, variant, extension).
fn print_html(
    html: BTreeMap<String, Vec<(String, Variant, String)>>,
    lqips: &BTreeMap<String, String>,
    base: &Path,
) {
    if html.is_empty() {
        return;
    }
    println!();
    for (file, variants) in html {
        println!(
            "{}",
            LightCyan.paint(format!("<!-- {} -->", relative(&file, base)))
        );
        let mut by_ext: BTreeMap<String, Vec<(String, Variant)>> = BTreeMap::new();
        for (path, variant, ext) in variants {
            by_ext.entry(ext).or_default().push((path, variant));
        }
        for (ext, mut list) in by_ext {
            list.sort_by_key(|(_, v)| v.pixel_width());
            let srcset = list
                .iter()
                .map(|(p, v)| format!("{p} {}", v.descriptor()))
                .collect::<Vec<_>>()
                .join(", ");
            println!("  <source type=\"image/{ext}\" srcset=\"{srcset}\">");
        }
        if let Some(uri) = lqips.get(&file) {
            println!("  <!-- LQIP: {uri} -->");
        }
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let Some(source) = args.source.or(args.source_arg) else {
        usage_error("try 'imageoptimize -h' or 'imageoptimize --help' for more information");
    };
    let output = match args.output {
        Some(output) => output,
        None if args.overwrite => source.clone(),
        None => String::new(),
    };
    if output.is_empty() {
        usage_error("output path is empty");
    }
    if args.widths.is_some() && args.densities.is_some() {
        usage_error("use either --widths or --densities, not both");
    }
    // --lossless pins the per-format quality to 100; the auto modes search quality instead,
    // so the two are mutually exclusive rather than silently ignoring one.
    if args.lossless && (args.auto_quality || args.auto_format) {
        usage_error("--lossless cannot be combined with --auto-quality / --auto-format");
    }
    let variants = parse_variants(
        args.widths.as_deref(),
        args.densities.as_deref(),
        args.base_width,
    )
    .unwrap_or_else(|message| usage_error(&message));

    let dry_run = args.dry_run;
    let quiet = args.quiet;
    let emit_html = args.emit_html;
    let srcset_mode = !variants.is_empty();
    // Auto-format produces one best-format output per source; it is mutually exclusive with
    // srcset (which is inherently per-format) and takes no effect there.
    let auto_format_mode = args.auto_format && !srcset_mode;
    if args.auto_format && srcset_mode {
        println!(
            "{}",
            LightYellow.paint("--auto-format is ignored with --widths / --densities")
        );
    }
    // Default the filename pattern to a width- or density-appropriate template.
    let srcset_pattern = args.srcset_pattern.unwrap_or_else(|| {
        if args.densities.is_some() {
            "{name}@{x}x.{ext}".to_string()
        } else {
            "{name}-{w}w.{ext}".to_string()
        }
    });
    let exclude_patterns: Vec<Pattern> = args
        .exclude
        .unwrap_or_default()
        .iter()
        .filter_map(|p| match Pattern::new(p) {
            Ok(pat) => Some(pat),
            Err(e) => {
                println!(
                    "{}",
                    LightRed.paint(format!("Invalid exclude pattern '{p}': {e}"))
                );
                None
            }
        })
        .collect();

    let base = PathBuf::from(&output);

    let formats = args
        .format
        .unwrap_or_else(|| vec![ImageFormat::Jpeg, ImageFormat::Jpg, ImageFormat::Png]);
    let mut extensions: Vec<&str> = formats
        .iter()
        .flat_map(|format| format.extensions())
        .collect();
    extensions.sort();
    extensions.dedup();

    let convert = convert_targets(&args.convert.unwrap_or_else(|| {
        vec![
            ConvertFormat::JpegAvif,
            ConvertFormat::JpegWebp,
            ConvertFormat::PngAvif,
            ConvertFormat::PngWebp,
        ]
    }));

    // The tree is walked once and filtered by extension, rather than once per extension.
    let pattern = format!("{source}/**/*");
    if !quiet {
        println!(
            "Searching pattern: {}",
            LightCyan.paint(format!(
                "{}.{{{}}}",
                relative(&pattern, &base),
                extensions.join(",")
            ))
        );
    }
    let min_size_bytes = args.min_size.map(|kb| kb * 1024);
    let mut image_optimize_params = vec![];
    for path in find_sources(&pattern, &extensions, min_size_bytes, &exclude_patterns) {
        let file = path.to_string_lossy().to_string();
        for target in output_targets(&path, &source, &output, &convert, auto_format_mode) {
            image_optimize_params.push(ImageOptimizeParams {
                file: file.clone(),
                target,
            });
        }
    }

    // Total unique source images found (before incremental filter).
    let total_source_count = image_optimize_params
        .iter()
        .map(|i| i.file.as_str())
        .collect::<HashSet<_>>()
        .len();
    if total_source_count == 0 {
        println!("{}", LightYellow.paint("No images found."));
        return;
    }

    // --incremental: drop targets whose output is already newer than the source.
    let mut incremental_skipped = 0usize;
    if args.incremental && source != output {
        let mode = if srcset_mode {
            IncrementalMode::Srcset {
                variants: &variants,
                pattern: &srcset_pattern,
            }
        } else if auto_format_mode {
            IncrementalMode::AutoFormat
        } else {
            IncrementalMode::Fixed
        };
        image_optimize_params.retain(|item| !is_up_to_date(&item.file, &item.target, mode));
        let remaining: HashSet<&str> = image_optimize_params
            .iter()
            .map(|i| i.file.as_str())
            .collect();
        incremental_skipped = total_source_count - remaining.len();
    }

    // Group every output target under its source file so each source is decoded once
    // and reused across all its output formats.
    let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
    for item in image_optimize_params {
        grouped.entry(item.file).or_default().push(item.target);
    }

    let incremental_note = if incremental_skipped > 0 {
        format!(
            ", {} up-to-date",
            LightYellow.paint(format!("{incremental_skipped}"))
        )
    } else {
        String::new()
    };
    println!(
        "Found {} image{}{}",
        LightCyan.paint(format!("{total_source_count}")),
        if total_source_count == 1 { "" } else { "s" },
        incremental_note,
    );
    if grouped.is_empty() {
        return;
    }

    if dry_run {
        println!(
            "{}",
            LightYellow.paint("[DRY RUN] No files will be written.")
        );
    }
    if !quiet {
        print_table_header();
    }

    // `--threads 0` would leave every task waiting for a permit forever.
    let concurrency = args.threads.unwrap_or_else(num_cpus::get).max(1);
    // --lossless forces every per-format quality to 100. WebP, PNG (and JXL) treat >=100 as
    // a true lossless encode; AVIF/JPEG have no lossless mode so 100 is best-effort.
    let quality_of = |fixed: u8| if args.lossless { 100 } else { fixed };
    let flags = EncodeFlags {
        dry_run,
        strip_exif: args.strip_exif,
        no_diff: args.no_diff,
        is_variant: false,
        auto_quality: args.auto_quality,
        auto_format: auto_format_mode,
    };
    let job = Arc::new(Job {
        qualities: ImageQualities {
            avif: quality_of(args.avif_quality),
            avif_speed: args.avif_speed,
            webp: quality_of(args.webp_quality),
            png: quality_of(args.png_quality),
            jpeg: quality_of(args.jpeg_quality),
            jxl: quality_of(args.jxl_quality),
            target_diff: args.target_diff,
        },
        resize: args.resize,
        variants,
        srcset_pattern,
        flags,
        // The original snapshot only feeds the explicit diff task: auto modes score against
        // their own encoder input, and srcset variants are always resized (never comparable).
        keep_original: !flags.no_diff && !flags.auto_quality && !flags.auto_format,
        diff_budget: DiffBudget::for_threads(concurrency),
        lqip_width: args.lqip.then_some(args.lqip_width),
    });

    // One task per source file, at most `--threads` of them at a time.
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let mut join_set: JoinSet<(String, Vec<Outcome>, Option<String>)> = JoinSet::new();
    for (file, targets) in grouped {
        let job = job.clone();
        let sem = semaphore.clone();
        join_set.spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let (outcomes, lqip) = process_source(&file, targets, &job).await;
            (file, outcomes, lqip)
        });
    }

    let mut summary = Summary::default();
    // For --emit-html: source file -> (relative variant path, variant, ext).
    let mut html: BTreeMap<String, Vec<(String, Variant, String)>> = BTreeMap::new();
    // For --lqip: source file -> data: URI placeholder.
    let mut lqips: BTreeMap<String, String> = BTreeMap::new();

    while let Some(task) = join_set.join_next().await {
        let (file, outcomes, lqip) = task.expect("task panicked");
        if let Some(uri) = lqip {
            lqips.insert(file.clone(), uri);
        }
        let src_ext = Path::new(&file)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        for outcome in outcomes {
            // Auto-format writes a different extension than the placeholder target carried;
            // use the encoder's actual output path for display and accounting.
            let target = match &outcome.result {
                Ok(Encoded {
                    path: Some(actual), ..
                }) => actual.clone(),
                _ => outcome.target,
            };
            let tgt_ext = Path::new(&target)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string();
            match (&outcome.result, outcome.variant) {
                // Every failed output counts, conversions included: they aren't summed into
                // the savings, but a failure must still be reported and fail the run.
                (Err(_), _) => summary.errors += 1,
                // Normal mode: only count same-format optimisation toward the savings
                // summary; avif/webp conversions are reported per row but not summed.
                // Auto-format always yields a single replacement output, so it counts too.
                (Ok(encoded), None) => {
                    if auto_format_mode || src_ext.eq_ignore_ascii_case(&tgt_ext) {
                        if encoded.skipped {
                            summary.skipped += 1;
                        } else {
                            summary.original += encoded.original_size;
                            summary.optimized += encoded.size;
                            summary.count += 1;
                        }
                    }
                }
                // srcset/density variant: a derivative output, tallied separately from "saved".
                (Ok(encoded), Some(variant)) => {
                    summary.variants += 1;
                    summary.variant_bytes += encoded.size;
                    if emit_html {
                        html.entry(file.clone()).or_default().push((
                            relative(&target, &base),
                            variant,
                            tgt_ext,
                        ));
                    }
                }
            }
            print_row(&target, outcome.millis, &outcome.result, &base, quiet);
        }
    }

    if srcset_mode {
        summary.print_variants(dry_run);
        if emit_html {
            print_html(html, &lqips, &base);
        }
    } else {
        summary.print_savings(dry_run);
    }

    // --lqip: print the placeholder data URIs. --emit-html already embeds them as comments
    // under each source's snippet, so skip the standalone list in that case.
    let embedded_in_html = emit_html && srcset_mode;
    if !(lqips.is_empty() || embedded_in_html) {
        println!();
        println!("{}", LightCyan.paint("LQIP placeholders:"));
        for (file, uri) in &lqips {
            println!("  {}  {}", relative(file, &base), uri);
        }
    }

    // A non-zero exit lets scripts and CI notice that some outputs could not be produced.
    if summary.errors > 0 {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        convert_targets, find_sources, format_size, is_up_to_date, output_path, output_targets,
        parse_u32_list, parse_variants, srcset_path, ConvertFormat, DiffBudget, IncrementalMode,
        Variant,
    };
    use glob::Pattern;
    use std::path::{Path, PathBuf};

    #[test]
    fn test_parse_variants() {
        assert!(parse_variants(None, None, None).unwrap().is_empty());
        let widths = parse_variants(Some("640,320"), None, None).unwrap();
        let px: Vec<u32> = widths.iter().map(|v| v.pixel_width()).collect();
        assert_eq!(px, [320, 640]);
        assert_eq!(widths[0].descriptor(), "320w");
        // Densities multiply the base width.
        let densities = parse_variants(None, Some("1,2"), Some(50)).unwrap();
        let px: Vec<u32> = densities.iter().map(|v| v.pixel_width()).collect();
        assert_eq!(px, [50, 100]);
        assert_eq!(densities[1].descriptor(), "2x");
        // Errors name the flag at fault.
        let err = parse_variants(None, Some("1,2"), None).unwrap_err();
        assert_eq!(err, "--densities requires --base-width");
        assert!(parse_variants(Some("0"), None, None)
            .unwrap_err()
            .starts_with("--widths "));
    }

    #[test]
    fn test_output_targets() {
        let convert = convert_targets(&[
            ConvertFormat::JpegAvif,
            ConvertFormat::Disable,
            ConvertFormat::JpegWebp,
            ConvertFormat::PngWebp,
        ]);
        assert_eq!(convert["jpeg"], ["avif", "webp"]);
        assert_eq!(convert["png"], ["webp"]);

        // Conversions first, then the same-format target; `.JPG` counts as JPEG.
        let targets = output_targets(Path::new("in/a/IMG.JPG"), "in", "out", &convert, false);
        assert_eq!(
            targets,
            ["out/a/IMG.avif", "out/a/IMG.webp", "out/a/IMG.JPG"]
        );
        // A WebP source has no conversions.
        let targets = output_targets(Path::new("in/w.webp"), "in", "out", &convert, false);
        assert_eq!(targets, ["out/w.webp"]);
        // Auto-format queues only the placeholder target.
        let targets = output_targets(Path::new("in/p.png"), "in", "out", &convert, true);
        assert_eq!(targets, ["out/p.png"]);
    }

    #[test]
    fn test_find_sources() {
        let dir = std::env::temp_dir().join(format!("imageoptimize-find-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub/dir.png")).unwrap();
        for (name, len) in [
            ("a.jpg", 10),
            ("B.JPG", 10),
            ("small.png", 2),
            ("sub/c.png", 10),
            ("sub/dir.png/d.jpg", 10),
            ("notes.txt", 10),
        ] {
            std::fs::write(dir.join(name), vec![0u8; len]).unwrap();
        }
        let pattern = format!("{}/**/*", dir.to_string_lossy());
        let find = |extensions: &[&str], min_size, exclude: &[Pattern]| {
            let mut found: Vec<String> = find_sources(&pattern, extensions, min_size, exclude)
                .iter()
                .map(|p| p.strip_prefix(&dir).unwrap().to_string_lossy().into_owned())
                .collect();
            found.sort();
            found
        };
        // Extensions match case-insensitively; a directory named like an image is skipped.
        assert_eq!(
            find(&["jpg", "png"], None, &[]),
            [
                "B.JPG",
                "a.jpg",
                "small.png",
                "sub/c.png",
                "sub/dir.png/d.jpg"
            ]
        );
        assert_eq!(find(&["png"], Some(5), &[]), ["sub/c.png"]);
        let exclude = [Pattern::new("**/sub/**").unwrap()];
        assert_eq!(find(&["jpg", "png"], Some(5), &exclude), ["B.JPG", "a.jpg"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_diff_budget() {
        let budget = DiffBudget::new(2048);
        // About 150 bytes per pixel: 1 MP reserves ~143 MB, an 8.8 MP photo ~1.25 GB.
        assert_eq!(budget.cost((1000, 1000)), 144);
        assert_eq!(budget.cost((4096, 2160)), 1266);
        assert_eq!(budget.cost((1, 1)), 1);
        // Larger than the whole budget: capped, so it can still run (alone).
        assert_eq!(budget.cost((20_000, 20_000)), 2048);

        let photo = budget.cost((4096, 2160));
        let first = budget.megabytes.try_acquire_many(photo).unwrap();
        // A second large comparison has to wait, while small ones still fit beside it.
        assert!(budget.megabytes.try_acquire_many(photo).is_err());
        let small = budget.cost((1000, 1000));
        assert!(budget.megabytes.try_acquire_many(small).is_ok());
        drop(first);
        assert!(budget.megabytes.try_acquire_many(photo).is_ok());

        // 4 GB at least, growing with the thread count beyond 16 threads.
        assert_eq!(DiffBudget::for_threads(1).total, 4096);
        assert_eq!(DiffBudget::for_threads(12).total, 4096);
        assert_eq!(DiffBudget::for_threads(64).total, 16_384);
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(512), "512b");
        assert_eq!(format_size(1536), "1.5kb");
        assert_eq!(format_size(5 * 1024 * 1024 / 2), "2.5mb");
    }

    #[test]
    fn test_output_path() {
        // Only the leading source directory is swapped, not a repeated name deeper down.
        assert_eq!(
            output_path(Path::new("img/a/img/b.png"), "img", "out"),
            PathBuf::from("out/a/img/b.png")
        );
        assert_eq!(
            output_path(Path::new("img/b.png"), "./img", "out"),
            PathBuf::from("out/b.png")
        );
        assert_eq!(
            output_path(Path::new("img/b.png"), "img", "img"),
            PathBuf::from("img/b.png")
        );
    }

    #[test]
    fn test_is_up_to_date() {
        let dir = std::env::temp_dir().join(format!("imageoptimize-incr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("photo.png");
        image::RgbImage::new(100, 50).save(&src).unwrap();
        let src = src.to_string_lossy().into_owned();
        // Outputs are written after the source, so they count as fresh.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let target = dir.join("out.png").to_string_lossy().into_owned();

        assert!(!is_up_to_date(&src, &target, IncrementalMode::Fixed));
        std::fs::write(dir.join("out.webp"), b"x").unwrap();
        assert!(!is_up_to_date(&src, &target, IncrementalMode::Fixed));
        // Auto-format may have written any candidate extension.
        assert!(is_up_to_date(&src, &target, IncrementalMode::AutoFormat));

        // srcset: only variants narrower than the 100px source are expected.
        let variants = [Variant::Width(40), Variant::Width(80), Variant::Width(200)];
        let mode = IncrementalMode::Srcset {
            variants: &variants,
            pattern: "{name}-{w}w.{ext}",
        };
        std::fs::write(dir.join("out-40w.png"), b"x").unwrap();
        assert!(!is_up_to_date(&src, &target, mode));
        std::fs::write(dir.join("out-80w.png"), b"x").unwrap();
        assert!(is_up_to_date(&src, &target, mode));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_parse_u32_list() {
        assert_eq!(
            parse_u32_list("320,640,1280", "width", "320,640,1280").unwrap(),
            vec![320, 640, 1280]
        );
        // trims, sorts, dedups
        assert_eq!(
            parse_u32_list(" 3, 1 , 2,1 ", "density", "1,2,3").unwrap(),
            vec![1, 2, 3]
        );
        assert!(parse_u32_list("0", "density", "1,2,3").is_err());
        assert!(parse_u32_list("abc", "width", "320").is_err());
        assert!(parse_u32_list("", "width", "320").is_err());
    }

    #[test]
    fn test_variant() {
        let w = Variant::Width(640);
        assert_eq!(w.pixel_width(), 640);
        assert_eq!(w.descriptor(), "640w");
        assert_eq!(w.density(), 1);

        let d = Variant::Density { px: 96, density: 3 };
        assert_eq!(d.pixel_width(), 96);
        assert_eq!(d.descriptor(), "3x");
        assert_eq!(d.density(), 3);
    }

    #[test]
    fn test_srcset_path() {
        // width pattern uses pixel width
        assert_eq!(
            srcset_path("out/photo.jpg", "{name}-{w}w.{ext}", Variant::Width(640)),
            "out/photo-640w.jpg"
        );
        // density pattern uses the multiplier; {w} still resolves to the pixel width
        assert_eq!(
            srcset_path(
                "out/logo.png",
                "{name}@{x}x.{ext}",
                Variant::Density { px: 96, density: 3 }
            ),
            "out/logo@3x.png"
        );
        assert_eq!(
            srcset_path(
                "out/logo.png",
                "{name}-{w}px-{x}x.{ext}",
                Variant::Density { px: 64, density: 2 }
            ),
            "out/logo-64px-2x.png"
        );
    }
}
