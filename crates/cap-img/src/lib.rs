//! cap-img — 图片处理原语。
//!
//! 从 `z-biz-tool-file/src-tauri/src/image_utils.rs` 原样搬迁（矩阵规划 03 §4.3）：
//! 路径进 → 路径出的纯函数，不持状态、不建资产库、不调模型。
//! 安全策略（读黑名单/写黑名单/相对路径拒绝）属于调用方的 path_guard，
//! 不在本 crate 的职责内——file 的守卫测试留在 file 侧继续生效。

use image::{DynamicImage, GenericImageView, ImageFormat};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// 图片信息
#[derive(Debug, Serialize, Deserialize)]
pub struct ImageInfo {
    pub width: u32,
    pub height: u32,
    pub format: String,
    pub size: u64,
    pub has_alpha: bool,
    pub exif: Option<std::collections::HashMap<String, String>>,
}

/// 获取图片信息
pub fn info(path: impl AsRef<Path>) -> Result<ImageInfo, String> {
    
    let metadata = fs::metadata(path.as_ref()).map_err(|e| format!("读取元数据失败: {}", e))?;
    let size = metadata.len();

    let img = image::open(path.as_ref()).map_err(|e| format!("打开图片失败: {}", e))?;
    let (width, height) = img.dimensions();
    let has_alpha = img.color().has_alpha();

    let format = detect_format(path.as_ref());

    // 尝试读取 EXIF（JPEG / TIFF）。注意 detect_format 返回的是小写扩展名，
    // 早先这里比的是 "JPEG"/"TIFF"，分支永远走不到 —— 属性面板因此从不显示 EXIF。
    let exif = if matches!(format.as_str(), "jpg" | "tiff") {
        read_basic_exif(path.as_ref())
    } else {
        None
    };

    Ok(ImageInfo {
        width,
        height,
        format,
        size,
        has_alpha,
        exif,
    })
}

/// 简单 EXIF 解析（只取几个常用 tag）
fn read_basic_exif(path: &Path) -> Option<std::collections::HashMap<String, String>> {
    let bytes = fs::read(path).ok()?;
    // JPEG: APP1 段 (0xFFE1) + "Exif\0\0" 签名
    if bytes.len() < 14 || &bytes[0..2] != b"\xff\xd8" {
        return None;
    }
    let mut i = 2;
    while i + 4 < bytes.len() {
        if bytes[i] != 0xff {
            return None;
        }
        let marker = bytes[i + 1];
        let seg_len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        if marker == 0xe1 && &bytes[i + 4..i + 10] == b"Exif\0\0" {
            // 找到 EXIF 段
            return Some(parse_exif_minimal(&bytes[i + 10..i + 2 + seg_len]));
        }
        if marker == 0xda {
            // 图像数据开始，停止
            return None;
        }
        i += 2 + seg_len;
    }
    None
}

fn parse_exif_minimal(data: &[u8]) -> std::collections::HashMap<String, String> {
    use std::collections::HashMap;
    let mut map = HashMap::new();
    if data.len() < 8 {
        return map;
    }
    let little_endian = matches!(data[0], b'I');
    let read_u16 = |d: &[u8]| -> u16 {
        if little_endian {
            u16::from_le_bytes([d[0], d[1]])
        } else {
            u16::from_be_bytes([d[0], d[1]])
        }
    };
    let read_u32 = |d: &[u8]| -> u32 {
        if little_endian {
            u32::from_le_bytes([d[0], d[1], d[2], d[3]])
        } else {
            u32::from_be_bytes([d[0], d[1], d[2], d[3]])
        }
    };

    let ifd0_offset = read_u32(&data[4..8]) as usize;
    if ifd0_offset >= data.len() {
        return map;
    }
    let ifd0_count = read_u16(&data[ifd0_offset..ifd0_offset + 2]) as usize;
    for i in 0..ifd0_count {
        let entry = ifd0_offset + 2 + i * 12;
        if entry + 12 > data.len() {
            break;
        }
        let tag = read_u16(&data[entry..entry + 2]);
        // 0x010F Make, 0x0110 Model, 0x0131 Software, 0x0132 DateTime, 0x8825 GPS
        let (name, val_offset) = match tag {
            0x010F => ("Make", read_u32(&data[entry + 8..entry + 12]) as usize),
            0x0110 => ("Model", read_u32(&data[entry + 8..entry + 12]) as usize),
            0x0131 => ("Software", read_u32(&data[entry + 8..entry + 12]) as usize),
            0x0132 => ("DateTime", read_u32(&data[entry + 8..entry + 12]) as usize),
            0x8298 => ("Copyright", read_u32(&data[entry + 8..entry + 12]) as usize),
            _ => continue,
        };
        // 简化：直接当 ASCII 字符串读
        if val_offset + 8 < data.len() {
            let s: String = data[val_offset..]
                .iter()
                .take_while(|&&b| b != 0)
                .map(|&b| b as char)
                .collect();
            map.insert(name.to_string(), s.trim().to_string());
        }
    }
    map
}

