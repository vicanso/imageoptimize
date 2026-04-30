# image optimize

支持多种图片处理操作，包括：缩放、灰度、裁剪、翻转、旋转、亮度、对比度、锐化、模糊、填充、水印和优化。

## 安装

### Shell 脚本（Linux / macOS）

```bash
curl -fsSL https://raw.githubusercontent.com/vicanso/imageoptimize/main/install.sh | bash
```

安装指定版本或自定义安装目录：

```bash
# 指定版本
curl -fsSL https://raw.githubusercontent.com/vicanso/imageoptimize/main/install.sh | bash -s v0.4.3

# 自定义安装目录
curl -fsSL https://raw.githubusercontent.com/vicanso/imageoptimize/main/install.sh | INSTALL_DIR=~/.local/bin bash
```

### 预构建二进制

从 [GitHub Releases](https://github.com/vicanso/imageoptimize/releases) 下载对应平台的压缩包，解压后将二进制文件放入 `PATH`。

| 平台 | 压缩包 |
|------|--------|
| macOS Apple Silicon | `imageoptimize-darwin-aarch64.tar.gz` |
| macOS Intel | `imageoptimize-darwin-x86_64.tar.gz` |
| Linux x86_64 (musl) | `imageoptimize-linux-musl-x86_64.tar.gz` |
| Linux aarch64 (musl) | `imageoptimize-linux-musl-aarch64.tar.gz` |
| Windows x86_64 | `imageoptimize-windows.exe.zip` |

### 从源码构建

```bash
cargo install imageoptimize --features bin
```

## 使用方法

```
imageoptimize [OPTIONS] <SOURCE>
```

`<SOURCE>` 为扫描的根目录，工具会递归遍历并处理所有匹配的图片文件。

### 选项

| 选项 | 默认值 | 说明 |
|------|--------|------|
| `-s, --source <DIR>` | — | 源目录（位置参数的替代写法） |
| `--output <DIR>` | — | 输出目录（与 `--overwrite` 二选一，必填其一） |
| `-o, --overwrite` | false | 将优化后的文件写回源目录 |
| `-f, --format <FMT>` | jpeg,jpg,png | 仅处理指定格式（`jpeg`、`jpg`、`png`） |
| `--convert <CONV>` | 全部四种 | 生成的格式转换类型（`jpeg-avif`、`jpeg-webp`、`png-avif`、`png-webp`、`disable`） |
| `--jpeg-quality <N>` | 80 | JPEG 编码质量（0–100） |
| `--png-quality <N>` | 90 | PNG 编码质量（0–100） |
| `--avif-quality <N>` | 80 | AVIF 编码质量（0–100） |
| `--webp-quality <N>` | 80 | WebP 编码质量（0–99 有损，≥100 无损） |
| `-t, --threads <N>` | CPU 核心数 | 并行工作线程数 |
| `--dry-run` | false | 预览结果但不写入任何文件 |
| `--min-size <KB>` | — | 跳过小于该大小（KB）的文件 |
| `--exclude <GLOB>` | — | 排除匹配该 glob 模式的文件（可重复使用） |
| `-q, --quiet` | false | 仅输出最终汇总，不打印每个文件的处理结果 |
| `--resize <WxH>` | — | 编码前将超出尺寸的图片缩放至指定范围内，小图不受影响（如 `1920x1080`、`1920x0`） |
| `--strip-exif` | false | 从输出文件中移除 EXIF 元数据（含 GPS 定位），无需重新编码 |

### 示例

**原地优化** — 用更小的文件覆盖原文件：

```bash
imageoptimize --overwrite /path/to/images
```

**输出到独立目录** — 保留原文件不变，同时生成 AVIF/WebP 变体：

```bash
imageoptimize /path/to/source --output /path/to/output
```

**仅处理 JPEG 文件**：

```bash
imageoptimize /path/to/source --output /path/to/output --format jpeg --format jpg
```

**仅生成 AVIF 变体**（跳过 WebP）：

```bash
imageoptimize /path/to/source --output /path/to/output --convert jpeg-avif --convert png-avif
```

**禁用格式转换**（仅优化原始格式，不生成 AVIF/WebP）：

```bash
imageoptimize /path/to/source --output /path/to/output --convert disable
```

**自定义质量**：

```bash
imageoptimize /path/to/source --output /path/to/output \
  --jpeg-quality 75 --png-quality 85 --avif-quality 70 --webp-quality 75
```

**试运行** — 预览压缩结果，不写入任何文件：

```bash
imageoptimize /path/to/source --output /path/to/output --dry-run
```

**跳过小文件** — 忽略小于 100 KB 的文件（太小，优化收益不明显）：

```bash
imageoptimize /path/to/source --output /path/to/output --min-size 100
```

**排除路径** — 跳过缩略图目录和带有 `-small` 后缀的文件：

```bash
imageoptimize /path/to/source --output /path/to/output \
  --exclude "**/thumb/**" \
  --exclude "*-small.*"
```

**编码前缩放** — 将宽度超过 1920 px 或高度超过 1080 px 的图片缩小至目标尺寸：

```bash
imageoptimize /path/to/source --output /path/to/output --resize 1920x1080
```

使用 `0` 表示该方向不限制（如 `--resize 1920x0` 仅限制宽度）。

### 输出格式

处理开始前，工具会先输出找到的源图片数量：

```
Found 12 images
```

随后每个处理完的文件输出一行摘要：

```
 PCT    DIFF   SIZE    TIME  FILE
----  ------  -----  ------  ----
 69%  (0.59)   12kb    2.8s  asset/image/line_mixin.webp (N)
 55%  (0.54)   10kb    2.8s  asset/image/line_mixin.avif (N)
SKIP                   2.8s  asset/image/line_mixin.jpeg (-)
```

字段含义：输出路径 · 文件大小 · 相对原文件的大小比例 · dssim 感知差异评分 · 耗时。

`DIFF` 列的值为 [DSSIM](https://github.com/kornelski/dssim) 评分乘以 1000。数值越低越好，`0.00` 表示视觉上无损。超过 `1.00` 时以黄色显示，提示质量损失较明显。

- `(N)` — 新建文件
- `(U)` — 覆盖已有文件
- `(-)` — 跳过（优化后大小不小于原文件，保留原文件不变）

所有文件处理完毕后输出汇总行：

```
Optimized 12 files: 8.4mb → 5.1mb, saved 3.3mb (39%), 2 unchanged
```

如有文件处理失败，汇总行末尾会以红色显示失败数量，如 `, 1 failed`。

`--dry-run` 模式下，标题显示 `[DRY RUN]`，汇总行读作 `Would optimize …`。

## 库 API

核心处理流水线以 Rust 库的形式对外开放。每个处理步骤是一个任务——`Vec<String>`，其中第一个元素是任务名称，其余为参数。

```rust
use imageoptimize::{run, new_load_task, new_resize_task, new_optim_task};

let result = run(vec![
    new_load_task("file:///path/to/image.jpg"),
    new_resize_task(800, 0),        // 宽度=800，高度等比缩放
    new_optim_task("webp", 80, 0),  // 编码为 WebP，质量 80
]).await?;

let bytes = result.get_buffer()?;
```

### 可用任务

| 任务 | 辅助函数 | 参数 | 说明 |
|------|----------|------|------|
| `load` | `new_load_task(url)` | HTTP URL；`file:///绝对路径`；或标准 base64 编码的图片字节 | 加载图片；自动根据 EXIF 方向信息旋转/翻转 |
| `resize` | `new_resize_task(w, h)` | 宽度、高度（0 表示等比缩放） | 缩放至精确尺寸 |
| `fit` | `new_fit_task(max_w, max_h)` | 最大宽度、最大高度（0 表示不限制） | 等比缩放至目标范围内，已满足则跳过 |
| `crop` | `new_crop_task(x, y, w, h)` | x、y、宽度、高度 | 裁剪区域 |
| `gray` | `new_gray_task()` | — | 转为灰度图 |
| `flip` | `new_flip_task(dir)` | `"h"` / `"horizontal"` 或 `"v"` / `"vertical"` | 翻转图片 |
| `rotate` | `new_rotate_task(deg)` | `90`、`180`、`270` | 旋转（其他值无效果） |
| `brighten` | `new_brighten_task(val)` | 整数，正值增亮 / 负值变暗 | 调整亮度 |
| `contrast` | `new_contrast_task(val)` | 浮点数，正值增强 / 负值减弱 | 调整对比度 |
| `sharpen` | `new_sharpen_task(sigma, threshold)` | sigma（如 `1.0`）、threshold（如 `0`） | USM 锐化 |
| `blur` | `new_blur_task(sigma)` | sigma（如 `2.0`） | 高斯模糊 |
| `strip` | `new_strip_task()` | — | 从编码后的缓冲区移除 EXIF 元数据，无需重新编码（支持 JPEG、PNG、WebP） |
| `padding` | `new_padding_task(w, h, color)` | 宽度、高度、十六进制颜色（`#rrggbb` / `#rrggbbaa`，默认透明） | 扩展画布并居中图片 |
| `watermark` | `new_watermark_task(url, pos, ml, mt)` | url、位置、左边距、上边距 | 叠加水印 |
| `optim` | `new_optim_task(fmt, quality, speed)` | 格式（`jpeg`/`png`/`avif`/`webp`/`gif`）、质量 0–100、速度 | 编码并压缩 |
| `diff` | `new_diff_task()` | — | 计算 DSSIM × 1000 评分并存入 `ProcessImage::diff` |

**`optim` speed 参数说明：**

| 格式 | 作用 |
|------|------|
| `avif` | 编码速度 0–10，值越小速度越慢但压缩率/质量越好（默认 `0`） |
| `gif`  | 重新编码动态 GIF 时帧之间的延迟，单位为百分之一秒 |
| `jpeg` / `png` / `webp` | 忽略此参数 |

水印位置：`leftTop`、`top`、`rightTop`、`left`、`center`、`right`、`leftBottom`、`bottom`、`rightBottom`（默认）。

## 许可证

本项目基于 [Apache License 2.0 许可证]。

[Apache License 2.0 许可证]: https://github.com/vicanso/imageoptimize/blob/main/LICENSE
