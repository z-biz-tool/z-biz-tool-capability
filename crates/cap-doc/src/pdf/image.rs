//! 从 PDF 页面里取出嵌入位图。
//!
//! lopdf 的 `decompressed_content()` 对 `/Subtype /Image` 的流直接返回 Err（它只服务
//! 内容流），所以位图这条路要自己走一遍滤镜链：DCT/JPX 本来就是完整编码格式，原样落盘；
//! Flate 解出来还要还原 PNG 预测器，再按 BitsPerComponent 逐位取采样、套 Decode 数组、
//! 按颜色空间重建像素，最后统一编成 PNG。CCITT/JBIG2 这类扫描压缩本机没有解码器，
//! 明确报"跳过 + 原因"而不是静默丢图，否则用户看到的是一份少了图的假成功。

use flate2::read::ZlibDecoder;
use image::{DynamicImage, GrayImage, ImageFormat, RgbImage, RgbaImage};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::Path;

/// 像素总量上限：Width/Height 都来自文件，损坏或恶意的 PDF 写个 1e5x1e5 会让分配直接打爆内存
const MAX_PIXELS: u64 = 40_000_000;

/// 提取结果：成功写出的文件，以及没取出来的图和原因
#[derive(Debug, Clone, Serialize)]
pub struct ExtractReport {
    pub images: Vec<String>,
    pub skipped: Vec<String>,
}

pub(crate) enum Decoded {
    /// JPEG / JPEG2000：字节原样写出，转 PNG 只会二次损失画质还涨体积
    Passthrough { ext: &'static str, bytes: Vec<u8> },
    /// 统一收到 DynamicImage：无掩码的灰度/彩色图没必要一律撑成 RGBA，
    /// 24MP 的整页扫描图从 97MB 降到 24MB，PNG 编码也是同比例变快
    Image(DynamicImage),
}

enum ColorSpace {
    Gray,
    Rgb,
    Cmyk,
    Indexed { hival: u32, lookup: Vec<u8> },
}
pub fn extract_pdf_images_blocking(
    input_path: String,
    output_dir: String,
) -> Result<ExtractReport, String> {
    let file = crate::paths::input(&input_path)?;
    let doc = crate::pdf::ops::load_doc(&file)?;
    let dir = crate::paths::output(&output_dir)?;
    fs::create_dir_all(&dir).map_err(|e| format!("创建输出目录失败: {}", e))?;

    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "image".to_string());

    let mut report = ExtractReport {
        images: Vec::new(),
        skipped: Vec::new(),
    };
    let mut seen: BTreeSet<ObjectId> = BTreeSet::new();

    for (index, page) in doc.get_pages().values().enumerate() {
        for (name, id) in image_xobjects(&doc, *page) {
            if !seen.insert(id) {
                continue;
            }
            let label = format!("第 {} 页 {}", index + 1, name);
            let stream = match doc.get_object(id).and_then(Object::as_stream) {
                Ok(stream) => stream,
                Err(_) => {
                    report.skipped.push(format!("{}：对象不是流", label));
                    continue;
                }
            };
            let decoded = match decode_image(&doc, stream, 0) {
                Ok(decoded) => decoded,
                Err(reason) => {
                    report.skipped.push(format!("{}：{}", label, reason));
                    continue;
                }
            };
            let ext = match &decoded {
                Decoded::Passthrough { ext, .. } => *ext,
                Decoded::Image(_) => "png",
            };
            let path = dir.join(format!("{}-{:02}-{:02}.{}", stem, index + 1, id.0, ext));
            if let Err(reason) = save_decoded(&decoded, &path) {
                report.skipped.push(format!("{}：{}", label, reason));
                continue;
            }
            report.images.push(path.to_string_lossy().to_string());
        }
    }
    Ok(report)
}

fn save_decoded(decoded: &Decoded, path: &Path) -> Result<(), String> {
    match decoded {
        Decoded::Passthrough { bytes, .. } => {
            fs::write(path, bytes).map_err(|e| format!("写入失败: {}", e))
        }
        Decoded::Image(image) => image
            .save_with_format(path, ImageFormat::Png)
            .map_err(|e| format!("写入失败: {}", e)),
    }
}

/// 页面可达的位图：Resources（含祖先继承）→ XObject。
/// Form XObject 里还能再嵌图（实测一份 92 图的语料有 29 张只出现在 Form 资源里），
/// 所以要顺着 Form 走下去；`visited` 防互引成环，`depth` 防无限套娃。
pub(crate) fn image_xobjects(doc: &Document, page: ObjectId) -> Vec<(String, ObjectId)> {
    let mut found = Vec::new();
    let mut visited: BTreeSet<ObjectId> = BTreeSet::new();
    collect_images(doc, page, 0, "", &mut visited, &mut found);
    found
}

fn dict_of(object: &Object) -> Option<&Dictionary> {
    match object {
        Object::Dictionary(dict) => Some(dict),
        // Form/图片都是流对象，而 lopdf 的 `as_dict` 只认 Dictionary：
        // 用它取 Form 的 Resources 会静默得到 None，Form 里嵌的图就全丢了
        Object::Stream(stream) => Some(&stream.dict),
        _ => None,
    }
}