/// 保存 base64 编码的数据到文件，返回写入字节数。
///
/// `format` 与目标扩展名必须一致（jpeg 别名 .jpg/.jpeg 均可）：
/// 前端按所选格式编码后再拼扩展名，两边不一致时写出来的文件是"名字说谎"。
pub fn save_bytes(data: &str, dest_path: impl AsRef<Path>, format: &str) -> Result<u64, String> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let bytes = STANDARD
        .decode(data.as_bytes())
        .map_err(|e| format!("Base64 解码失败: {}", e))?;

    let dest = dest_path.as_ref();
    let want = match format.to_ascii_lowercase().as_str() {
        "jpeg" => vec![".jpg".to_string(), ".jpeg".to_string()],
        other => vec![format!(".{}", other)],
    };
    let ext = dest
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_ascii_lowercase()))
        .unwrap_or_default();
    if !want.contains(&ext) {
        return Err(format!(
            "目标扩展名与所选格式不符: {} vs {}",
            dest_path.as_ref().display(),
            format
        ));
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {}", e))?;
    }

    fs::write(dest, &bytes).map_err(|e| format!("写入文件失败: {}", e))?;
    Ok(bytes.len() as u64)
}

/// 缩略图结果（base64 编码的 PNG）
#[derive(Debug, Serialize, Deserialize)]
pub struct ImageThumbnail {
    pub data: String, // base64 编码的 PNG
    pub width: u32,
    pub height: u32,
}

/// 生成图片缩略图，返回 base64 编码的 PNG
pub fn thumbnail(path: impl AsRef<Path>, max_size: u32) -> Result<ImageThumbnail, String> {
    let img = image::open(path.as_ref()).map_err(|e| format!("打开图片失败: {}", e))?;

    // 缩放图片以适应 max_size
    let size = if max_size == 0 { 200 } else { max_size };
    let thumb = img.thumbnail(size, size);

    let (width, height) = thumb.dimensions();

    // 编码为 PNG 到内存
    let mut buf: Vec<u8> = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut buf);
    thumb
        .write_to(&mut cursor, ImageFormat::Png)
        .map_err(|e| format!("编码缩略图失败: {}", e))?;

    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(&buf);

    Ok(ImageThumbnail { data, width, height })
}

/// 导出图片为指定格式
pub fn export(path: impl AsRef<Path>, dest_path: impl AsRef<Path>, format: &str, quality: u8) -> Result<(), String> {
    let img = image::open(path.as_ref()).map_err(|e| format!("打开图片失败: {}", e))?;
    // 先校验再建目录：反过来会替调用方把 .ssh 这类敏感目录凭空创建出来
    let dest = dest_path.as_ref();
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {}", e))?;
    }

    let image_format = parse_format(format)?;
    let quality = quality.clamp(1, 100);

    match image_format {
        ImageFormat::Jpeg => {
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
                fs::File::create(dest).map_err(|e| format!("创建文件失败: {}", e))?,
                quality,
            );
            encoder
                .encode(
                    img.to_rgb8().as_raw(),
                    img.width(),
                    img.height(),
                    image::ExtendedColorType::Rgb8,
                )
                .map_err(|e| format!("编码JPEG失败: {}", e))?;
        }
        _ => {
            img.save_with_format(dest, image_format)
                .map_err(|e| format!("保存图片失败: {}", e))?;
        }
    }

    Ok(())
}

/// 缩放图片
pub fn resize(path: impl AsRef<Path>, dest_path: impl AsRef<Path>, width: u32, height: u32) -> Result<(), String> {
    let img = image::open(path.as_ref()).map_err(|e| format!("打开图片失败: {}", e))?;
    let resized = img.resize(width, height, image::imageops::FilterType::Lanczos3);

    save_like_source(path, dest_path, &resized)
}

