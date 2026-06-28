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
use glob::{glob, Pattern};
use imageoptimize::{run_with_image, strip_exif_bytes, ImageProcessingError, ProcessImage};
use nu_ansi_term::Color::{LightCyan, LightGreen, LightRed, LightYellow};
use snafu::{ResultExt, Snafu};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::fs;
use tokio::sync::Semaphore;
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
}

impl ImageFormat {
    fn extensions(&self) -> Vec<&'static str> {
        match self {
            ImageFormat::Jpeg => vec!["jpeg"],
            ImageFormat::Jpg => vec!["jpg"],
            ImageFormat::Png => vec!["png"],
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
        help = "Filter by image formats (jpeg, jpg, png). Default: jpeg,jpg,png"
    )]
    format: Option<Vec<ImageFormat>>,

    /// Override quality
    #[arg(short, long)]
    overwrite: bool,

    /// Convert to format
    #[arg(
        long,
        value_enum,
        help = "Convert to format (jpeg-avif, jpeg-webp, png-avif, png-webp, jpeg-jxl, png-jxl). Default: jpeg-avif, jpeg-webp, png-avif, png-webp (jxl is opt-in)"
    )]
    convert: Option<Vec<ConvertFormat>>,

    /// PNG quality
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
    /// output, which needs the `jxl` build feature (enabled by default).
    #[arg(long, default_value = "80")]
    jxl_quality: u8,

    /// Encode at maximum fidelity (forces every per-format quality to 100). WebP becomes
    /// truly lossless; AVIF is visually near-lossless only (the rav1e encoder has no
    /// bit-exact mode); JPEG is max-quality lossy (the format has no lossless mode); PNG
    /// uses its highest-quality palette. Overrides the per-format quality flags. Cannot be
    /// combined with --auto-quality / --auto-format.
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

    /// Strip EXIF metadata (including GPS location) from output files without re-encoding
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