fn collect_images(
    doc: &Document,
    container: ObjectId,
    depth: u8,
    prefix: &str,
    visited: &mut BTreeSet<ObjectId>,
    found: &mut Vec<(String, ObjectId)>,
) {
    if depth > 4 || !visited.insert(container) {
        return;
    }
    // 页面的 Resources 可以从祖先继承，Form 的只能是自己声明的
    let resources = if depth == 0 {
        crate::pdf::ops::inheritable(doc, container, b"Resources")
    } else {
        doc.get_object(container)
            .ok()
            .and_then(dict_of)
            .and_then(|dict| dict.get(b"Resources").ok().cloned())
    };
    let Some(resources) = resources.as_ref().and_then(|value| deref(doc, value)) else {
        return;
    };
    let Some(xobjects) = dict_of(resources)
        .and_then(|dict| dict.get(b"XObject").ok())
        .and_then(|value| deref(doc, value))
        .and_then(dict_of)
    else {
        return;
    };
    for (key, value) in xobjects.iter() {
        let Ok(id) = value.as_reference() else {
            continue;
        };
        let Ok(stream) = doc.get_object(id).and_then(Object::as_stream) else {
            continue;
        };
        let name = String::from_utf8_lossy(key).to_string();
        let label = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{} {}", prefix, name)
        };
        let subtype = stream
            .dict
            .get(b"Subtype")
            .and_then(Object::as_name)
            .ok()
            .map(|value| value.to_vec());
        match subtype.as_deref() {
            Some(b"Image") => found.push((label, id)),
            Some(b"Form") => collect_images(doc, id, depth + 1, &label, visited, found),
            // 少数生产者漏写 /Subtype，按图片处理至少能试一次解码
            None => found.push((label, id)),
            _ => {}
        }
    }
}

fn field_i64(stream: &Stream, key: &[u8]) -> Result<i64, String> {
    let value = stream
        .dict
        .get(key)
        .map_err(|_| format!("缺少 /{}", String::from_utf8_lossy(key)))?;
    value
        .as_i64()
        .map_err(|_| format!("/{} 不是整数", String::from_utf8_lossy(key)))
}

fn filters_of(stream: &Stream) -> Vec<Vec<u8>> {
    match stream.dict.get(b"Filter") {
        Ok(Object::Name(name)) => vec![name.clone()],
        Ok(Object::Array(names)) => names
            .iter()
            .filter_map(|item| item.as_name().ok().map(|name| name.to_vec()))
            .collect(),
        _ => Vec::new(),
    }
}

/// Decode 数组里 0.0/1.0 可能是 Integer 也可能是 Real
fn number(object: &Object) -> Option<f32> {
    match object {
        Object::Integer(value) => Some(*value as f32),
        Object::Real(value) => Some(*value),
        _ => None,
    }
}

fn is_true(stream: &Stream, key: &[u8]) -> bool {
    matches!(stream.dict.get(key).ok(), Some(Object::Boolean(true)))
}

/// `depth` 只用来防 /SMask 自引用成环：损坏文件里 A 的掩码指回 A 会让递归永不返回
pub(crate) fn decode_image(doc: &Document, stream: &Stream, depth: u8) -> Result<Decoded, String> {
    let width = field_i64(stream, b"Width")?;
    let height = field_i64(stream, b"Height")?;
    if width <= 0 || height <= 0 {
        return Err("宽高不是正数".to_string());
    }
    if (width as u64) * (height as u64) > MAX_PIXELS {
        return Err(format!(
            "尺寸异常（{}x{}），拒绝解码以免耗尽内存",
            width, height
        ));
    }

    let filters = filters_of(stream);
    for (name, ext) in [(&b"DCTDecode"[..], "jpg"), (&b"JPXDecode"[..], "jp2")] {
        if filters.iter().any(|filter| filter == name) {
            return Ok(Decoded::Passthrough {
                ext,
                bytes: stream.content.clone(),
            });
        }
    }
    const UNSUPPORTED: [&str; 4] = [
        "CCITTFaxDecode",
        "JBIG2Decode",
        "LZWDecode",
        "RunLengthDecode",
    ];
    if let Some(codec) = filters
        .iter()
        .find(|filter| UNSUPPORTED.contains(&String::from_utf8_lossy(filter).as_ref()))
    {
        return Err(format!(
            "{} 压缩暂不支持（旧式扫描图常见），已跳过",
            String::from_utf8_lossy(codec)
        ));
    }

    let stencil = is_true(stream, b"ImageMask");
    let bits = field_i64(stream, b"BitsPerComponent").unwrap_or(if stencil { 1 } else { 8 });
    if !matches!(bits, 1 | 2 | 4 | 8) {
        return Err(format!("不支持 BitsPerComponent={}", bits));
    }
    let (space, inverted_default) = color_space_of(doc, stream, stencil)?;
    let components = match &space {
        ColorSpace::Gray | ColorSpace::Indexed { .. } => 1,
        ColorSpace::Rgb => 3,
        ColorSpace::Cmyk => 4,
    };

    let samples = if filters.iter().any(|filter| filter == b"FlateDecode") {
        inflate(&stream.content)?
    } else if filters.is_empty() {
        stream.content.clone()
    } else {
        return Err("不支持的滤镜链".to_string());
    };
    let params = decode_parms(doc, stream);
    let samples = match &params {
        Some(params) => apply_predictor(&samples, params, width as usize, components)?,
        None => samples,
    };

    let decode = decode_array(stream, components, inverted_default);
    let mask = match stream.dict.get(b"SMask").ok().map(Object::as_reference) {
        Some(Ok(id)) if depth < 1 => Some(
            doc.get_object(id)
                .and_then(Object::as_stream)
                .map_err(|_| "/SMask 引用无效")?,
        ),
        Some(Ok(_)) => None,
        Some(Err(_)) => return Err("/SMask 引用无效".to_string()),
        None => None,
    };
    let mask_image = match mask {
        Some(stream) => match decode_image(doc, stream, depth + 1)? {
            Decoded::Image(image)
                if image.width() == width as u32 && image.height() == height as u32 =>
            {
                // 掩码只用一条通道当 alpha，先归一到灰度，省掉一份 RGBA 的中间缓冲
                Some(image.to_luma8())
            }
            // 掩码尺寸对不上时按不透明处理，至少图取得出来
            _ => None,
        },
        None => None,
    };

    Ok(Decoded::Image(rasterize(
        &samples,
        width as u32,
        height as u32,
        bits as u8,
        &space,
        &decode,
        mask_image.as_ref(),
    )?))
}