/// 旋转图片
pub fn rotate(path: impl AsRef<Path>, dest_path: impl AsRef<Path>, degrees: u32) -> Result<(), String> {
    let img = image::open(path.as_ref()).map_err(|e| format!("打开图片失败: {}", e))?;
    let rotated = match degrees {
        90 => img.rotate90(),
        180 => img.rotate180(),
        270 => img.rotate270(),
        _ => return Err(format!("不支持的角度: {}，仅支持 90/180/270", degrees)),
    };

    save_like_source(path, dest_path, &rotated)
}

/// 翻转图片
pub fn flip(path: impl AsRef<Path>, dest_path: impl AsRef<Path>, horizontal: bool) -> Result<(), String> {
    let img = image::open(path.as_ref()).map_err(|e| format!("打开图片失败: {}", e))?;
    let flipped = if horizontal { img.fliph() } else { img.flipv() };

    save_like_source(path, dest_path, &flipped)
}

/// 裁剪图片
pub fn crop(path: impl AsRef<Path>, dest_path: impl AsRef<Path>, x: u32, y: u32, w: u32, h: u32) -> Result<(), String> {
    let img = image::open(path.as_ref()).map_err(|e| format!("打开图片失败: {}", e))?;

    // 验证裁剪区域
    if x + w > img.width() || y + h > img.height() {
        return Err(format!(
            "裁剪区域超出图片范围: 图片{}x{}, 裁剪({},{})+{}x{}",
            img.width(),
            img.height(),
            x,
            y,
            w,
            h
        ));
    }

    let cropped = img.crop_imm(x, y, w, h);
    save_like_source(path, dest_path, &cropped)
}

/// 应用滤镜
pub fn apply_filter(path: impl AsRef<Path>, dest_path: impl AsRef<Path>, filter_name: &str) -> Result<(), String> {
    let img = image::open(path.as_ref()).map_err(|e| format!("打开图片失败: {}", e))?;
    let filtered = filter_impl(&img, filter_name)?;
    save_like_source(path, dest_path, &filtered)
}

/// 按"源图扩展名决定输出格式"保存——resize/rotate/flip/crop/filter 共用的收尾
fn save_like_source(src: impl AsRef<Path>, dest_path: impl AsRef<Path>, img: &DynamicImage) -> Result<(), String> {
    // 先校验再建目录：反过来会替调用方把 .ssh 这类敏感目录凭空创建出来
    let dest = dest_path.as_ref();
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {}", e))?;
    }

    let format = detect_format(src);
    let image_format = parse_format(&format).unwrap_or(ImageFormat::Png);
    img.save_with_format(dest, image_format)
        .map_err(|e| format!("保存图片失败: {}", e))?;

    Ok(())
}

