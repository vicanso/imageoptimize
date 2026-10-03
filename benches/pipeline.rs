//! Benchmarks for the stages a pipeline spends its time in: decoding, pixel transforms,
//! encoding, the auto-quality search and the perceptual diff.
//!
//! `cargo bench` runs everything; `cargo bench -- encode` runs one group, and
//! `cargo bench -- --save-baseline main` / `--baseline main` compare across changes.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use imageoptimize::{
    new_auto_quality_task, new_blur_task, new_diff_task, new_gray_task, new_optim_task,
    new_resize_task, new_thumbnail_task, run_with_image, ProcessImage,
};
use std::hint::black_box;
use std::io::Cursor;
use std::time::Duration;

const WIDTH: u32 = 1024;
const HEIGHT: u32 = 768;

/// A deterministic photo-like image: smooth gradients, some texture and fine noise, so the
/// encoders have realistic work to do without a large asset in the repository.
fn sample_rgb() -> image::RgbImage {
    image::RgbImage::from_fn(WIDTH, HEIGHT, |x, y| {
        let (fx, fy) = (x as f32 / WIDTH as f32, y as f32 / HEIGHT as f32);
        let texture = (fx * 24.0).sin() * (fy * 18.0).cos() * 24.0;
        // A cheap integer hash for per-pixel noise.
        let mut h = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA6B);
        h ^= h >> 15;
        h = h.wrapping_mul(0x2C1B_3C6D);
        h ^= h >> 12;
        let noise = (h & 0x0f) as f32 - 8.0;
        let channel = |base: f32| (base + texture + noise).clamp(0.0, 255.0) as u8;
        image::Rgb([
            channel(40.0 + 180.0 * fx),
            channel(60.0 + 140.0 * fy),
            channel(200.0 - 150.0 * fx * fy),
        ])
    })
}

fn run(image: ProcessImage, tasks: Vec<Vec<String>>) -> ProcessImage {
    tokio_test::block_on(run_with_image(image, tasks)).expect("pipeline failed")
}

/// The sample encoded as PNG: the lossless source every benchmark starts from.
fn sample_png() -> Vec<u8> {
    let mut png = Vec::new();
    image::DynamicImage::ImageRgb8(sample_rgb())
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("png encode");
    png
}

/// The sample decoded, without the snapshot that only the diff task needs.
fn sample_image() -> ProcessImage {
    ProcessImage::new_without_original(sample_png(), "png").expect("decode sample")
}

/// The sample re-encoded as `format`, as a decoder input.
fn sample_bytes(format: &str) -> Vec<u8> {
    run(sample_image(), vec![new_optim_task(format, 85, 0)])
        .get_buffer()
        .expect("encoded buffer")
        .into_owned()
}

/// Benchmark a pipeline over the decoded sample. The image is cloned outside the timing.
fn bench_tasks(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    name: &str,
    base: &ProcessImage,
    tasks: Vec<Vec<String>>,
) {
    group.bench_function(name, |b| {
        b.iter_batched(
            || (base.clone(), tasks.clone()),
            |(image, tasks)| black_box(run(image, tasks)),
            BatchSize::LargeInput,
        )
    });
}

fn decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("decode");
    let mut inputs = vec![("png", sample_png())];
    for format in ["jpeg", "webp", "avif"] {
        inputs.push((format, sample_bytes(format)));
    }
    #[cfg(feature = "jxl")]
    inputs.push(("jxl", sample_bytes("jxl")));
    for (format, bytes) in inputs {
        group.bench_function(format, |b| {
            b.iter_batched(
                || bytes.clone(),
                |bytes| black_box(ProcessImage::new_without_original(bytes, format).unwrap()),
                BatchSize::LargeInput,
            )
        });
    }
    group.finish();
}

fn transform(c: &mut Criterion) {
    let mut group = c.benchmark_group("transform");
    let base = sample_image();
    bench_tasks(
        &mut group,
        "resize_half",
        &base,
        vec![new_resize_task(WIDTH / 2, 0)],
    );
    bench_tasks(
        &mut group,
        "thumbnail_256",
        &base,
        vec![new_thumbnail_task(256, 256)],
    );
    bench_tasks(&mut group, "gray", &base, vec![new_gray_task()]);
    bench_tasks(&mut group, "blur_2", &base, vec![new_blur_task(2.0)]);
    group.finish();
}

fn encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("encode");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(8));
    let base = sample_image();
    let mut cases = vec![
        ("jpeg_q80", new_optim_task("jpeg", 80, 0)),
        ("webp_q80", new_optim_task("webp", 80, 0)),
        ("webp_lossless", new_optim_task("webp", 100, 0)),
        ("png_q90", new_optim_task("png", 90, 0)),
        ("png_lossless", new_optim_task("png", 100, 0)),
        ("avif_q80_speed4", new_optim_task("avif", 80, 4)),
        ("avif_q80_speed10", new_optim_task("avif", 80, 10)),
    ];
    if cfg!(feature = "jxl") {
        cases.push(("jxl_q80", new_optim_task("jxl", 80, 0)));
    }
    for (name, task) in cases {
        bench_tasks(&mut group, name, &base, vec![task]);
    }
    group.finish();
}

fn auto_quality(c: &mut Criterion) {
    let mut group = c.benchmark_group("auto_quality");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(15));
    let base = sample_image();
    for format in ["jpeg", "webp", "avif"] {
        // AVIF at the CLI's default speed: its search probes at the fastest preset.
        let speed = if format == "avif" { 4 } else { 0 };
        let task = new_auto_quality_task(format, speed, 1.0);
        bench_tasks(&mut group, format, &base, vec![task]);
    }
    group.finish();
}

fn diff(c: &mut Criterion) {
    let mut group = c.benchmark_group("diff");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(8));
    // The diff compares against the snapshot taken at load, so keep it here.
    let base = ProcessImage::new(sample_png(), "png").expect("decode sample");
    // Encode once outside the timing; the benchmark is the DSSIM comparison alone.
    let encoded = run(base, vec![new_optim_task("webp", 80, 0)]);
    bench_tasks(&mut group, "dssim", &encoded, vec![new_diff_task()]);
    group.finish();
}

criterion_group!(benches, decode, transform, encode, auto_quality, diff);
criterion_main!(benches);