fn inflate(input: &[u8]) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    ZlibDecoder::new(input)
        .read_to_end(&mut output)
        .map_err(|e| format!("Flate 解压失败: {}", e))?;
    Ok(output)
}

/// DecodeParms 可能是字典，也可能是与 Filter 数组逐项对齐的字典数组
fn decode_parms(doc: &Document, stream: &Stream) -> Option<Dictionary> {
    let value = stream
        .dict
        .get(b"DecodeParms")
        .or_else(|_| stream.dict.get(b"DP"))
        .ok()?
        .clone();
    match deref(doc, &value)? {
        Object::Dictionary(dict) => Some(dict.clone()),
        Object::Array(items) => items.iter().find_map(|item| {
            deref(doc, item)
                .and_then(|object| object.as_dict().ok())
                .cloned()
        }),
        _ => None,
    }
}

fn deref<'a>(doc: &'a Document, object: &'a Object) -> Option<&'a Object> {
    match object {
        Object::Reference(id) => doc.get_object(*id).ok(),
        other => Some(other),
    }
}

/// PNG 预测器（Predictor 10~15）：每行开头一个滤镜字节。Tiff 水平差分（2~8）不在常见 PDF 里，遇到时原样返回。
fn apply_predictor(
    data: &[u8],
    params: &Dictionary,
    width: usize,
    components: usize,
) -> Result<Vec<u8>, String> {
    let predictor = params
        .get(b"Predictor")
        .and_then(Object::as_i64)
        .unwrap_or(1);
    if predictor < 10 {
        return Ok(data.to_vec());
    }
    let colors = params
        .get(b"Colors")
        .and_then(Object::as_i64)
        .unwrap_or(components as i64) as usize;
    let bits = params
        .get(b"BitsPerComponent")
        .and_then(Object::as_i64)
        .unwrap_or(8) as usize;
    let columns = params
        .get(b"Columns")
        .and_then(Object::as_i64)
        .unwrap_or(width as i64) as usize;
    let bytes_per_row = (columns * colors * bits).div_ceil(8);
    if bytes_per_row == 0 {
        return Ok(data.to_vec());
    }
    let stride = (colors * bits).div_ceil(8);
    let mut out = Vec::with_capacity(data.len());
    let mut previous = vec![0u8; bytes_per_row];
    for row in data.chunks(bytes_per_row + 1) {
        if row.len() <= bytes_per_row {
            break;
        }
        let mut current = vec![0u8; bytes_per_row];
        for i in 0..bytes_per_row {
            let raw = row[1 + i] as i32;
            let a = if i >= stride {
                current[i - stride] as i32
            } else {
                0
            };
            let b = previous[i] as i32;
            let c = if i >= stride {
                previous[i - stride] as i32
            } else {
                0
            };
            let value = match row[0] {
                0 => raw,
                1 => raw + a,
                2 => raw + b,
                3 => raw + (a + b) / 2,
                4 => {
                    let estimate = a + b - c;
                    let (pa, pb, pc) = (
                        (estimate - a).abs(),
                        (estimate - b).abs(),
                        (estimate - c).abs(),
                    );
                    let predicted = if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    };
                    raw + predicted
                }
                _ => raw,
            };
            current[i] = (value & 0xff) as u8;
        }
        out.extend_from_slice(&current);
        previous = current;
    }
    Ok(out)
}

/// 返回颜色空间和"默认 Decode 是否反相"：CMYK 与 ImageMask 的规范默认值都是 [1 0 …]
fn color_space_of(
    doc: &Document,
    stream: &Stream,
    stencil: bool,
) -> Result<(ColorSpace, bool), String> {
    if stencil {
        return Ok((ColorSpace::Gray, true));
    }
    let declared = stream
        .dict
        .get(b"ColorSpace")
        .map_err(|_| "缺少 /ColorSpace".to_string())?
        .clone();
    let declared = deref(doc, &declared).ok_or("/ColorSpace 引用无效")?;
    if let Ok(name) = declared.as_name() {
        return match name {
            b"DeviceGray" => Ok((ColorSpace::Gray, false)),
            b"DeviceRGB" => Ok((ColorSpace::Rgb, false)),
            b"DeviceCMYK" => Ok((ColorSpace::Cmyk, true)),
            other => Err(format!(
                "不支持颜色空间 /{}",
                String::from_utf8_lossy(other)
            )),
        };
    }
    let array = declared
        .as_array()
        .map_err(|_| "/ColorSpace 类型无法识别".to_string())?;
    let family = array
        .first()
        .and_then(|object| object.as_name().ok())
        .unwrap_or_default()
        .to_vec();
    match family.as_slice() {
        b"Indexed" if array.len() >= 4 => {
            let hival = array[2]
                .as_i64()
                .map_err(|_| "/Indexed 缺 hival".to_string())?;
            let lookup = match deref(doc, &array[3]).ok_or("/Indexed 调色板引用无效")? {
                Object::Array(values) => values
                    .iter()
                    .map(|v| v.as_i64().unwrap_or(0).clamp(0, 255) as u8)
                    .collect(),
                Object::Stream(palette) => {
                    if filters_of(palette).iter().any(|f| f == b"FlateDecode") {
                        inflate(&palette.content)?
                    } else {
                        palette.content.clone()
                    }
                }
                _ => return Err("/Indexed 调色板形式无法识别".to_string()),
            };
            Ok((
                ColorSpace::Indexed {
                    hival: hival as u32,
                    lookup,
                },
                false,
            ))
        }
        b"ICCBased" => {
            // ICC 描述文件按规范就是一个流（参数是它的引用），只认字典会漏掉真实语料里的全部 ICC 图
            let dict = array
                .get(1)
                .and_then(|object| deref(doc, object))
                .and_then(dict_of)
                .ok_or("/ICCBased 参数不是字典或流".to_string())?;
            let n = dict.get(b"N").and_then(Object::as_i64).unwrap_or(3);
            match n {
                1 => Ok((ColorSpace::Gray, false)),
                3 => Ok((ColorSpace::Rgb, false)),
                4 => Ok((ColorSpace::Cmyk, true)),
                other => Err(format!("不支持 ICC 通道数 {}", other)),
            }
        }
        // Cal* 的白点/伽马没有 ICC 描述文件配套时无处可查，按线性 RGB/灰度取整值，
        // 与查看器缺 Profile 时的近似一致；继续报错的话这份语料里 24 张图就白丢了
        b"CalRGB" => Ok((ColorSpace::Rgb, false)),
        b"CalGray" => Ok((ColorSpace::Gray, false)),
        _ => Err(format!(
            "不支持颜色空间 {}",
            String::from_utf8_lossy(&family)
        )),
    }
}

