# image optimize

[中文](./README_zh.md) | English


Support multi process for image, such as: resize, gray, crop, flip, rotate, brighten, contrast, sharpen, blur, padding, watermark and optimize.

## Installation

### Shell script (Linux / macOS)

```bash
curl -fsSL https://raw.githubusercontent.com/vicanso/imageoptimize/main/install.sh | bash
```

Install a specific version or to a custom directory:

```bash
# Specific version
curl -fsSL https://raw.githubusercontent.com/vicanso/imageoptimize/main/install.sh | bash -s v0.4.3

# Custom install directory
curl -fsSL https://raw.githubusercontent.com/vicanso/imageoptimize/main/install.sh | INSTALL_DIR=~/.local/bin bash
```

### Pre-built binary

Download the archive for your platform from [GitHub Releases](https://github.com/vicanso/imageoptimize/releases), extract and place the binary in your `PATH`.

| Platform | Archive |
|----------|---------|
| macOS Apple Silicon | `imageoptimize-darwin-aarch64.tar.gz` |
| macOS Intel | `imageoptimize-darwin-x86_64.tar.gz` |
| Linux x86_64 (musl) | `imageoptimize-linux-musl-x86_64.tar.gz` |
| Linux aarch64 (musl) | `imageoptimize-linux-musl-aarch64.tar.gz` |
| Windows x86_64 | `imageoptimize-windows.exe.zip` |

### Build from source

```bash
cargo install imageoptimize --features bin
```

## Usage

```
imageoptimize [OPTIONS] <SOURCE>
```

`<SOURCE>` is the root directory to scan. The tool recurses through it and processes all matching images.

### Options

| Option | Default | Description |
|--------|---------|-------------|
| `-s, --source <DIR>` | — | Source directory (alternative to positional arg) |
| `--output <DIR>` | — | Output directory (required unless `--overwrite`) |
| `-o, --overwrite` | false | Write optimized files back to the source directory |
| `-f, --format <FMT>` | jpeg,jpg,png | Only process these formats (`jpeg`, `jpg`, `png`) |
| `--convert <CONV>` | all four | Format conversions to generate (`jpeg-avif`, `jpeg-webp`, `png-avif`, `png-webp`, `disable`) |
| `--jpeg-quality <N>` | 80 | JPEG encode quality (0–100) |
| `--png-quality <N>` | 90 | PNG encode quality (0–100) |
| `--avif-quality <N>` | 80 | AVIF encode quality (0–100) |
| `--webp-quality <N>` | 80 | WebP encode quality (0–99 lossy, ≥100 lossless) |
| `-t, --threads <N>` | CPU count | Number of parallel worker threads |
| `--dry-run` | false | Preview results without writing any files |
| `--min-size <KB>` | — | Skip files smaller than this size in KB |
| `--exclude <GLOB>` | — | Exclude files matching this glob pattern (repeatable) |
| `-q, --quiet` | false | Suppress per-file output; print only the final summary |
| `--resize <WxH>` | — | Resize images to fit within WxH before encoding; smaller images are untouched (e.g. `1920x1080`, `1920x0`) |
| `--strip-exif` | false | Strip EXIF metadata (including GPS) from output files without re-encoding |

### Examples

**Optimize in place** — overwrite originals with smaller files:

```bash
imageoptimize --overwrite /path/to/images
```

**Optimize to a separate directory** — keeps originals untouched and also generates AVIF/WebP variants:

```bash
imageoptimize /path/to/source --output /path/to/output
```

**Process only JPEG files**:

```bash
imageoptimize /path/to/source --output /path/to/output --format jpeg --format jpg
```

**Generate AVIF variants only** (skip WebP):

```bash
imageoptimize /path/to/source --output /path/to/output --convert jpeg-avif --convert png-avif
```

**Disable format conversion** (optimize originals only, no AVIF/WebP):

```bash
imageoptimize /path/to/source --output /path/to/output --convert disable
```

**Custom quality**:

```bash
imageoptimize /path/to/source --output /path/to/output \
  --jpeg-quality 75 --png-quality 85 --avif-quality 70 --webp-quality 75
```

**Dry run** — preview compression results without writing any files:

```bash
imageoptimize /path/to/source --output /path/to/output --dry-run
```

**Skip small files** — ignore files under 100 KB (too small to benefit from optimization):

```bash
imageoptimize /path/to/source --output /path/to/output --min-size 100
```

**Exclude paths** — skip thumbnail directories and files with a `-small` suffix:

```bash
imageoptimize /path/to/source --output /path/to/output \
  --exclude "**/thumb/**" \
  --exclude "*-small.*"
```

**Resize before encoding** — shrink anything wider than 1920 px or taller than 1080 px:

```bash
imageoptimize /path/to/source --output /path/to/output --resize 1920x1080
```

Use `0` to leave one dimension unconstrained (e.g. `--resize 1920x0` limits width only).

### Output

Before processing begins, the tool prints how many source images were found:

```
Found 12 images
```

Each processed file then prints a one-line summary:

```
 PCT    DIFF   SIZE    TIME  FILE
----  ------  -----  ------  ----
 69%  (0.59)   12kb    2.8s  asset/image/line_mixin.webp (N)
 55%  (0.54)   10kb    2.8s  asset/image/line_mixin.avif (N)
SKIP                   2.8s  asset/image/line_mixin.jpeg (-)
```

Fields: output path · file size · size relative to original · dssim perceptual difference · elapsed time.

The `DIFF` value is the [DSSIM](https://github.com/kornelski/dssim) score multiplied by 1000. Lower is better; `0.00` means visually lossless. Values above `1.00` are shown in yellow as a warning of noticeable quality loss.

- `(N)` — new file created
- `(U)` — existing file overwritten
- `(-)` — skipped (optimized size was not smaller than original; original is preserved)

A summary line is printed after all files are processed:

```
Optimized 12 files: 8.4mb → 5.1mb, saved 3.3mb (39%), 2 unchanged
```

If any files fail to process, the count is shown in red at the end of the summary: `, 1 failed`.

In `--dry-run` mode the header shows `[DRY RUN]` and the summary reads `Would optimize …`.

## Library API

The core pipeline is exposed as a Rust library. Each processing step is a task — a `Vec<String>` where the first element is the task name and the rest are arguments.

```rust
use imageoptimize::{run, new_load_task, new_resize_task, new_optim_task};

let result = run(vec![
    new_load_task("file:///path/to/image.jpg"),
    new_resize_task(800, 0),        // width=800, height proportional
    new_optim_task("webp", 80, 0),  // encode to WebP quality 80
]).await?;

let bytes = result.get_buffer()?;
```

### Available tasks

| Task | Helper | Arguments | Description |
|------|--------|-----------|-------------|
| `load` | `new_load_task(url)` | HTTP URL; `file:///abs/path`; or standard base64-encoded bytes | Load image; EXIF orientation is applied automatically |
| `resize` | `new_resize_task(w, h)` | width, height (0 = proportional) | Resize to exact dimensions |
| `fit` | `new_fit_task(max_w, max_h)` | max width, max height (0 = unconstrained) | Resize to fit within bounds, preserve aspect ratio; no-op if already smaller |
| `crop` | `new_crop_task(x, y, w, h)` | x, y, width, height | Crop region |
| `gray` | `new_gray_task()` | — | Convert to grayscale |
| `flip` | `new_flip_task(dir)` | `"h"` / `"horizontal"` or `"v"` / `"vertical"` | Flip image |
| `rotate` | `new_rotate_task(deg)` | `90`, `180`, `270` | Rotate (other values are no-ops) |
| `brighten` | `new_brighten_task(val)` | integer, positive brightens / negative darkens | Adjust brightness |
| `contrast` | `new_contrast_task(val)` | float, positive increases / negative decreases | Adjust contrast |
| `sharpen` | `new_sharpen_task(sigma, threshold)` | sigma (e.g. `1.0`), threshold (e.g. `0`) | USM sharpening |
| `blur` | `new_blur_task(sigma)` | sigma (e.g. `2.0`) | Gaussian blur |
| `strip` | `new_strip_task()` | — | Strip EXIF metadata from the encoded buffer without re-encoding (JPEG, PNG, WebP) |
| `padding` | `new_padding_task(w, h, color)` | width, height, hex color (`#rrggbb` / `#rrggbbaa`, default transparent) | Extend canvas, center image |
| `watermark` | `new_watermark_task(url, pos, ml, mt)` | url, position, margin-left, margin-top | Overlay watermark |
| `optim` | `new_optim_task(fmt, quality, speed)` | format (`jpeg`/`png`/`avif`/`webp`/`gif`), quality 0–100, speed | Encode & compress |
| `diff` | `new_diff_task()` | — | Compute DSSIM × 1000 score vs original; stored in `ProcessImage::diff` |

**`optim` speed parameter:**

| Format | Effect |
|--------|--------|
| `avif` | Encoder speed 0–10; lower = slower but smaller/better quality (default `0`) |
| `gif`  | Frame delay in centiseconds between frames when re-encoding animated GIFs |
| `jpeg` / `png` / `webp` | Ignored |

Watermark positions: `leftTop`, `top`, `rightTop`, `left`, `center`, `right`, `leftBottom`, `bottom`, `rightBottom` (default).

## License

This project is licensed under the [Apache License 2.0 license].

[Apache License 2.0 license]: https://github.com/vicanso/imageoptimize/blob/main/LICENSE
