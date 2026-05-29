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

fn parse_widths(s: &str) -> std::result::Result<Vec<u32>, String> {
    let mut widths = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let w = part
            .parse::<u32>()
            .map_err(|_| format!("invalid width '{part}'"))?;
        if w == 0 {
            return Err("widths must be greater than 0".to_string());
        }
        widths.push(w);
    }
    if widths.is_empty() {
        return Err("expected at least one width, e.g. 320,640,1280".to_string());
    }
    widths.sort_unstable();
    widths.dedup();
    Ok(widths)
}

/// Insert the width into a target path via the pattern (`{name}`, `{w}`, `{ext}`).
fn srcset_path(target: &str, pattern: &str, width: u32) -> String {
    let p = Path::new(target);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    let name = pattern
        .replace("{name}", stem)
        .replace("{w}", &width.to_string())
        .replace("{ext}", ext);
    p.with_file_name(name).to_string_lossy().into_owned()
}

static IMAGE_JPEG: &str = "jpeg";
static IMAGE_PNG: &str = "png";
static IMAGE_AVIF: &str = "avif";
static IMAGE_WEBP: &str = "webp";

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
        help = "Convert to format (jpeg-avif, jpeg-webp, png-avif, png-webp). Default: jpeg-avif, jpeg-webp, png-avif, png-webp"
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

    /// Generate one output per width for responsive images (srcset). Comma-separated,
    /// e.g. 320,640,1280. Widths >= the source width are skipped (no upscaling).
    /// When set, --resize is ignored.
    #[arg(long, value_name = "W1,W2,...")]
    widths: Option<String>,

    /// Filename pattern for width variants: {name} = stem, {w} = width, {ext} = extension
    #[arg(long, default_value = "{name}-{w}w.{ext}")]
    srcset_pattern: String,

    /// Print a ready-to-paste responsive <source srcset> snippet per source (with --widths)
    #[arg(long)]
    emit_html: bool,
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
}

