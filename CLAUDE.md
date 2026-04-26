# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
make test       # cargo test
make lint       # cargo clippy
make lint-fix   # cargo clippy --fix --allow-staged
make fmt        # cargo fmt --all --
make hooks      # install git pre-commit hook (runs fmt + lint before each commit)
```

To run a single test:
```bash
cargo test test_name
```

The CLI binary requires the `bin` feature:
```bash
cargo build --features bin
```

## Architecture

`imageoptimize` is a Rust image processing library with an optional CLI. The core abstraction is an **async task pipeline**: callers pass a `Vec<Vec<String>>` of named operations and the library executes them in sequence, threading a `ProcessImage` state struct through each step.

### Source layout

- `src/image_processing.rs` — pipeline engine and all `Process` trait implementations
- `src/images.rs` — per-format encoders/decoders (JPEG via mozjpeg, PNG via imagequant+lodepng, AVIF, WebP, GIF)
- `src/lib.rs` — re-exports only
- `bin/imageoptimize.rs` — CLI (clap, glob, batch processing); compiled only with `--features bin`

### Pipeline pattern

`ProcessImage` carries `original` (RGBA snapshot), `di` (current `DynamicImage`), `buffer` (encoded bytes), `diff` (dssim score), and `ext` (output format).

Each processor implements `Process` (an async trait) and transforms `ProcessImage → Result<ProcessImage>`. The `run()` / `run_with_image()` functions dispatch task names to the right processor:

| Task | Processor |
|------|-----------|
| `load` | `LoaderProcess` — HTTP or `file://` URL |
| `resize` | `ResizeProcess` |
| `gray` | `GrayProcess` |
| `crop` | `CropProcess` |
| `watermark` | `WatermarkProcess` |
| `optim` | `OptimProcess` — encode to target format + quality |
| `diff` | inline dssim comparison against `original` |

Task args are positional strings, e.g. `["optim", "webp", "80", "0"]` (format, quality, speed).

### Image encoding

Each format in `images.rs` converts an `ImageInfo` (normalized RGBA buffer) to bytes. PNG uses `imagequant` for palette quantization. JPEG uses `mozjpeg`. AVIF has a custom decoder (`avif_decode`) for broader compatibility. Quality comparison uses `dssim-core`.

### Feature flags

```toml
[features]
default = []
bin = ["clap", "tokio", "glob", "nu-ansi-term"]
```

The library itself has no async runtime dependency; `tokio` is only pulled in for the CLI.