/// 滤镜实现
fn filter_impl(img: &DynamicImage, filter_name: &str) -> Result<DynamicImage, String> {
    match filter_name {
        "grayscale" => Ok(DynamicImage::ImageLuma8(img.to_luma8())),
        "sepia" => {
            let mut rgba = img.to_rgba8();
            for pixel in rgba.pixels_mut() {
                let r = pixel[0] as f32;
                let g = pixel[1] as f32;
                let b = pixel[2] as f32;
                // Sepia矩阵
                let new_r = (r * 0.393 + g * 0.769 + b * 0.189).min(255.0) as u8;
                let new_g = (r * 0.349 + g * 0.686 + b * 0.168).min(255.0) as u8;
                let new_b = (r * 0.272 + g * 0.534 + b * 0.131).min(255.0) as u8;
                pixel[0] = new_r;
                pixel[1] = new_g;
                pixel[2] = new_b;
            }
            Ok(DynamicImage::ImageRgba8(rgba))
        }
        "invert" => {
            let mut rgba = img.to_rgba8();
            for pixel in rgba.pixels_mut() {
                pixel[0] = 255 - pixel[0];
                pixel[1] = 255 - pixel[1];
                pixel[2] = 255 - pixel[2];
            }
            Ok(DynamicImage::ImageRgba8(rgba))
        }
        "brightness" => {
            let mut rgba = img.to_rgba8();
            for pixel in rgba.pixels_mut() {
                pixel[0] = (pixel[0] as f32 * 1.3).min(255.0) as u8;
                pixel[1] = (pixel[1] as f32 * 1.3).min(255.0) as u8;
                pixel[2] = (pixel[2] as f32 * 1.3).min(255.0) as u8;
            }
            Ok(DynamicImage::ImageRgba8(rgba))
        }
        "contrast" => {
            let mut rgba = img.to_rgba8();
            let factor: f32 = 1.5; // 对比度因子
            for pixel in rgba.pixels_mut() {
                pixel[0] = (factor * (pixel[0] as f32 - 128.0) + 128.0)
                    .clamp(0.0, 255.0) as u8;
                pixel[1] = (factor * (pixel[1] as f32 - 128.0) + 128.0)
                    .clamp(0.0, 255.0) as u8;
                pixel[2] = (factor * (pixel[2] as f32 - 128.0) + 128.0)
                    .clamp(0.0, 255.0) as u8;
            }
            Ok(DynamicImage::ImageRgba8(rgba))
        }
        "blur" => Ok(img.blur(3.0)),
        "sharpen" => {
            // 简单锐化: 先模糊再与原图混合
            let blurred = img.blur(1.0);
            let mut result = img.to_rgba8();
            let blurred_rgba = blurred.to_rgba8();
            let blurred_vec: Vec<_> = blurred_rgba.pixels().collect();
            for (i, pixel) in result.pixels_mut().enumerate() {
                if let Some(blurred_pixel) = blurred_vec.get(i) {
                    // Unsharp mask: original + (original - blurred) * amount
                    let amount: f32 = 1.5;
                    pixel[0] = (pixel[0] as f32 + (pixel[0] as f32 - blurred_pixel[0] as f32) * amount)
                        .clamp(0.0, 255.0) as u8;
                    pixel[1] = (pixel[1] as f32 + (pixel[1] as f32 - blurred_pixel[1] as f32) * amount)
                        .clamp(0.0, 255.0) as u8;
                    pixel[2] = (pixel[2] as f32 + (pixel[2] as f32 - blurred_pixel[2] as f32) * amount)
                        .clamp(0.0, 255.0) as u8;
                }
            }
            Ok(DynamicImage::ImageRgba8(result))
        }
        _ => Err(format!(
            "不支持的滤镜: {}，支持: grayscale/sepia/invert/brightness/contrast/blur/sharpen",
            filter_name
        )),
    }
}

/// 按扩展名检测图片格式（返回小写规范名，未知扩展名默认 png）
pub fn detect_format(path: impl AsRef<Path>) -> String {
    let ext = path.as_ref()
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    match ext.as_str() {
        "jpg" | "jpeg" => "jpg".to_string(),
        "png" => "png".to_string(),
        "gif" => "gif".to_string(),
        "bmp" => "bmp".to_string(),
        "webp" => "webp".to_string(),
        "ico" => "ico".to_string(),
        "tiff" | "tif" => "tiff".to_string(),
        _ => "png".to_string(), // 默认
    }
}

/// 解析图片格式字符串
pub fn parse_format(format: &str) -> Result<ImageFormat, String> {
    match format.to_lowercase().as_str() {
        "jpg" | "jpeg" => Ok(ImageFormat::Jpeg),
        "png" => Ok(ImageFormat::Png),
        "gif" => Ok(ImageFormat::Gif),
        "bmp" => Ok(ImageFormat::Bmp),
        "webp" => Ok(ImageFormat::WebP),
        "ico" => Ok(ImageFormat::Ico),
        "tiff" | "tif" => Ok(ImageFormat::Tiff),
        _ => Err(format!(
            "不支持的图片格式: {}，支持: jpg/png/gif/bmp/webp",
            format
        )),
    }
}

// ---------------------------------------------------------------------------
// 0.2.0：内存进内存出的字节级原语（data URL / 魔数嗅探 / 帧编码）
// 这些场景没有落盘路径可传，因此不走上面的「路径进路径出」签名。
// ---------------------------------------------------------------------------