/// Decode and EXIF-orient a source file exactly once, applying the optional resize.
/// The returned `ProcessImage` keeps the original RGBA snapshot so each output format
/// can compute its own diff, and is cloned per target instead of re-decoding the source.
async fn load_base(file: &str, resize: Option<(u32, u32)>) -> Result<ProcessImage> {
    let bytes = fs::read(file).await.context(WriteFileSnafu)?;
    let ext = Path::new(file)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    // ProcessImage::new keeps the original snapshot (needed for the per-target diff).
    let base =
        tokio::task::block_in_place(|| ProcessImage::new(bytes, ext)).context(OptimizeSnafu)?;
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
/// The trailing `Option<String>` is the actual written path when it differs from `target`
/// (auto-format chooses the output extension); `None` means `target` was used as-is.
async fn encode_target(
    base: ProcessImage,
    file: &str,
    target: &str,
    qualities: &ImageQualities,
    flags: EncodeFlags,
) -> Result<(usize, usize, f64, bool, bool, Option<String>)> {
    let placeholder_type = target.split('.').next_back().unwrap_or_default();

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
    let mut tasks = vec![optim];
    // Auto modes score their chosen output internally, so a separate diff task is redundant.
    let auto = flags.auto_quality || flags.auto_format;
    if !flags.no_diff && !auto {
        tasks.push(vec!["diff".to_string()]);
    }
    if flags.strip_exif {
        tasks.push(vec!["strip".to_string()]);
    }

    let img = run_with_image(base, tasks).await.context(OptimizeSnafu)?;

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
        .unwrap_or("");
    // A width variant is a distinct derivative, never a replacement for the source,
    // so it always writes its encoded output (never falls back to the original).
    let same_format = src_ext == out_ext;
    if !flags.is_variant && same_format && size >= img.original_size {
        if !flags.dry_run && file != write_target {
            if let Some(parent) = Path::new(&write_target).parent() {
                fs::create_dir_all(parent).await.context(CreateDirSnafu)?;
            }
            let original = fs::read(file).await.context(WriteFileSnafu)?;
            let bytes_to_write = if flags.strip_exif {
                strip_exif_bytes(original, src_ext)
            } else {
                original
            };
            fs::write(&write_target, bytes_to_write)
                .await
                .context(WriteFileSnafu)?;
        }
        return Ok((
            img.original_size,
            img.original_size,
            img.diff,
            existed,
            true,
            out_path,
        ));
    }

    if !flags.dry_run {
        if let Some(parent) = Path::new(&write_target).parent() {
            fs::create_dir_all(parent).await.context(CreateDirSnafu)?;
        }
        fs::write(&write_target, buf)
            .await
            .context(WriteFileSnafu)?;
    }
    Ok((size, img.original_size, img.diff, existed, false, out_path))
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let Some(source) = args.source.or(args.source_arg) else {
        println!(
            "imageoptimize: try 'imageoptimize -h' or 'imageoptimize --help' for more information"
        );
        std::process::exit(1);
    };

    let output = args.output.unwrap_or_else(|| {
        if args.overwrite {
            source.clone()
        } else {
            "".to_string()
        }
    });
    if output.is_empty() {
        println!("imageoptimize: output path is empty");
        std::process::exit(1);
    }

    let dry_run = args.dry_run;
    let quiet = args.quiet;
    let resize = args.resize;
    let strip_exif = args.strip_exif;
    let incremental = args.incremental;
    let no_diff = args.no_diff;
    if args.widths.is_some() && args.densities.is_some() {
        println!("imageoptimize: use either --widths or --densities, not both");
        std::process::exit(1);
    }
    // --lossless pins the per-format quality to 100; the auto modes search quality instead,
    // so the two are mutually exclusive rather than silently ignoring one.
    if args.lossless && (args.auto_quality || args.auto_format) {
        println!(
            "imageoptimize: --lossless cannot be combined with --auto-quality / --auto-format"
        );
        std::process::exit(1);
    }
    // Build the responsive variant list from either widths (Nw) or densities × base-width (Nx).
    let variants: Vec<Variant> = if let Some(s) = args.widths.as_deref() {
        match parse_u32_list(s, "width", "320,640,1280") {
            Ok(ws) => ws.into_iter().map(Variant::Width).collect(),
            Err(e) => {
                println!("imageoptimize: --widths {e}");
                std::process::exit(1);
            }
        }
    } else if let Some(s) = args.densities.as_deref() {
        let Some(base_width) = args.base_width else {
            println!("imageoptimize: --densities requires --base-width");
            std::process::exit(1);
        };
        match parse_u32_list(s, "density", "1,2,3") {
            Ok(ds) => ds
                .into_iter()
                .map(|d| Variant::Density {
                    px: base_width.saturating_mul(d),
                    density: d,
                })
                .collect(),
            Err(e) => {
                println!("imageoptimize: --densities {e}");
                std::process::exit(1);
            }
        }
    } else {
        Vec::new()
    };
    let srcset_mode = !variants.is_empty();
    let density_mode = args.densities.is_some();
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
    let srcset_pattern = args.srcset_pattern.clone().unwrap_or_else(|| {
        if density_mode {
            "{name}@{x}x.{ext}".to_string()
        } else {
            "{name}-{w}w.{ext}".to_string()
        }
    });
    let emit_html = args.emit_html;
    let min_size_bytes = args.min_size.map(|kb| kb * 1024);
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

    //  glob patterns
    let formats = args
        .format
        .unwrap_or_else(|| vec![ImageFormat::Jpeg, ImageFormat::Jpg, ImageFormat::Png]);

    // sort and ded
    let mut extensions: Vec<&str> = formats
        .iter()
        .flat_map(|format| format.extensions())
        .collect();
    extensions.sort();
    extensions.dedup();

    let convert_formats = args.convert.unwrap_or_else(|| {
        vec![
            ConvertFormat::JpegAvif,
            ConvertFormat::JpegWebp,
            ConvertFormat::PngAvif,
            ConvertFormat::PngWebp,
        ]
    });
    let mut convert_extensions: HashMap<String, Vec<String>> = HashMap::new();
    for item in convert_formats.iter() {
        let (source, target) = match item {
            ConvertFormat::JpegAvif => (IMAGE_JPEG, IMAGE_AVIF),
            ConvertFormat::JpegWebp => (IMAGE_JPEG, IMAGE_WEBP),
            ConvertFormat::PngAvif => (IMAGE_PNG, IMAGE_AVIF),
            ConvertFormat::PngWebp => (IMAGE_PNG, IMAGE_WEBP),
            ConvertFormat::JpegJxl => (IMAGE_JPEG, IMAGE_JXL),
            ConvertFormat::PngJxl => (IMAGE_PNG, IMAGE_JXL),
            ConvertFormat::Disable => continue,
        };
        if let Some(targets) = convert_extensions.get_mut(source) {
            targets.push(target.to_string());
        } else {
            convert_extensions.insert(source.to_string(), vec![target.to_string()]);
        }
    }

    let mut image_optimize_params = vec![];
    for ext in extensions {
        let pattern = format!("{source}/**/*.{ext}");
        if !quiet {
            println!(
                "{}",
                format!(
                    "Searching pattern: {}",
                    LightCyan.paint(relative(&pattern, &base))
                )
            );
        }
        let entries = match glob(&pattern) {
            Ok(entries) => entries,
            Err(e) => {
                println!("{}", LightRed.paint(format!("Error reading path: {e}")));
                continue;
            }
        };

        for entry in entries {
            let path = match entry {
                Ok(path) => path,
                Err(e) => {
                    println!("{}", LightRed.paint(format!("Error reading path: {e}")));
                    continue;
                }
            };
            // min-size filter
            if let Some(min_bytes) = min_size_bytes {
                let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                if file_size < min_bytes {
                    continue;
                }
            }

            // exclude filter
            if !exclude_patterns.is_empty() {
                let path_str = path.to_string_lossy();
                if exclude_patterns.iter().any(|p| p.matches(&path_str)) {
                    continue;
                }
            }

            let ext = path.extension().unwrap_or_default();
            let ext = ext.to_str().unwrap_or_default();
            let image_type = match ext {
                "png" => IMAGE_PNG,
                _ => IMAGE_JPEG,
            };
            let file = path.to_string_lossy().to_string();
            let target = if source == output {
                path
            } else {
                Path::new(
                    &path
                        .to_string_lossy()
                        .replace(source.as_str(), output.as_str()),
                )
                .to_path_buf()
            };
            let mut targets = vec![];
            // Auto-format emits a single best-format output per source, so the fixed
            // conversion matrix is skipped — only the placeholder target is queued.
            if !auto_format_mode {
                if let Some(extensions) = convert_extensions.get(image_type) {
                    for item in extensions {
                        let new_target = target.clone().with_extension(item);
                        targets.push(new_target);
                    }
                }
            }
            targets.push(target);
            for target in targets {
                image_optimize_params.push(ImageOptimizeParams {
                    file: file.clone(),
                    target: target.to_string_lossy().into_owned(),
                });
            }
        }
    }

    // --lossless forces every per-format quality to 100. WebP (and JXL) treat >=100 as a
    // true lossless encode; AVIF/JPEG have no lossless mode so 100 is best-effort, and PNG
    // encodes at its top palette quality.
    let quality_of = |fixed: u8| if args.lossless { 100 } else { fixed };
    let qualities = ImageQualities {
        avif: quality_of(args.avif_quality),
        avif_speed: args.avif_speed,
        webp: quality_of(args.webp_quality),
        png: quality_of(args.png_quality),
        jpeg: quality_of(args.jpeg_quality),
        jxl: quality_of(args.jxl_quality),
        target_diff: args.target_diff,
    };

    let kb: usize = 1024;
    let mb = kb * 1024;

    // Total unique source images found (before incremental filter).
    let total_source_count = image_optimize_params
        .iter()
        .map(|i| i.file.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len();

    if total_source_count == 0 {
        println!("{}", LightYellow.paint("No images found."));
        return;
    }

    // --incremental: drop targets whose output is already newer than the source.
    let mut incremental_skipped = 0usize;
    if incremental && source != output {
        image_optimize_params.retain(|item| {
            let src_mtime = std::fs::metadata(&item.file)
                .and_then(|m| m.modified())
                .ok();
            let tgt_mtime = std::fs::metadata(&item.target)
                .and_then(|m| m.modified())
                .ok();
            match (src_mtime, tgt_mtime) {
                (Some(s), Some(t)) => t <= s,
                _ => true,
            }
        });
        let remaining: std::collections::HashSet<&str> = image_optimize_params
            .iter()
            .map(|i| i.file.as_str())
            .collect();
        incremental_skipped = total_source_count - remaining.len();
    }

    // Group every output target under its source file so each source is decoded once
    // and reused across all its output formats.
    let mut grouped: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
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
    // Other columns: fixed upper bounds.
    let pct_col = 4.max("PCT".len()); // "100%" = 4
    let diff_col = 6.max("DIFF".len()); // "(0.00)" = 6
    let size_col = 5.max("SIZE".len()); // "999kb" ≤ 5
    let dur_col = 6.max("TIME".len()); // "1000ms" = 6
    if !quiet {
        println!(
            "{:>pct_col$}  {:>diff_col$}  {:>size_col$}  {:>dur_col$}  FILE",
            "PCT", "DIFF", "SIZE", "TIME",
        );
        println!(
            "{}  {}  {}  {}  ----",
            "-".repeat(pct_col),
            "-".repeat(diff_col),
            "-".repeat(size_col),
            "-".repeat(dur_col),
        );
    }

    // One task per source file: decode once, then encode each output. The variant is set
    // for srcset/density outputs (None in normal mode). Each task returns all its outcomes.
    type TargetOutcome = (
        String,
        Option<Variant>,
        u128,
        Result<(usize, usize, f64, bool, bool, Option<String>)>,
    );
    let concurrency = args.threads.unwrap_or_else(num_cpus::get);
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let normal_flags = EncodeFlags {
        dry_run,
        strip_exif,
        no_diff,
        is_variant: false,
        auto_quality: args.auto_quality,
        auto_format: auto_format_mode,
    };
    let variant_flags = EncodeFlags {
        is_variant: true,
        // srcset variants are per-format derivatives; never auto-pick their format.
        auto_format: false,
        ..normal_flags
    };
    let mut join_set: JoinSet<(String, Vec<TargetOutcome>)> = JoinSet::new();
    for (file, targets) in grouped {
        let qualities = qualities.clone();
        let sem = semaphore.clone();
        let variants = variants.clone();
        let srcset_pattern = srcset_pattern.clone();
        join_set.spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let mut outcomes: Vec<TargetOutcome> = Vec::new();

            if variants.is_empty() {
                // Normal mode: decode + resize once, encode each target format.
                match load_base(&file, resize).await {
                    Ok(base) => {
                        let mut base = Some(base);
                        let last = targets.len().saturating_sub(1);
                        for (i, target) in targets.iter().enumerate() {
                            // Move the decoded image into the final encode; clone for the rest.
                            let b = if i == last {
                                base.take().unwrap()
                            } else {
                                base.as_ref().unwrap().clone()
                            };
                            let start = Instant::now();
                            let res =
                                encode_target(b, &file, target, &qualities, normal_flags).await;
                            outcomes.push((target.clone(), None, start.elapsed().as_millis(), res));
                        }
                    }
                    Err(e) => {
                        let message = format!("{e}");
                        for target in targets {
                            outcomes.push((
                                target,
                                None,
                                0,
                                Err(Error::Common {
                                    message: message.clone(),
                                }),
                            ));
                        }
                    }
                }
            } else {
                // srcset / density mode: decode once (no resize here), then variant × format.
                match load_base(&file, None).await {
                    Ok(base) => {
                        let src_w = base.get_size().0;
                        for &variant in &variants {
                            let w = variant.pixel_width();
                            if w >= src_w {
                                continue; // never upscale
                            }
                            // Resize the decoded image to this width once, reused for all formats.
                            let resized = match run_with_image(
                                base.clone(),
                                vec![vec!["resize".to_string(), w.to_string(), "0".to_string()]],
                            )
                            .await
                            {
                                Ok(r) => r,
                                Err(e) => {
                                    let message = format!("{e}");
                                    for target in &targets {
                                        let path = srcset_path(target, &srcset_pattern, variant);
                                        outcomes.push((
                                            path,
                                            Some(variant),
                                            0,
                                            Err(Error::Common {
                                                message: message.clone(),
                                            }),
                                        ));
                                    }
                                    continue;
                                }
                            };
                            let mut resized = Some(resized);
                            let last = targets.len().saturating_sub(1);
                            for (i, target) in targets.iter().enumerate() {
                                let b = if i == last {
                                    resized.take().unwrap()
                                } else {
                                    resized.as_ref().unwrap().clone()
                                };
                                let path = srcset_path(target, &srcset_pattern, variant);
                                let start = Instant::now();
                                let res =
                                    encode_target(b, &file, &path, &qualities, variant_flags).await;
                                outcomes.push((
                                    path,
                                    Some(variant),
                                    start.elapsed().as_millis(),
                                    res,
                                ));
                            }
                        }
                    }
                    Err(e) => {
                        let message = format!("{e}");
                        for target in &targets {
                            for &variant in &variants {
                                let path = srcset_path(target, &srcset_pattern, variant);
                                outcomes.push((
                                    path,
                                    Some(variant),
                                    0,
                                    Err(Error::Common {
                                        message: message.clone(),
                                    }),
                                ));
                            }
                        }
                    }
                }
            }
            (file, outcomes)
        });
    }

    let print_row =
        |target: &str,
         duration: u128,
         res: Result<(usize, usize, f64, bool, bool, Option<String>)>| {
            match res {
                Ok((size, original_size, diff, existed, skipped, _)) => {
                    if quiet {
                        return;
                    }
                    let duration_str = if duration < 1000 {
                        format!("{}ms", duration)
                    } else {
                        format!("{:.1}s", duration as f64 / 1000.0)
                    };
                    if skipped {
                        println!(
                            "{:>pct_col$}  {:>diff_col$}  {:>size_col$}  {:>dur_col$}  {} {}",
                            LightYellow.paint("SKIP"),
                            "",
                            "",
                            duration_str,
                            relative(target, &base),
                            LightYellow.paint("(-)"),
                        );
                        return;
                    }
                    let size_str = if size >= mb {
                        format!("{}mb", size / mb)
                    } else if size >= kb {
                        format!("{}kb", size / kb)
                    } else {
                        format!("{}b", size)
                    };
                    // diff < 0 means "not computed" (--no-diff, after a resize, or GIF).
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
                    let percent = size * 100 / original_size;
                    let status = if existed {
                        LightYellow.paint("(U)").to_string()
                    } else {
                        LightGreen.paint("(N)").to_string()
                    };
                    println!(
                        "{:>pct_col$}  ({diff_inner})  {:>size_col$}  {:>dur_col$}  {} {status}",
                        format!("{percent}%"),
                        size_str,
                        duration_str,
                        relative(target, &base),
                    );
                }
                Err(e) => {
                    println!(
                        "{}",
                        LightRed.paint(format!("{}: {e:?}", relative(target, &base)))
                    );
                }
            }
        };

    let mut summary_original: usize = 0;
    let mut summary_optimized: usize = 0;
    let mut summary_count: usize = 0;
    let mut summary_skipped: usize = 0;
    let mut summary_errors: usize = 0;
    let mut summary_variants: usize = 0;
    let mut summary_variant_bytes: usize = 0;
    // For --emit-html: source file -> (relative variant path, variant, ext).
    let mut html: std::collections::BTreeMap<String, Vec<(String, Variant, String)>> =
        std::collections::BTreeMap::new();

    while let Some(task) = join_set.join_next().await {
        let (file, outcomes) = task.expect("task panicked");
        let src_ext = Path::new(&file)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        for (target, width, duration, result) in outcomes {
            // Auto-format writes a different extension than the placeholder target carried;
            // use the encoder's actual output path for display and accounting.
            let effective_target = match &result {
                Ok((.., Some(actual))) => actual.clone(),
                _ => target,
            };
            let tgt_ext = Path::new(&effective_target)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string();
            match width {
                // Normal mode: only count same-format optimisation toward the savings
                // summary; avif/webp conversions are reported per row but not summed.
                // Auto-format always yields a single replacement output, so it counts too.
                None => {
                    if auto_format_mode || src_ext == tgt_ext {
                        match &result {
                            Ok((size, original_size, _, _, skipped, _)) => {
                                if *skipped {
                                    summary_skipped += 1;
                                } else {
                                    summary_original += original_size;
                                    summary_optimized += size;
                                    summary_count += 1;
                                }
                            }
                            Err(_) => summary_errors += 1,
                        }
                    }
                }
                // srcset/density variant: a derivative output, tallied separately from "saved".
                Some(variant) => match &result {
                    Ok((size, _, _, _, _, _)) => {
                        summary_variants += 1;
                        summary_variant_bytes += size;
                        if emit_html {
                            html.entry(file.clone()).or_default().push((
                                relative(&effective_target, &base),
                                variant,
                                tgt_ext.clone(),
                            ));
                        }
                    }
                    Err(_) => summary_errors += 1,
                },
            }
            print_row(&effective_target, duration, result);
        }
    }

    let format_size = |size: usize| {
        if size >= mb {
            format!("{:.1}mb", size as f64 / mb as f64)
        } else if size >= kb {
            format!("{:.1}kb", size as f64 / kb as f64)
        } else {
            format!("{}b", size)
        }
    };
    let error_note = if summary_errors > 0 {
        format!(", {} failed", LightRed.paint(format!("{summary_errors}")))
    } else {
        String::new()
    };

    if srcset_mode {
        if summary_variants > 0 || summary_errors > 0 {
            println!();
            let verb = if dry_run {
                "Would generate"
            } else {
                "Generated"
            };
            println!(
                "{}",
                LightCyan.paint(format!(
                    "{verb} {summary_variants} variant{} ({} total){error_note}",
                    if summary_variants == 1 { "" } else { "s" },
                    format_size(summary_variant_bytes),
                ))
            );
        }

        // One <source> srcset line per format, per source.
        if emit_html && !html.is_empty() {
            println!();
            for (file, variants) in html {
                println!(
                    "{}",
                    LightCyan.paint(format!("<!-- {} -->", relative(&file, &base)))
                );
                let mut by_ext: std::collections::BTreeMap<String, Vec<(String, Variant)>> =
                    std::collections::BTreeMap::new();
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
            }
        }
    } else if summary_count > 0 || summary_skipped > 0 || summary_errors > 0 {
        let saved = summary_original.saturating_sub(summary_optimized);
        let saved_pct = if summary_original > 0 {
            saved * 100 / summary_original
        } else {
            0
        };
        let skipped_note = if summary_skipped > 0 {
            format!(
                ", {} unchanged",
                LightYellow.paint(format!("{summary_skipped}"))
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
                "{verb} {summary_count} file{}: {} → {}, saved {} ({saved_pct}%){skipped_note}{error_note}",
                if summary_count == 1 { "" } else { "s" },
                format_size(summary_original),
                format_size(summary_optimized),
                LightGreen.paint(format_size(saved)),
            ))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_u32_list, srcset_path, Variant};

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