/// Per-encode behaviour flags. `is_variant` marks a resized srcset derivative, which
/// always writes its output rather than falling back to keeping the original.
#[derive(Debug, Clone, Copy)]
struct EncodeFlags {
    dry_run: bool,
    strip_exif: bool,
    no_diff: bool,
    is_variant: bool,
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
async fn encode_target(
    base: ProcessImage,
    file: &str,
    target: &str,
    qualities: &ImageQualities,
    flags: EncodeFlags,
) -> Result<(usize, usize, f64, bool, bool)> {
    let output_type = target.split('.').next_back().unwrap_or_default();
    let quality = match output_type {
        "avif" => qualities.avif,
        "webp" => qualities.webp,
        "png" => qualities.png,
        _ => qualities.jpeg,
    };
    let speed = if output_type == "avif" {
        qualities.avif_speed
    } else {
        0
    };
    let mut tasks = vec![vec![
        "optim".to_string(),
        output_type.to_string(),
        quality.to_string(),
        speed.to_string(),
    ]];
    if !flags.no_diff {
        tasks.push(vec!["diff".to_string()]);
    }
    if flags.strip_exif {
        tasks.push(vec!["strip".to_string()]);
    }

    let img = run_with_image(base, tasks).await.context(OptimizeSnafu)?;

    let existed = fs::try_exists(target).await.unwrap_or(false);
    let buf = img.get_buffer().context(OptimizeSnafu)?;
    let size = buf.len();

    let src_ext = Path::new(file)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    // A width variant is a distinct derivative, never a replacement for the source,
    // so it always writes its encoded output (never falls back to the original).
    let same_format = src_ext == output_type;
    if !flags.is_variant && same_format && size >= img.original_size {
        if !flags.dry_run && file != target {
            if let Some(parent) = Path::new(target).parent() {
                fs::create_dir_all(parent).await.context(CreateDirSnafu)?;
            }
            let original = fs::read(file).await.context(WriteFileSnafu)?;
            let bytes_to_write = if flags.strip_exif {
                strip_exif_bytes(original, src_ext)
            } else {
                original
            };
            fs::write(target, bytes_to_write)
                .await
                .context(WriteFileSnafu)?;
        }
        return Ok((
            img.original_size,
            img.original_size,
            img.diff,
            existed,
            true,
        ));
    }

    if !flags.dry_run {
        if let Some(parent) = Path::new(target).parent() {
            fs::create_dir_all(parent).await.context(CreateDirSnafu)?;
        }
        fs::write(target, buf).await.context(WriteFileSnafu)?;
    }
    Ok((size, img.original_size, img.diff, existed, false))
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
    let widths = match args.widths.as_deref() {
        Some(s) => match parse_widths(s) {
            Ok(w) => w,
            Err(e) => {
                println!("imageoptimize: --widths {e}");
                std::process::exit(1);
            }
        },
        None => Vec::new(),
    };
    let srcset_mode = !widths.is_empty();
    let srcset_pattern = args.srcset_pattern.clone();
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
            if let Some(extensions) = convert_extensions.get(image_type) {
                for item in extensions {
                    let new_target = target.clone().with_extension(item);
                    targets.push(new_target);
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

    let qualities = ImageQualities {
        avif: args.avif_quality,
        avif_speed: args.avif_speed,
        webp: args.webp_quality,
        png: args.png_quality,
        jpeg: args.jpeg_quality,
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

    // One task per source file: decode once, then encode each output. `width` is set
    // for srcset variants (None in normal mode). Each task returns all its outcomes.
    type TargetOutcome = (
        String,
        Option<u32>,
        u128,
        Result<(usize, usize, f64, bool, bool)>,
    );
    let concurrency = args.threads.unwrap_or_else(num_cpus::get);
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let normal_flags = EncodeFlags {
        dry_run,
        strip_exif,
        no_diff,
        is_variant: false,
    };
    let variant_flags = EncodeFlags {
        is_variant: true,
        ..normal_flags
    };
    let mut join_set: JoinSet<(String, Vec<TargetOutcome>)> = JoinSet::new();
    for (file, targets) in grouped {
        let qualities = qualities.clone();
        let sem = semaphore.clone();
        let widths = widths.clone();
        let srcset_pattern = srcset_pattern.clone();
        join_set.spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let mut outcomes: Vec<TargetOutcome> = Vec::new();

            if widths.is_empty() {
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
                // srcset mode: decode once (no resize here), then width × format.
                match load_base(&file, None).await {
                    Ok(base) => {
                        let src_w = base.get_size().0;
                        for &w in &widths {
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
                                        let path = srcset_path(target, &srcset_pattern, w);
                                        outcomes.push((
                                            path,
                                            Some(w),
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
                                let path = srcset_path(target, &srcset_pattern, w);
                                let start = Instant::now();
                                let res =
                                    encode_target(b, &file, &path, &qualities, variant_flags).await;
                                outcomes.push((path, Some(w), start.elapsed().as_millis(), res));
                            }
                        }
                    }
                    Err(e) => {
                        let message = format!("{e}");
                        for target in &targets {
                            for &w in &widths {
                                let path = srcset_path(target, &srcset_pattern, w);
                                outcomes.push((
                                    path,
                                    Some(w),
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

    let print_row = |target: &str, duration: u128, res: Result<(usize, usize, f64, bool, bool)>| {
        match res {
            Ok((size, original_size, diff, existed, skipped)) => {
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
    // For --emit-html: source file -> (relative variant path, width, ext).
    let mut html: std::collections::BTreeMap<String, Vec<(String, u32, String)>> =
        std::collections::BTreeMap::new();

    while let Some(task) = join_set.join_next().await {
        let (file, outcomes) = task.expect("task panicked");
        let src_ext = Path::new(&file)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        for (target, width, duration, result) in outcomes {
            let tgt_ext = Path::new(&target)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string();
            match width {
                // Normal mode: only count same-format optimisation toward the savings
                // summary; avif/webp conversions are reported per row but not summed.
                None => {
                    if src_ext == tgt_ext {
                        match &result {
                            Ok((size, original_size, _, _, skipped)) => {
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
                // srcset variant: a derivative output, tallied separately from "saved".
                Some(w) => match &result {
                    Ok((size, _, _, _, _)) => {
                        summary_variants += 1;
                        summary_variant_bytes += size;
                        if emit_html {
                            html.entry(file.clone()).or_default().push((
                                relative(&target, &base),
                                w,
                                tgt_ext.clone(),
                            ));
                        }
                    }
                    Err(_) => summary_errors += 1,
                },
            }
            print_row(&target, duration, result);
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
                let mut by_ext: std::collections::BTreeMap<String, Vec<(String, u32)>> =
                    std::collections::BTreeMap::new();
                for (path, w, ext) in variants {
                    by_ext.entry(ext).or_default().push((path, w));
                }
                for (ext, mut list) in by_ext {
                    list.sort_by_key(|(_, w)| *w);
                    let srcset = list
                        .iter()
                        .map(|(p, w)| format!("{p} {w}w"))
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