/// 按文件头魔数嗅探图片格式（**不看扩展名**，扩展名可伪造）。
/// 识别 png/jpg/gif/webp，其余返回 None。
pub fn sniff_format(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

/// 解析出的 data URL：MIME（小写、去掉参数段）+ 字节。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataUrl {
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// 解析 `data:<mime>;base64,<payload>` → MIME + 字节。
///
/// - 非 `data:` 前缀、缺逗号、meta 里没有 base64 标记、或 payload 不是合法 base64 → None
/// - payload 允许无 padding（`AA` 与 `AA==` 都收），与 note 侧历史行为一致
pub fn decode_data_url(url: &str) -> Option<DataUrl> {
    let rest = url.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    if !meta.to_ascii_lowercase().contains("base64") {
        return None;
    }
    let mime = meta
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let bytes = base64_decode_padded(payload.trim()).ok()?;
    Some(DataUrl { mime, bytes })
}

/// 字节 → `data:<mime>;base64,<payload>`
pub fn encode_data_url(bytes: &[u8], mime: &str) -> String {
    use base64::Engine;
    format!(
        "data:{};base64,{}",
        mime,
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

/// MIME → 扩展名（`image/jpeg` → `jpg`，`image/svg+xml` → `svg`，认不出 → `png`）
pub fn ext_for_mime(mime: &str) -> String {
    let mime = mime.to_ascii_lowercase();
    mime
        .split_once('/')
        .map(|(_, sub)| match sub {
            "jpeg" | "jpg" => "jpg".to_string(),
            other => other.split('+').next().unwrap_or("png").to_string(),
        })
        .unwrap_or_else(|| "png".to_string())
}

/// 扩展名 → MIME（认不出一律按 png 处理）
pub fn mime_for_ext(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

/// base64 解码，接受有/无 padding 两种写法（内部补齐到 4 的倍数）。
fn base64_decode_padded(s: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    let mut s = s.trim().to_string();
    while !s.len().is_multiple_of(4) {
        s.push('=');
    }
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| format!("Base64 解码失败: {}", e))
}

/// 内存进内存出的 JPEG 帧编码结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameJpeg {
    pub base64: String,
    pub width: u32,
    pub height: u32,
}

/// 把一帧 RGBA 原始像素编码成 JPEG/base64（截屏流这类**没有落盘路径**的场景）。
///
/// - `rgba_raw` 必须恰好 `width * height * 4` 字节；长度不符返回 Err —— **不 panic**。
///   （历史 bug：把 4 通道数据声明成 `Rgb8` 交给 JPEG 编码器会直接 abort 整个进程）
/// - `max_width == 0` 或原图已更窄时不缩放；缩放用三角滤波、新高度向上取整
/// - `quality` 夹到 10..=100
pub fn encode_frame_jpeg(
    rgba_raw: &[u8],
    width: u32,
    height: u32,
    quality: u8,
    max_width: u32,
) -> Result<FrameJpeg, String> {
    use base64::Engine;
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| format!("尺寸溢出: {}x{}", width, height))?;
    if rgba_raw.len() != expected {
        return Err(format!(
            "RGBA 数据长度不符: 期望 {} 字节, 实得 {} 字节 ({}x{})",
            expected,
            rgba_raw.len(),
            width,
            height
        ));
    }
    let mut img = image::RgbaImage::from_raw(width, height, rgba_raw.to_vec())
        .ok_or_else(|| format!("无法构造 RGBA 图像: {}x{}", width, height))?;

    if max_width != 0 && width > max_width {
        let new_h = ((height as f64) * (max_width as f64 / width as f64)).ceil() as u32;
        img = image::imageops::resize(
            &img,
            max_width,
            new_h.max(1),
            image::imageops::FilterType::Triangle,
        );
    }

    let (w, h) = img.dimensions();
    // RgbaImage 自身没有 to_rgb8，要先包成 DynamicImage；这一步把 4 通道降成 3 通道，
    // 下面声明 Rgb8 才和真实缓冲长度对得上（历史 bug 正是这里没做转换）
    let rgb = DynamicImage::ImageRgba8(img).to_rgb8();
    let mut buf: Vec<u8> = Vec::new();
    let q = quality.clamp(10, 100);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, q)
        .encode(rgb.as_raw(), w, h, image::ExtendedColorType::Rgb8)
        .map_err(|e| format!("编码 JPEG 失败: {}", e))?;

    Ok(FrameJpeg {
        base64: base64::engine::general_purpose::STANDARD.encode(&buf),
        width: w,
        height: h,
    })
}
