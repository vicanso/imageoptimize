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
| Linux x86_64 | `imageoptimize-linux-x86_64.tar.gz` |
| Linux aarch64 | `imageoptimize-linux-aarch64.tar.gz` |
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
| `--avif-speed <N>` | 4 | AVIF encoder speed (0 = slowest/best quality, 10 = fastest/lower quality) |
| `--incremental` | false | Skip images whose every output file is already newer than the source; only applies with `--output` |
| `--no-diff` | false | Skip the DSSIM diff metric; avoids re-decoding AVIF/JXL output just to score it (DIFF column shows `—`) |
| `--widths <W1,W2,...>` | — | Generate one output per width for responsive `srcset` (e.g. `320,640,1280`). Widths ≥ the source width are skipped (no upscaling); `--resize` is ignored when set |
| `--srcset-pattern <PAT>` | `{name}-{w}w.{ext}` | Filename pattern for width variants (`{name}` = stem, `{w}` = width, `{ext}` = extension) |
| `--emit-html` | false | Print a ready-to-paste `<source srcset>` snippet per source (with `--widths`) |
| `--auto-quality` | false | Auto-tune quality per output: binary-search the lowest quality whose perceptual diff stays within `--target-diff`. Overrides the per-format quality flags |
| `--auto-format` | false | Auto-pick the output format: encode each source once as the smallest of webp/avif plus a lossless fallback (png if it has transparency, else jpeg), each quality-tuned to `--target-diff`. One output per source; ignores `--convert`, and is ignored under `--widths` |
| `--target-diff <N>` | 1.0 | Perceptual-diff target (DSSIM ×1000) for `--auto-quality` / `--auto-format`; lower = higher fidelity, `1.0` ≈ visually lossless |

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

**Responsive images (srcset)** — generate multiple widths per source, each in every output format, and print the HTML snippet:

```bash
imageoptimize /path/to/source --output /path/to/output --widths 320,640,1280 --emit-html
```

Produces e.g. `photo-320w.avif`, `photo-640w.avif`, … (widths larger than the source are skipped). The `--emit-html` snippet is ready to drop into a `<picture>` element:

```html
<picture>
  <source type="image/avif" srcset="photo-320w.avif 320w, photo-640w.avif 640w, photo-1280w.avif 1280w">
  <source type="image/webp" srcset="photo-320w.webp 320w, photo-640w.webp 640w, photo-1280w.webp 1280w">
  <img src="photo-1280w.jpeg" sizes="(max-width: 640px) 100vw, 640px" alt="">
</picture>
```

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
| `hue` | `new_hue_task(shift)` | integer degrees, wraps around (e.g. `90`, `-45`) | Rotate hue of every pixel |
| `saturate` | `new_saturate_task(factor)` | float (0.0 = grayscale, 1.0 = unchanged, >1.0 = boost) | Scale saturation of every pixel |
| `thumbnail` | `new_thumbnail_task(w, h)` | width, height | Scale to cover `w×h` (fill mode) then center-crop; unlike `fit` this never letterboxes |
| `thumbnail` (smart) | `new_smart_thumbnail_task(w, h)` | width, height | Like `thumbnail`, but content-aware: the crop slides to the highest-detail region (luminance-gradient energy + center bias) instead of centering |
| `invert` | `new_invert_task()` | — | Invert RGB channels; alpha is preserved |
| `opacity` | `new_opacity_task(factor)` | float (0.0 = transparent, 1.0 = unchanged) | Multiply every pixel's alpha by factor |
| `gamma` | `new_gamma_task(gamma)` | float (1.0 = unchanged, <1.0 = brighten, >1.0 = darken) | Gamma correction: `output = (input/255)^gamma × 255`; alpha unaffected |
| `background` | `new_background_task(color)` | hex color (`#rrggbb` / `#rrggbbaa`, empty = opaque white) | Flatten transparency by compositing over a solid background; use before encoding to JPEG/JXL so transparent areas don't turn black |
| `normalize` | `new_normalize_task(per_channel)` | bool (`true` = per-channel RGB, `false` = luminance) | Auto-contrast: stretch the histogram to the full 0–255 range |
| `trim` | `new_trim_task(tolerance)` | tolerance 0–255 (max per-channel RGBA difference from the top-left reference color) | Auto-crop a uniform border |
| `strip` | `new_strip_task()` | — | Strip EXIF metadata from the encoded buffer without re-encoding (JPEG, PNG, WebP) |
| `padding` | `new_padding_task(w, h, color)` | width, height, hex color (`#rrggbb` / `#rrggbbaa`, default transparent) | Extend canvas, center image |
| `watermark` | `new_watermark_task(url, pos, ml, mt)` | url, position, margin-left, margin-top | Overlay watermark |
| `optim` | `new_optim_task(fmt, quality, speed)` | format (`jpeg`/`png`/`avif`/`webp`/`gif`/`jxl`), quality 0–100, speed | Encode & compress |
| `optim` (auto-quality) | `new_auto_quality_task(fmt, speed, target)` | format, speed, target DSSIM ×1000 | Binary-search the lowest quality whose perceptual diff stays within `target` |
| `optim` (auto-format) | `new_auto_format_task(quality, speed, target)` | quality 0–100, speed, target DSSIM ×1000 | Encode candidate formats (alpha-aware: webp/avif/png or webp/avif/jpeg) and keep the smallest within `target` |
| `optim` (full auto) | `new_auto_task(speed, target)` | speed, target DSSIM ×1000 | Search both format and quality for the smallest output within `target` |
| `diff` | `new_diff_task()` | — | Compute DSSIM × 1000 score vs original; stored in `ProcessImage::diff` |

**`optim` speed parameter:**

| Format | Effect |
|--------|--------|
| `avif` | Encoder speed 0–10; lower = slower but smaller/better quality (default `0`) |
| `gif`  | Frame delay in centiseconds between frames when re-encoding animated GIFs |
| `jxl`  | Ignored (encoder effort is fixed) |
| `jpeg` / `png` / `webp` | Ignored |

Watermark positions: `leftTop`, `top`, `rightTop`, `left`, `center`, `right`, `leftBottom`, `bottom`, `rightBottom` (default).

## License

This project is licensed under the [Apache License 2.0 license].

[Apache License 2.0 license]: https://github.com/vicanso/imageoptimize/blob/main/LICENSE
