# image optimize

[English](./README.md) | 中文

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
| Linux x86_64 | `imageoptimize-linux-x86_64.tar.gz` |
| Linux aarch64 | `imageoptimize-linux-aarch64.tar.gz` |
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
| `--avif-speed <N>` | 4 | AVIF 编码速度（0 = 最慢/最佳质量，10 = 最快/较低质量） |
| `--incremental` | false | 跳过所有输出文件均比源文件新的图片（仅适用于 `--output` 模式） |
| `--no-diff` | false | 跳过 DSSIM 评分；避免为算分而二次解码 AVIF/JXL（DIFF 列显示 `—`） |
| `--widths <W1,W2,...>` | — | 按宽度生成响应式 `srcset` 的 `Nw` 描述符（流式图，如 `320,640,1280`）。宽度 ≥ 源宽的会被跳过（不放大）；设置后忽略 `--resize`。与 `--densities` 互斥 |
| `--densities <D1,D2,...>` | — | 按像素密度生成 `srcset` 的 `Nx` 描述符（固定尺寸图，如 `1,2,3`）。需配合 `--base-width`，每份输出为 base-width × 倍率 像素；宽度 ≥ 源宽的倍率会被跳过。与 `--widths` 互斥 |
| `--base-width <W>` | — | `--densities` 的 1× 显示宽度（CSS px），输出尺寸 = base-width × 倍率 |
| `--srcset-pattern <PAT>` | `{name}-{w}w.{ext}` / `{name}@{x}x.{ext}` | 变体文件名模板（`{name}` = 主名，`{w}` = 像素宽度，`{x}` = 倍率，`{ext}` = 扩展名）。默认按宽度/密度模式自动选择 |
| `--emit-html` | false | 为每个源图打印可直接粘贴的 `<source srcset>` 片段（需配合 `--widths` 或 `--densities`） |
| `--auto-quality` | false | 按输出自动调质量：二分搜索使感知差异保持在 `--target-diff` 内的最低质量；会覆盖各格式的质量参数 |
| `--auto-format` | false | 自动选格式：每个源图只输出一份，取 webp/avif 与无损兜底（含透明用 png，否则 jpeg）中体积最小者，各候选按 `--target-diff` 调质量；忽略 `--convert`，在 `--widths` 下不生效 |
| `--target-diff <N>` | 1.0 | `--auto-quality` / `--auto-format` 的感知差异目标（DSSIM ×1000），越小保真度越高，`1.0` 约为视觉无损 |

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

**响应式图片（srcset）** — 为每个源图按多个宽度各生成一份（每种输出格式都生成），并打印 HTML 片段：

```bash
imageoptimize /path/to/source --output /path/to/output --widths 320,640,1280 --emit-html
```

生成如 `photo-320w.avif`、`photo-640w.avif`……（大于源宽的尺寸会被跳过）。`--emit-html` 输出的片段可直接放进 `<picture>` 元素：

```html
<picture>
  <source type="image/avif" srcset="photo-320w.avif 320w, photo-640w.avif 640w, photo-1280w.avif 1280w">
  <source type="image/webp" srcset="photo-320w.webp 320w, photo-640w.webp 640w, photo-1280w.webp 1280w">
  <img src="photo-1280w.jpeg" sizes="(max-width: 640px) 100vw, 640px" alt="">
</picture>
```

**固定尺寸图（密度 `Nx`）** — 对以固定 CSS 尺寸显示的图（图标、头像、logo），改用密度描述符。给出 1× 显示宽度和倍率：

```bash
imageoptimize /path/to/source --output /path/to/output --densities 1,2,3 --base-width 320 --emit-html
```

每种格式生成 `photo@1x`（320px）、`photo@2x`（640px）、`photo@3x`（960px）（宽度超过源图的倍率会被跳过）。片段使用 `Nx` 描述符，无需 `sizes`：