/// /Decode 默认值：CMYK 与 ImageMask 反相，其余每通道 [0 1]
fn decode_array(stream: &Stream, components: usize, inverted: bool) -> Vec<f32> {
    let defaults: Vec<f32> = (0..components)
        .flat_map(|_| {
            if inverted {
                [1.0f32, 0.0f32]
            } else {
                [0.0f32, 1.0f32]
            }
        })
        .collect();
    match stream.dict.get(b"Decode") {
        Ok(Object::Array(values)) => (0..components * 2)
            .map(|i| values.get(i).and_then(number).unwrap_or(defaults[i]))
            .collect(),
        _ => defaults,
    }
}

/// 按位取采样。PDF 图像每行的比特数不必是 8 的倍数，行尾补零到整字节；
/// `expand` 为真时把 1/2/4 bit 的取值域线性铺到 0..=255（否则 1-bit 图会解出接近纯黑的 0/1）。
fn unpack(
    samples: &[u8],
    width: usize,
    height: usize,
    bits: u8,
    components: usize,
    expand: bool,
) -> Vec<u8> {
    let per_row_bytes = (width * components * bits as usize).div_ceil(8);
    // 8 bit 时每行本来就是整字节，逐位拼窗口只是把一次 memcpy 拆成几千万次移位
    if bits == 8 {
        let row_len = width * components;
        let mut out = Vec::with_capacity(row_len * height);
        for row in 0..height {
            let start = row * per_row_bytes;
            match samples.get(start..start + row_len) {
                Some(row_bytes) => out.extend_from_slice(row_bytes),
                // 数据被截断时按补零处理，与下面的逐位路径一致
                None => {
                    for column in 0..row_len {
                        out.push(*samples.get(start + column).unwrap_or(&0));
                    }
                }
            }
        }
        return out;
    }
    let mask = (1u16 << bits) - 1;
    let mut out = Vec::with_capacity(width * height * components);
    for row in 0..height {
        let base = row * per_row_bytes * 8;
        for i in 0..width * components {
            let bit = base + i * bits as usize;
            let offset = (bit % 8) as u16;
            let window = ((samples.get(bit / 8).copied().unwrap_or(0) as u16) << 8)
                | (samples.get(bit / 8 + 1).copied().unwrap_or(0) as u16);
            let raw = (window >> (16 - offset - bits as u16)) & mask;
            out.push(if expand && bits != 8 {
                (raw * 255 / mask) as u8
            } else {
                raw as u8
            });
        }
    }
    out
}

/// /Decode 给的是颜色空间里的 0~1 区间，落到字节还要再乘 255；
/// CMYK 与 ImageMask 的默认值 [1 0] 就是靠这一步反相的。
fn scale(value: u8, from: f32, to: f32) -> u8 {
    let unit = from + (value as f32 / 255.0) * (to - from);
    (unit * 255.0).clamp(0.0, 255.0).round() as u8
}

fn scale_table(from: f32, to: f32) -> [u8; 256] {
    let mut table = [0u8; 256];
    for (value, slot) in table.iter_mut().enumerate() {
        *slot = scale(value as u8, from, to);
    }
    table
}

