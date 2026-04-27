# image optimize

Support multi process for image, such as: resize, gray, crop, watermark and optimize.

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

### Output

Each processed file prints a one-line summary:

```
 PCT    DIFF   SIZE    TIME  FILE
----  ------  -----  ------  ----
 69%  (0.59)   12kb    2.8s  asset/image/line_mixin.webp (N)
 55%  (0.54)   10kb    2.8s  asset/image/line_mixin.avif (N)
SKIP                   2.8s  asset/image/line_mixin.jpeg (-)
```

Fields: output path · file size · size relative to original · dssim perceptual difference · elapsed time.

## License

This project is licensed under the [Apache License 2.0 license].

[Apache License 2.0 license]: https://github.com/vicanso/imageoptimize/blob/main/LICENSE
