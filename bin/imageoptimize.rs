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
use glob::glob;
use imageoptimize::{run, ImageProcessingError};
use nu_ansi_term::Color::{LightCyan, LightGreen, LightRed, LightYellow};
use snafu::{ResultExt, Snafu};
use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;
use tokio::fs;

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
) -> Result<(usize, usize, f64)> {
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

    let img = run(vec![load_task, optim_task, diff_task])
        .await
        .context(OptimizeSnafu)?;

    if let Some(parent) = Path::new(&target).parent() {
        fs::create_dir_all(parent).await.context(CreateDirSnafu)?;
    }
    let buf = img.get_buffer().context(OptimizeSnafu)?;
    let size = buf.len();
    fs::write(target, buf).await.context(WriteFileSnafu)?;

    Ok((size, img.original_size, img.diff))
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
        println!(
            "{}",
            format!("Searching pattern: {}", LightCyan.paint(pattern.clone()))
        );
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
        webp: 0,
        png: args.png_quality,
        jpeg: args.jpeg_quality,
    };
    let kb = 1024;
    let mb = kb * 1024;
    for item in image_optimize_params.iter() {
        let start = Instant::now();
        match optimize_image(item, qualities.clone()).await {
            Ok((size, original_size, diff)) => {
                let diff_str = format!("{:.2}", diff);
                let diff_text = if diff > 1.0 {
                    LightYellow.paint(diff_str)
                } else {
                    LightGreen.paint(diff_str)
                };
                let size_str = if size >= mb {
                    format!("{}mb", size / mb)
                } else if size >= kb {
                    format!("{}kb", size / kb)
                } else {
                    format!("{}b", size)
                };
                let percent = size * 100 / original_size;
                let duration = start.elapsed().as_millis();
                let duration_str = if duration < 1000 {
                    format!("{}ms", duration)
                } else {
                    format!("{}s", duration / 1000)
                };
                println!(
                    "{}: {size_str} {percent}%({diff_text}) {duration_str}",
                    item.target.clone(),
                );
            }
            Err(e) => {
                println!("{}", LightRed.paint(format!("{}: {e:?}", &item.file)));
            }
        }
    }
}