fn rasterize(
    samples: &[u8],
    width: u32,
    height: u32,
    bits: u8,
    space: &ColorSpace,
    decode: &[f32],
    mask: Option<&GrayImage>,
) -> Result<DynamicImage, String> {
    let components = match space {
        ColorSpace::Gray | ColorSpace::Indexed { .. } => 1,
        ColorSpace::Rgb => 3,
        ColorSpace::Cmyk => 4,
    };
    // 调色板图的分量是索引，不能被当成亮度铺开到 0..=255，也不能套区间表
    let indexed = matches!(space, ColorSpace::Indexed { .. });
    let raw = unpack(
        samples,
        width as usize,
        height as usize,
        bits,
        components,
        !indexed,
    );
    // 每个分量烘一张 256 项表：整页扫描图有两千多万像素，
    // 逐点做浮点乘加比逐点搬字节贵一个数量级
    let tables: Vec<[u8; 256]> = if indexed {
        Vec::new()
    } else {
        (0..components)
            .map(|component| scale_table(decode[component * 2], decode[component * 2 + 1]))
            .collect()
    };
    let gray_out = components == 1 && mask.is_none() && !indexed;
    let channels = if mask.is_some() {
        4
    } else if gray_out {
        1
    } else {
        3
    };
    let total = (width as usize) * (height as usize);
    let mut pixels: Vec<u8> = Vec::with_capacity(total * channels);
    // 掩码与图像同尺寸且都是行主序，并排走就不用逐点算 x/y（除法和取余在热路上很贵）
    let mut mask_pixels = mask.into_iter().flat_map(|image| image.pixels());
    for sample in raw.chunks(components) {
        let pick =
            |component: usize| tables[component][*sample.get(component).unwrap_or(&0) as usize];
        let rgb: [u8; 3] = if indexed {
            let (hival, lookup) = match space {
                ColorSpace::Indexed { hival, lookup } => (*hival, lookup),
                _ => unreachable!(),
            };
            let at = (*sample.first().unwrap_or(&0)).min(hival as u8) as usize * 3;
            [
                *lookup.get(at).unwrap_or(&0),
                *lookup.get(at + 1).unwrap_or(&0),
                *lookup.get(at + 2).unwrap_or(&0),
            ]
        } else {
            match space {
                ColorSpace::Gray => {
                    let g = pick(0);
                    [g, g, g]
                }
                ColorSpace::Rgb => [pick(0), pick(1), pick(2)],
                ColorSpace::Cmyk => {
                    let (c, m, yk, k) = (pick(0), pick(1), pick(2), pick(3));
                    [
                        ((255 - c as u16) * (255 - k as u16) / 255) as u8,
                        ((255 - m as u16) * (255 - k as u16) / 255) as u8,
                        ((255 - yk as u16) * (255 - k as u16) / 255) as u8,
                    ]
                }
                ColorSpace::Indexed { .. } => unreachable!(),
            }
        };
        match channels {
            1 => pixels.push(rgb[0]),
            3 => pixels.extend_from_slice(&rgb),
            _ => {
                pixels.extend_from_slice(&rgb);
                pixels.push(match mask_pixels.next() {
                    Some(alpha) => alpha.0[0],
                    None => 255,
                });
            }
        }
    }
    let bad = || "像素数量与宽高不符".to_string();
    Ok(match channels {
        1 => DynamicImage::ImageLuma8(GrayImage::from_raw(width, height, pixels).ok_or_else(bad)?),
        3 => DynamicImage::ImageRgb8(RgbImage::from_raw(width, height, pixels).ok_or_else(bad)?),
        _ => DynamicImage::ImageRgba8(RgbaImage::from_raw(width, height, pixels).ok_or_else(bad)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;
    use std::path::{Path, PathBuf};

    fn name(value: &str) -> Object {
        Object::Name(value.as_bytes().to_vec())
    }

    fn base_dict(width: i64, height: i64) -> Dictionary {
        let mut dict = Dictionary::new();
        dict.set("Width", Object::Integer(width));
        dict.set("Height", Object::Integer(height));
        dict.set("BitsPerComponent", Object::Integer(8));
        dict.set("ColorSpace", name("DeviceRGB"));
        dict
    }

    /// 用 lopdf 自己生成结构（含 xref），测试不必手写页树，避免 fixture 比被测代码更脆弱
    struct Fixture {
        doc: Document,
        xobjects: Vec<(String, ObjectId)>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                doc: Document::new(),
                xobjects: Vec::new(),
            }
        }

        fn stream(&mut self, dict: Dictionary, content: Vec<u8>) -> ObjectId {
            let mut dict = dict;
            dict.set("Type", name("XObject"));
            dict.set("Subtype", name("Image"));
            self.doc
                .add_object(Object::Stream(Stream::new(dict, content)))
        }

        fn image(&mut self, key: &str, dict: Dictionary, content: Vec<u8>) -> ObjectId {
            let id = self.stream(dict, content);
            self.xobjects.push((key.to_string(), id));
            id
        }

        /// 把已经建好的对象包进一个 Form，Form 自己挂在页面资源上。
        /// Form 是流对象而不是普通字典，正好用来卡住"Resources 取不到"那条分支。
        fn form(&mut self, key: &str, inner: &[(&str, ObjectId)]) -> ObjectId {
            let mut xobjects = Dictionary::new();
            for (inner_key, id) in inner {
                xobjects.set(inner_key.as_bytes().to_vec(), Object::Reference(*id));
            }
            let mut resources = Dictionary::new();
            resources.set("XObject", Object::Dictionary(xobjects));
            let resources = self.doc.add_object(Object::Dictionary(resources));
            let mut dict = Dictionary::new();
            dict.set("Type", name("XObject"));
            dict.set("Subtype", name("Form"));
            dict.set(
                "BBox",
                Object::Array(vec![
                    Object::Integer(0),
                    Object::Integer(0),
                    Object::Integer(100),
                    Object::Integer(100),
                ]),
            );
            dict.set("Resources", Object::Reference(resources));
            let id = self
                .doc
                .add_object(Object::Stream(Stream::new(dict, Vec::new())));
            self.xobjects.push((key.to_string(), id));
            id
        }

        fn save(&mut self, dir: &Path, file_name: &str) -> PathBuf {
            let mut resources = Dictionary::new();
            let mut xobjects = Dictionary::new();
            for (key, id) in &self.xobjects {
                xobjects.set(key.as_bytes().to_vec(), Object::Reference(*id));
            }
            resources.set("XObject", Object::Dictionary(xobjects));
            let resources = self.doc.add_object(Object::Dictionary(resources));
            let content = self.doc.add_object(Object::Stream(Stream::new(
                Dictionary::new(),
                b"q 100 0 0 100 10 10 cm /Im0 Do Q".to_vec(),
            )));
            let page = self.doc.add_object(Object::Dictionary({
                let mut dict = Dictionary::new();
                dict.set("Type", name("Page"));
                dict.set(
                    "MediaBox",
                    Object::Array(vec![
                        Object::Integer(0),
                        Object::Integer(0),
                        Object::Integer(200),
                        Object::Integer(200),
                    ]),
                );
                dict.set("Resources", Object::Reference(resources));
                dict.set("Contents", Object::Reference(content));
                dict
            }));
            let pages = self.doc.add_object(Object::Dictionary({
                let mut dict = Dictionary::new();
                dict.set("Type", name("Pages"));
                dict.set("Kids", Object::Array(vec![Object::Reference(page)]));
                dict.set("Count", Object::Integer(1));
                dict
            }));
            let catalog = self.doc.add_object(Object::Dictionary({
                let mut dict = Dictionary::new();
                dict.set("Type", name("Catalog"));
                dict.set("Pages", Object::Reference(pages));
                dict
            }));
            self.doc.trailer.set("Root", Object::Reference(catalog));
            let path = dir.join(file_name);
            self.doc.save(&path).unwrap();
            path
        }
    }

    /// 跑命令并保证输出目录确实是命令建出来的
    fn extract(dir: &Path, file: &Path) -> ExtractReport {
        let out = dir.join(format!(
            "{}-out",
            file.file_stem().unwrap().to_string_lossy()
        ));
        assert!(!out.exists(), "输出目录应该由命令自己创建");
        let report = extract_pdf_images_blocking(
            file.to_string_lossy().to_string(),
            out.to_string_lossy().to_string(),
        )
        .unwrap();
        assert!(out.exists());
        report
    }

    fn flate(input: &[u8]) -> Vec<u8> {
        use flate2::write::ZlibEncoder;
        use std::io::Write;
        let mut encoder = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(input).unwrap();
        encoder.finish().unwrap()
    }

    /// 8-bit RGB + /SMask：掩码必须真的改变 alpha，否则测试没测到那条分支
    #[test]
    fn rgb_image_with_smask_keeps_alpha() {
        let dir = crate::test_support::TempDir::new("pdfimg-rgb");
        let mut fixture = Fixture::new();
        let mut mask_dict = base_dict(2, 2);
        mask_dict.remove(b"ColorSpace");
        mask_dict.set("ColorSpace", name("DeviceGray"));
        let mask = fixture.stream(mask_dict, vec![255u8, 0, 128, 255]);

        let mut rgb = base_dict(2, 2);
        rgb.set("Filter", name("FlateDecode"));
        rgb.set("SMask", Object::Reference(mask));
        fixture.image(
            "Im0",
            rgb,
            flate(&[255, 0, 0, 0, 255, 0, 0, 0, 255, 20, 20, 20]),
        );
        let file = fixture.save(&dir, "rgb.pdf");

        let report = extract(&dir, &file);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(report.images.len(), 1);
        let rgba = image::open(&report.images[0]).unwrap().to_rgba8();
        assert_eq!((rgba.width(), rgba.height()), (2, 2));
        assert_eq!(rgba.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(rgba.get_pixel(1, 0).0, [0, 255, 0, 0]);
        assert_eq!(rgba.get_pixel(0, 1).0, [0, 0, 255, 128]);
    }

    /// 图只挂在 Form XObject 的资源里（一份 92 图的书里有 29 张这样）。
    /// 只扫页面 Resources 会整批漏图，且 lopdf 的 `as_dict` 不认流对象，
    /// 所以 Form 的 Resources 必须走 dict_of —— 这条用 as_dict 会静默返回 None。
    #[test]
    fn images_hidden_inside_a_form_xobject_are_found() {
        let dir = crate::test_support::TempDir::new("pdfimg-form");
        let mut fixture = Fixture::new();
        let mut gray = base_dict(2, 1);
        gray.remove(b"ColorSpace");
        gray.set("ColorSpace", name("DeviceGray"));
        gray.set("Filter", name("FlateDecode"));
        let hidden = fixture.stream(gray, flate(&[10u8, 200]));
        fixture.form("Fm0", &[("Im0", hidden)]);
        let file = fixture.save(&dir, "form.pdf");

        let report = extract(&dir, &file);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(report.images.len(), 1, "Form 里的图没被走到");
        let rgba = image::open(&report.images[0]).unwrap().to_rgba8();
        assert_eq!(rgba.get_pixel(0, 0).0, [10, 10, 10, 255]);
        assert_eq!(rgba.get_pixel(1, 0).0, [200, 200, 200, 255]);
    }

    /// Form 套 Form：最里层的图也要挖出来。再让内层 Form 反向引用外层 Form 构成环，
    /// 靠 visited 收敛——没有它这条递归会一直走下去
    #[test]
    fn nested_forms_are_walked_and_back_edges_do_not_loop() {
        let dir = crate::test_support::TempDir::new("pdfimg-deep");
        let mut fixture = Fixture::new();
        let mut gray = base_dict(1, 1);
        gray.remove(b"ColorSpace");
        gray.set("ColorSpace", name("DeviceGray"));
        let deep = fixture.stream(gray, vec![77u8]);
        let middle = fixture.form("Fm1", &[("Im0", deep)]);
        let outer = fixture.form("Fm0", &[("Fm1", middle)]);
        let resources = match fixture.doc.get_object(middle) {
            Ok(Object::Stream(stream)) => stream
                .dict
                .get(b"Resources")
                .and_then(Object::as_reference)
                .unwrap(),
            _ => panic!("Form 应该是流对象"),
        };
        let dict = fixture
            .doc
            .get_object_mut(resources)
            .unwrap()
            .as_dict_mut()
            .unwrap();
        if let Ok(Object::Dictionary(xobjects)) = dict.get_mut(b"XObject") {
            xobjects.set(b"Fm0".to_vec(), Object::Reference(outer));
        } else {
            panic!("Form 的 XObject 应该是字典");
        }
        let file = fixture.save(&dir, "deep.pdf");

        let report = extract(&dir, &file);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(report.images.len(), 1);
        let rgba = image::open(&report.images[0]).unwrap().to_rgba8();
        assert_eq!(rgba.get_pixel(0, 0).0, [77, 77, 77, 255]);
    }

    /// ICC 颜色空间的参数按规范是"描述文件流"（通道数在流字典的 /N 上），Cal* 则根本没有
    /// 参数可查。只认字典会让一份书稿语料里 24 张 ICC 图全报"参数不是字典"。
    #[test]
    fn icc_profile_streams_and_cal_colorspaces_decode() {
        let dir = crate::test_support::TempDir::new("pdfimg-icc");
        let mut fixture = Fixture::new();

        let mut profile = Dictionary::new();
        profile.set("N", Object::Integer(3));
        let profile = fixture
            .doc
            .add_object(Object::Stream(Stream::new(profile, Vec::new())));
        let mut rgb = base_dict(1, 1);
        rgb.set(
            "ColorSpace",
            Object::Array(vec![name("ICCBased"), Object::Reference(profile)]),
        );
        fixture.image("Im0", rgb, vec![9u8, 8, 7]);

        let mut cal = base_dict(1, 1);
        cal.remove(b"ColorSpace");
        let mut white = Vec::new();
        for value in [0.9505f32, 1.0, 1.0890] {
            white.push(Object::Real(value));
        }
        let mut params = Dictionary::new();
        params.set("WhitePoint", Object::Array(white));
        cal.set(
            "ColorSpace",
            Object::Array(vec![name("CalGray"), Object::Dictionary(params)]),
        );
        fixture.image("Im1", cal, vec![123u8]);
        let file = fixture.save(&dir, "icc.pdf");

        let report = extract(&dir, &file);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(report.images.len(), 2);
        let rgb_out = image::open(&report.images[0]).unwrap().to_rgba8();
        assert_eq!(rgb_out.get_pixel(0, 0).0, [9, 8, 7, 255]);
        let gray_out = image::open(&report.images[1]).unwrap().to_rgba8();
        assert_eq!(gray_out.get_pixel(0, 0).0, [123, 123, 123, 255]);
    }

    /// 1-bit 图：每行按字节补齐，且 0/1 要铺成 0/255，否则解出来是一整块黑
    #[test]
    fn one_bit_rows_are_byte_aligned() {
        let dir = crate::test_support::TempDir::new("pdfimg-1bit");
        let mut dict = base_dict(6, 2);
        dict.set("BitsPerComponent", Object::Integer(1));
        dict.remove(b"ColorSpace");
        dict.set("ColorSpace", name("DeviceGray"));
        let mut fixture = Fixture::new();
        // 6 像素宽 = 6 bit/行，行尾补两个 0 才凑成整字节
        fixture.image("Im0", dict, vec![0b1100_1000u8, 0b0001_1100]);
        let file = fixture.save(&dir, "gray1.pdf");

        let report = extract(&dir, &file);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        let rgba = image::open(&report.images[0]).unwrap().to_rgba8();
        assert_eq!(
            (0..6)
                .map(|x| rgba.get_pixel(x, 0).0[0])
                .collect::<Vec<_>>(),
            vec![255, 255, 0, 0, 255, 0]
        );
        assert_eq!(
            (0..6)
                .map(|x| rgba.get_pixel(x, 1).0[0])
                .collect::<Vec<_>>(),
            vec![0, 0, 0, 255, 255, 255]
        );
    }

    /// 只有 /ImageMask 没有 /ColorSpace 的轮廓图：默认 1 bit、默认 Decode 反相（1 表示着色）
    #[test]
    fn stencil_mask_without_colorspace_paints_black() {
        let dir = crate::test_support::TempDir::new("pdfimg-stencil");
        let mut dict = Dictionary::new();
        dict.set("Width", Object::Integer(2));
        dict.set("Height", Object::Integer(1));
        dict.set("ImageMask", Object::Boolean(true));
        dict.set("BitsPerComponent", Object::Integer(1));
        let mut fixture = Fixture::new();
        fixture.image("Im0", dict, vec![0b1000_0000u8]);
        let file = fixture.save(&dir, "stencil.pdf");

        let report = extract(&dir, &file);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        let rgba = image::open(&report.images[0]).unwrap().to_rgba8();
        assert_eq!(rgba.get_pixel(0, 0).0[0], 0, "1 位应着色成黑");
        assert_eq!(rgba.get_pixel(1, 0).0[0], 255, "0 位应是白");
    }

    /// PNG 预测器：每行开头一个滤镜字节，Sub/Up 都要还原，否则整幅是噪声
    #[test]
    fn png_predictor_rows_are_rebuilt() {
        let dir = crate::test_support::TempDir::new("pdfimg-predictor");
        let mut params = Dictionary::new();
        params.set("Predictor", Object::Integer(15));
        params.set("Colors", Object::Integer(1));
        params.set("BitsPerComponent", Object::Integer(8));
        params.set("Columns", Object::Integer(2));
        let mut dict = base_dict(2, 2);
        dict.remove(b"ColorSpace");
        dict.set("ColorSpace", name("DeviceGray"));
        dict.set("DecodeParms", Object::Dictionary(params));
        let mut fixture = Fixture::new();
        fixture.image(
            "Im0",
            dict,
            vec![
                1u8, 10, 20, // Sub：10，20+10
                2, 5, 5, // Up：10+5，30+5
            ],
        );
        let file = fixture.save(&dir, "pred.pdf");

        let report = extract(&dir, &file);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        let rgba = image::open(&report.images[0]).unwrap().to_rgba8();
        assert_eq!(rgba.get_pixel(0, 0).0, [10, 10, 10, 255]);
        assert_eq!(rgba.get_pixel(1, 0).0, [30, 30, 30, 255]);
        assert_eq!(rgba.get_pixel(0, 1).0, [15, 15, 15, 255]);
        assert_eq!(rgba.get_pixel(1, 1).0, [35, 35, 35, 255]);
    }

    /// 内联调色板：索引查表，越界索引夹到 hival；表短了也不能 panic
    #[test]
    fn indexed_palette_clamps_and_survives_short_table() {
        let dir = crate::test_support::TempDir::new("pdfimg-indexed");
        let mut dict = base_dict(3, 1);
        dict.remove(b"ColorSpace");
        dict.set(
            "ColorSpace",
            Object::Array(vec![
                name("Indexed"),
                name("DeviceRGB"),
                Object::Integer(1),
                Object::Array(vec![
                    Object::Integer(255),
                    Object::Integer(0),
                    Object::Integer(0),
                    Object::Integer(0),
                    Object::Integer(0),
                    Object::Integer(255),
                ]),
            ]),
        );
        let mut fixture = Fixture::new();
        // 索引 5 越界，必须夹到 hival=1 而不是炸掉
        fixture.image("Im0", dict, vec![0u8, 1, 5]);
        let file = fixture.save(&dir, "idx.pdf");

        let report = extract(&dir, &file);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        let rgba = image::open(&report.images[0]).unwrap().to_rgba8();
        assert_eq!(rgba.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(rgba.get_pixel(1, 0).0, [0, 0, 255, 255]);
        assert_eq!(rgba.get_pixel(2, 0).0, [0, 0, 255, 255]);
    }

    /// JPEG 嵌图透传：转 PNG 是二次损失，体积还会涨
    #[test]
    fn jpeg_images_pass_through_untouched() {
        let dir = crate::test_support::TempDir::new("pdfimg-jpeg");
        let jpeg = {
            let buffer = RgbaImage::from_pixel(4, 3, Rgba([9, 8, 7, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buffer)
                .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Jpeg)
                .unwrap();
            bytes
        };
        let mut dict = base_dict(4, 3);
        dict.set("Filter", name("DCTDecode"));
        let mut fixture = Fixture::new();
        fixture.image("Im0", dict, jpeg.clone());
        let file = fixture.save(&dir, "dct.pdf");

        let report = extract(&dir, &file);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert!(report.images[0].ends_with(".jpg"), "{}", report.images[0]);
        assert_eq!(fs::read(&report.images[0]).unwrap(), jpeg);
    }

    /// 旧式扫描压缩本机没有解码器：要给出带图名的原因，不能假装"提取完成 0 张"
    #[test]
    fn ccitt_images_are_reported_not_silently_dropped() {
        let dir = crate::test_support::TempDir::new("pdfimg-ccitt");
        let mut dict = base_dict(2, 2);
        dict.set("Filter", name("CCITTFaxDecode"));
        let mut fixture = Fixture::new();
        fixture.image("ImFax", dict, vec![0u8; 8]);
        let file = fixture.save(&dir, "ccitt.pdf");

        let report = extract(&dir, &file);
        assert!(report.images.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert!(
            report.skipped[0].contains("CCITTFaxDecode"),
            "{:?}",
            report.skipped
        );
        assert!(report.skipped[0].contains("ImFax"), "{:?}", report.skipped);
    }

    /// /SMask 指回自己（损坏或构造的环）不能把提取变成无限递归
    #[test]
    fn self_referencing_smask_terminates() {
        let dir = crate::test_support::TempDir::new("pdfimg-smask-loop");
        let mut fixture = Fixture::new();
        let id = fixture.image("Im0", base_dict(1, 1), vec![0u8, 0, 0]);
        let Object::Stream(stream) = fixture.doc.get_object_mut(id).unwrap() else {
            panic!("fixture 里应该是流")
        };
        stream.dict.set("SMask", Object::Reference(id));
        let file = fixture.save(&dir, "loop.pdf");

        let report = extract(&dir, &file);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        let rgba = image::open(&report.images[0]).unwrap().to_rgba8();
        assert_eq!(
            rgba.get_pixel(0, 0).0,
            [0, 0, 0, 0],
            "掩码取自身第一通道（0），递归在第二层被切断"
        );
    }

    /// 宽高是文件里读的，假尺寸必须先夹住，不能拿它去算分配
    #[test]
    fn absurd_dimensions_are_refused() {
        let dir = crate::test_support::TempDir::new("pdfimg-huge");
        let mut fixture = Fixture::new();
        fixture.image("Im0", base_dict(100_000, 100_000), vec![0u8; 16]);
        let file = fixture.save(&dir, "huge.pdf");

        let report = extract(&dir, &file);
        assert!(report.images.is_empty());
        assert!(
            report.skipped[0].contains("尺寸异常"),
            "{:?}",
            report.skipped
        );
    }

    /// 没有任何图的 PDF：命令要成功返回空清单，而不是报错
    #[test]
    fn pdf_without_images_is_an_empty_success() {
        let dir = crate::test_support::TempDir::new("pdfimg-none");
        let mut fixture = Fixture::new();
        let file = fixture.save(&dir, "empty.pdf");
        let report = extract(&dir, &file);
        assert!(report.images.is_empty());
        assert!(report.skipped.is_empty());
    }
}
