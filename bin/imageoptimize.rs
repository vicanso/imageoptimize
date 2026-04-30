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
use imageoptimize::{run, strip_exif_bytes, ImageProcessingError};
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
    webp: u8,
    png: u8,
    jpeg: u8,
}

async fn optimize_image(
    item: &ImageOptimizeParams,
    qualities: ImageQualities,
    dry_run: bool,
    resize: Option<(u32, u32)>,
    strip_exif: bool,
) -> Result<(usize, usize, f64, bool, bool)> {
    let load_task = vec!["load".to_string(), format!("file://{}", &item.file)];
    let target = item.target.clone();
    let output_type = target.split('.').next_back().unwrap_or_default();
    let quality = match output_type {
        "avif" => qualities.avif,
        "webp" => qualities.webp,
        "png" => qualities.png,
        _ => qualities.jpeg,
    };
    let optim_task = vec![
        "optim".to_string(),
        output_type.to_string(),
        quality.to_string(),
        "0".to_string(),
    ];
    let diff_task = vec!["diff".to_string()];

    let mut tasks = vec![load_task];
    if let Some((max_w, max_h)) = resize {
        tasks.push(vec![
            "resize".to_string(),
            max_w.to_string(),
            max_h.to_string(),
            "fit".to_string(),
        ]);
    }
    tasks.push(optim_task);
    tasks.push(diff_task);
    if strip_exif {
        tasks.push(vec!["strip".to_string()]);
    }

    let img = run(tasks).await.context(OptimizeSnafu)?;

    let existed = fs::try_exists(&target).await.unwrap_or(false);
    let buf = img.get_buffer().context(OptimizeSnafu)?;
    let size = buf.len();

    let src_ext = Path::new(&item.file)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let same_format = src_ext == output_type;
    if same_format && size >= img.original_size {
        if !dry_run && item.file != item.target {
            if let Some(parent) = Path::new(&target).parent() {
                fs::create_dir_all(parent).await.context(CreateDirSnafu)?;
            }
            let original = fs::read(&item.file).await.context(WriteFileSnafu)?;
            let bytes_to_write = if strip_exif {
                strip_exif_bytes(original, src_ext)
            } else {
                original
            };
            fs::write(&target, bytes_to_write)
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

    if !dry_run {
        if let Some(parent) = Path::new(&target).parent() {
            fs::create_dir_all(parent).await.context(CreateDirSnafu)?;
        }
        fs::write(&target, buf).await.context(WriteFileSnafu)?;
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
        webp: args.webp_quality,
        png: args.png_quality,
        jpeg: args.jpeg_quality,
    };

    let kb: usize = 1024;
    let mb = kb * 1024;

    // Build group metadata before consuming image_optimize_params.
    // expected: how many targets each source file has
    // target_to_file: maps each target path back to its source file
    let mut expected: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut target_to_file: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for item in &image_optimize_params {
        *expected.entry(item.file.clone()).or_insert(0) += 1;
        target_to_file.insert(item.target.clone(), item.file.clone());
    }

    let source_count = expected.len();
    if source_count == 0 {
        println!("{}", LightYellow.paint("No images found."));
        return;
    }
    println!(
        "Found {} image{}",
        LightCyan.paint(format!("{source_count}")),
        if source_count == 1 { "" } else { "s" },
    );

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

    // Spawn all tasks in parallel — no change from original behaviour.
    let concurrency = args.threads.unwrap_or_else(num_cpus::get);
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let mut join_set: JoinSet<(
        String,
        String,
        Instant,
        Result<(usize, usize, f64, bool, bool)>,
    )> = JoinSet::new();
    for item in image_optimize_params {
        let qualities = qualities.clone();
        let sem = semaphore.clone();
        join_set.spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let start = Instant::now();
            let result = optimize_image(&item, qualities, dry_run, resize, strip_exif).await;
            (item.target, item.file, start, result)
        });
    }

    // Collect results into per-source buffers; flush immediately when a group is complete.
    type TaskResult = (
        String,
        String,
        Instant,
        Result<(usize, usize, f64, bool, bool)>,
    );
    let mut buffers: std::collections::HashMap<String, Vec<TaskResult>> =
        std::collections::HashMap::new();

    let print_row = |target: &str,
                     _file: &str,
                     start: Instant,
                     res: Result<(usize, usize, f64, bool, bool)>| {
        match res {
            Ok((size, original_size, diff, existed, skipped)) => {
                if quiet {
                    return;
                }
                let duration = start.elapsed().as_millis();
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
                let diff_num = format!("{diff:>4.2}");
                let diff_inner = if diff > 1.0 {
                    LightYellow.paint(&diff_num).to_string()
                } else {
                    LightGreen.paint(&diff_num).to_string()
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

    while let Some(task) = join_set.join_next().await {
        let (target, file, start, result) = task.expect("task panicked");
        let source = target_to_file[&target].clone();
        buffers
            .entry(source.clone())
            .or_default()
            .push((target, file, start, result));

        // Flush immediately once all variants of this source file have arrived.
        if buffers[&source].len() == expected[&source] {
            for (t, f, s, r) in buffers.remove(&source).unwrap() {
                let src_ext = Path::new(&f)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("");
                let tgt_ext = Path::new(&t)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("");
                if src_ext == tgt_ext {
                    match &r {
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
                print_row(&t, &f, s, r);
            }
        }
    }

    if summary_count > 0 || summary_skipped > 0 || summary_errors > 0 {
        let format_size = |size: usize| {
            if size >= mb {
                format!("{:.1}mb", size as f64 / mb as f64)
            } else if size >= kb {
                format!("{:.1}kb", size as f64 / kb as f64)
            } else {
                format!("{}b", size)
            }
        };
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
        let error_note = if summary_errors > 0 {
            format!(", {} failed", LightRed.paint(format!("{summary_errors}")))
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