```html
<picture>
  <source type="image/avif" srcset="photo@1x.avif 1x, photo@2x.avif 2x, photo@3x.avif 3x">
  <source type="image/webp" srcset="photo@1x.webp 1x, photo@2x.webp 2x, photo@3x.webp 3x">
  <img src="photo@1x.jpeg" alt="">
</picture>
```

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
| `hue` | `new_hue_task(shift)` | 整数角度，自动环绕（如 `90`、`-45`） | 旋转每个像素的色调 |
| `saturate` | `new_saturate_task(factor)` | 浮点数（0.0 = 灰度，1.0 = 不变，>1.0 = 增强） | 缩放每个像素的饱和度 |
| `thumbnail` | `new_thumbnail_task(w, h)` | 宽度、高度 | 缩放至覆盖 `w×h`（填充模式）后居中裁剪；与 `fit` 不同，不会留黑边 |
| `thumbnail`（智能） | `new_smart_thumbnail_task(w, h)` | 宽度、高度 | 与 `thumbnail` 相同，但内容感知：裁剪窗口滑向细节最丰富的区域（亮度梯度能量 + 中心偏置），而非死板居中 |
| `invert` | `new_invert_task()` | — | 反转 RGB 通道；alpha 通道保持不变 |
| `opacity` | `new_opacity_task(factor)` | 浮点数（0.0 = 完全透明，1.0 = 不变） | 将每个像素的 alpha 值乘以 factor |
| `gamma` | `new_gamma_task(gamma)` | 浮点数（1.0 = 不变，<1.0 = 增亮，>1.0 = 变暗） | Gamma 校正：`output = (input/255)^gamma × 255`；alpha 不受影响 |
| `background` | `new_background_task(color)` | 十六进制颜色（`#rrggbb` / `#rrggbbaa`，留空 = 不透明白色） | 将图片叠加到纯色背景上以拍平透明度；在编码为 JPEG/JXL 前使用，避免透明区域变黑 |
| `normalize` | `new_normalize_task(per_channel)` | 布尔值（`true` = 每通道 RGB，`false` = 亮度通道） | 自动对比度：将直方图拉伸至完整的 0–255 范围 |
| `trim` | `new_trim_task(tolerance)` | 容差 0–255（与左上角参考色的最大单通道 RGBA 差值） | 自动裁剪四周纯色边框 |
| `strip` | `new_strip_task()` | — | 从编码后的缓冲区移除 EXIF 元数据，无需重新编码（支持 JPEG、PNG、WebP） |
| `padding` | `new_padding_task(w, h, color)` | 宽度、高度、十六进制颜色（`#rrggbb` / `#rrggbbaa`，默认透明） | 扩展画布并居中图片 |
| `watermark` | `new_watermark_task(url, pos, ml, mt)` | url、位置、左边距、上边距 | 叠加水印 |
| `optim` | `new_optim_task(fmt, quality, speed)` | 格式（`jpeg`/`png`/`avif`/`webp`/`gif`/`jxl`）、质量 0–100、速度 | 编码并压缩 |
| `optim`（自动质量） | `new_auto_quality_task(fmt, speed, target)` | 格式、速度、目标 DSSIM ×1000 | 二分搜索使感知差异保持在 `target` 内的最低质量 |
| `optim`（自动格式） | `new_auto_format_task(quality, speed, target)` | 质量 0–100、速度、目标 DSSIM ×1000 | 编码多个候选格式（按是否含透明：webp/avif/png 或 webp/avif/jpeg），保留满足 `target` 的最小者 |
| `optim`（全自动） | `new_auto_task(speed, target)` | 速度、目标 DSSIM ×1000 | 同时搜索格式与质量，取满足 `target` 的最小输出 |
| `diff` | `new_diff_task()` | — | 计算 DSSIM × 1000 评分并存入 `ProcessImage::diff` |

**`optim` speed 参数说明：**

| 格式 | 作用 |
|------|------|
| `avif` | 编码速度 0–10，值越小速度越慢但压缩率/质量越好（默认 `0`） |
| `gif`  | 重新编码动态 GIF 时帧之间的延迟，单位为百分之一秒 |
| `jxl`  | 忽略此参数（编码 effort 固定） |
| `jpeg` / `png` / `webp` | 忽略此参数 |

水印位置：`leftTop`、`top`、`rightTop`、`left`、`center`、`right`、`leftBottom`、`bottom`、`rightBottom`（默认）。

## 许可证

本项目基于 [Apache License 2.0 许可证]。

[Apache License 2.0 许可证]: https://github.com/vicanso/imageoptimize/blob/main/LICENSE
