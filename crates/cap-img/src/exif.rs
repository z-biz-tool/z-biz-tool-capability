//! EXIF 全量读写。
//!
//! ## 为什么是这个形状
//!
//! 本模块此前不存在 —— cap-img 里只有一份手写的 `parse_exif_minimal`：
//! 自己扫 JPEG 的 APP1 段，自己解 TIFF 的 IFD0，只认 Make/Model/Software/
//! DateTime/Copyright 五个 ASCII 标签，而且是**标签字符串**不是结构化值。
//!
//! 那份实现有三个硬伤：
//!   1. **覆盖率约 5%**。光圈、快门、ISO、焦距、GPS、Orientation、镜头型号
//!      —— EXIF 里真正有用的字段一个都读不出来。
//!   2. **只认 IFD0**。Exif SubIFD（在 JPEG 里装着曝光参数的那个 IFD）与
//!      GPS IFD 都在更深处，全被跳过。
//!   3. **只读不写**。用户隐私场景要「清除 GPS」时只能整块丢弃 EXIF，
//!      连带丢掉拍摄参数。
//!
//! 与此同时 file 仓另有一份基于 `kamadak-exif` 的私有实现（`image_exif.rs`），
//! 读得到光圈/GPS 却同样只读。同一件事两套实现 ⇒ 本模块统一吃 `kamadak-exif`，
//! 消灭手写解析，也与 file 侧对齐。
//!
//! ## 设计约定
//!
//! - **类型化的值**：EXIF 的值有 10 种类型（ASCII/Rational/Short/Long/…），
//!   全部压成字符串就丢掉了「这是光圈还是焦距」的信息。本模块保留类型，
//!   同时给出**人类可读**的显示值（`"1/250"`、`"2.8"`）。
//! - **不丢未知 tag**：写回时原样保留读不到的 MakerNote 等私有数据。
//! - **IFD 归属用 Context 判定**：`exif::Tag` 自带 `Context`
//!   （Tiff / Exif / Gps / Interop），这正是 IFD 归属的真值 —— 不靠标签名字猜。

use ::exif::{Context, Exif, Field, In, Tag, Value};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// EXIF 值的类型标签（对应 `exif::Value` 的各个变体）。
///
/// 前端据此决定怎么渲染：ASCII 是字符串，Rational 是分数，Undefined 是二进制块。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExifValueKind {
    /// 7-bit ASCII 字符串（可能是多段，如多行注释）
    Ascii,
    /// 无符号有理数（曝光时间、光圈…）
    Rational,
    /// 16 位无符号整数
    Short,
    /// 32 位无符号整数
    Long,
    /// 8 位无符号整数
    Byte,
    /// 8 位有符号整数
    SByte,
    /// 16 位有符号整数
    SShort,
    /// 32 位有符号整数
    SLong,
    /// 8 位有符号浮点（单精度）
    Float,
    /// 64 位有符号浮点（双精度）
    Double,
    /// 带符号有理数
    SRational,
    /// 未定义字节块（MakerNote / 缩略图等）
    Undefined,
}

impl ExifValueKind {
    /// 从 kamadak-exif 的 `Value` 变体判定类型标签。
    fn of(value: &Value) -> Self {
        match value {
            Value::Ascii(_) => Self::Ascii,
            Value::Rational(_) => Self::Rational,
            Value::Short(_) => Self::Short,
            Value::Long(_) => Self::Long,
            Value::Byte(_) => Self::Byte,
            Value::SByte(_) => Self::SByte,
            Value::SShort(_) => Self::SShort,
            Value::SLong(_) => Self::SLong,
            Value::Float(_) => Self::Float,
            Value::Double(_) => Self::Double,
            Value::SRational(_) => Self::SRational,
            Value::Undefined(..) => Self::Undefined,
            // 未知类型按字节块处理，至少不丢数据
            Value::Unknown(..) => Self::Undefined,
        }
    }
}

/// 单个 EXIF 字段的完整表示。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExifField {
    /// 标签名（如 `Make`、`FNumber`、`GPSLatitude`）；未知标签退化为 `Tag_0x1234`
    pub name: String,
    /// 标签的十六进制值（如 `0x010f`）；名字可能重复，十六进制才唯一
    pub tag: u32,
    /// 值类型
    pub kind: ExifValueKind,
    /// 人类可读的显示值（`"1/250"`、`"2.8"`、`"Canon"`）
    pub display: String,
    /// 原始类型化值（字符串数组 / 数字数组 / 字节数组）
    ///
    /// Rational 存成 `"num/den"` 字符串以免 JSON 里出现浮点精度损失。
    pub values: Vec<ExifValue>,
}

/// 一个 EXIF 值的类型化表示（JSON 友好）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum ExifValue {
    /// 文本
    Text(String),
    /// 整数
    Int(i64),
    /// 有理数 `num/den`
    Rational(String),
    /// 浮点
    Float(f64),
    /// 字节（十六进制字符串）
    Bytes(String),
}

/// 一个 IFD 的内容（IFD0 / Exif SubIFD / GPS IFD）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExifIfd {
    /// 该 IFD 下的字段（按标签十六进制升序，读取稳定）
    pub fields: Vec<ExifField>,
}

/// EXIF 的完整快照。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExifData {
    /// 主 IFD（IFD0）：机型、方向、分辨率等
    pub primary: ExifIfd,
    /// Exif SubIFD：曝光、光圈、ISO、焦距、镜头等
    pub exif: ExifIfd,
    /// GPS IFD：经纬度、海拔、定位时间等
    pub gps: ExifIfd,
    /// 缩略图 IFD（IFD1）：相机内嵌的那张小图
    pub thumbnail: ExifIfd,
    /// 是否小端字节序（TIFF 头里的 `II` / `MM`）
    pub little_endian: bool,
}

impl ExifData {
    /// 是否**完全没有**任何字段。
    pub fn is_empty(&self) -> bool {
        self.primary.fields.is_empty()
            && self.exif.fields.is_empty()
            && self.gps.fields.is_empty()
            && self.thumbnail.fields.is_empty()
    }

    /// 字段总数（跨全部 IFD）。
    pub fn field_count(&self) -> usize {
        self.primary.fields.len()
            + self.exif.fields.len()
            + self.gps.fields.len()
            + self.thumbnail.fields.len()
    }

    /// 在全部 IFD 里按标签名找第一个字段（不区分大小写）。
    pub fn find(&self, name: &str) -> Option<&ExifField> {
        let want = name.to_ascii_lowercase();
        [
            &self.primary.fields,
            &self.exif.fields,
            &self.gps.fields,
            &self.thumbnail.fields,
        ]
        .iter()
        .flat_map(|fields| fields.iter())
        .find(|f| f.name.to_ascii_lowercase() == want)
    }

    /// 按标签名取显示值（找不到返回 None）。
    pub fn display(&self, name: &str) -> Option<&str> {
        self.find(name).map(|f| f.display.as_str())
    }

    /// 便捷取值：按标签名取单个数值（字符串型原样返回，数值型转字符串）。
    ///
    /// 这层糖是为了让调用方不必关心字段落在哪个 IFD —— `ExposureTime` 在 Exif
    /// SubIFD，`Make` 在 IFD0，用户视角都是「一个字段」。
    pub fn value(&self, name: &str) -> Option<String> {
        let f = self.find(name)?;
        f.values.first().map(|v| match v {
            ExifValue::Text(s) => s.clone(),
            ExifValue::Int(i) => i.to_string(),
            ExifValue::Rational(r) => r.clone(),
            ExifValue::Float(x) => x.to_string(),
            ExifValue::Bytes(b) => b.clone(),
        })
    }

    /// 取出 GPS 十进制度坐标（若有）。
    ///
    /// EXIF 里 GPS 坐标是 DMS（度/分/秒）三段有理数，还要配合 `*Ref` 判南北
    /// 东西；直接当数字读会得到 `39/1 54/1 0/1` 这样的原始值。
    pub fn gps_coordinates(&self) -> Option<(f64, f64)> {
        let lat = self.dms_to_decimal("GPSLatitude", "GPSLatitudeRef")?;
        let lon = self.dms_to_decimal("GPSLongitude", "GPSLongitudeRef")?;
        Some((lat, lon))
    }

    /// 把 DMS 字段转成十进制度。
    fn dms_to_decimal(&self, value_tag: &str, ref_tag: &str) -> Option<f64> {
        let field = self.find(value_tag)?;
        let parts: Vec<f64> = field
            .values
            .iter()
            .filter_map(|v| match v {
                ExifValue::Rational(r) => {
                    let (n, d) = r.split_once('/')?;
                    let n: f64 = n.trim().parse().ok()?;
                    let d: f64 = d.trim().parse().ok()?;
                    if d == 0.0 {
                        None
                    } else {
                        Some(n / d)
                    }
                }
                ExifValue::Int(i) => Some(*i as f64),
                ExifValue::Float(f) => Some(*f),
                _ => None,
            })
            .collect();
        if parts.len() < 2 {
            return None;
        }
        let deg = parts[0];
        let min = parts[1];
        let sec = parts.get(2).copied().unwrap_or(0.0);
        let mut dec = deg + min / 60.0 + sec / 3600.0;

        // Ref 判方向：S / W 取负
        if let Some(r) = self.find(ref_tag) {
            let text = match r.values.first() {
                Some(ExifValue::Text(s)) => s.trim().to_ascii_uppercase(),
                Some(ExifValue::Bytes(b)) => String::from_utf8_lossy(&hex_to_bytes(b)?)
                    .trim()
                    .to_ascii_uppercase(),
                _ => String::new(),
            };
            if text.starts_with('S') || text.starts_with('W') {
                dec = -dec;
            }
        }
        Some(dec)
    }
}

/// 读取 EXIF 的原始容器数据。
///
/// 支持 JPEG / TIFF / PNG / WebP（kamadak-exif 自带这几种容器的解析）。
/// 返回 `Ok(None)` 表示**文件里没有 EXIF**，不是错误。
///
/// 「没有 EXIF」与「解析失败」是两件事：
///   - 没有 EXIF ⇒ `Ok(None)`（一张普通 PNG 就是这样）
///   - 有 EXIF 但坏了 ⇒ `Err`（数据损坏，得告诉用户）
///
/// kamadak-exif 对这两种情况都返回 `Err`（`No Exif data found in JPEG`），
/// 所以这里靠错误文案区分 —— 判据是「错误消息里有没有说找不到 EXIF」。
pub fn read_container(path: impl AsRef<Path>) -> Result<Option<Exif>, String> {
    let path = path.as_ref();
    let file = fs::File::open(path).map_err(|e| format!("打开文件失败: {}", e))?;
    let mut reader = std::io::BufReader::new(file);
    match ::exif::Reader::new().read_from_container(&mut reader) {
        Ok(exif) => Ok(Some(exif)),
        Err(e) => {
            let msg = e.to_string();
            // 「容器里没有 EXIF」不是错误，是常态（一张没元数据的普通图片）
            if msg.contains("No Exif data found") || msg.contains("no Exif data") {
                Ok(None)
            } else {
                Err(format!("解析 EXIF 失败: {}", e))
            }
        }
    }
}

/// 把 kamadak-exif 的 `Exif` 归一化成我们的结构化快照。
///
/// 这一步做三件事：
///   1. 按 `Tag::context()` 分桶（Tiff → IFD0，Exif → SubIFD，Gps → GPS IFD）
///   2. 每个值拆成类型化表示 + 人类可读显示值
///   3. 字段按标签十六进制排序 ⇒ 同一张图两次读出的字节序完全一致（可对账）
pub fn normalize(exif: &Exif) -> ExifData {
    let mut data = ExifData {
        little_endian: exif.little_endian(),
        ..Default::default()
    };

    let mut primary = Vec::new();
    let mut gps = Vec::new();
    let mut sub = Vec::new();
    let mut thumbnail = Vec::new();

    for field in exif.fields() {
        let converted = convert_field(field);
        match field.tag.context() {
            Context::Gps => gps.push(converted),
            Context::Exif => sub.push(converted),
            Context::Tiff => {
                // IFD1（缩略图）由 `In::THUMBNAIL` 标记
                if field.ifd_num == In::THUMBNAIL {
                    thumbnail.push(converted);
                } else {
                    primary.push(converted);
                }
            }
            Context::Interop => sub.push(converted),
            // Context 是 #[non_exhaustive]，未来可能加新上下文
            _ => primary.push(converted),
        }
    }

    primary.sort_by_key(|f| f.tag);
    gps.sort_by_key(|f| f.tag);
    sub.sort_by_key(|f| f.tag);
    thumbnail.sort_by_key(|f| f.tag);
    data.primary.fields = primary;
    data.gps.fields = gps;
    data.exif.fields = sub;
    data.thumbnail.fields = thumbnail;

    data
}

/// 读取图片的完整 EXIF 快照；文件没有 EXIF 时返回 `Ok(None)`。
pub fn read(path: impl AsRef<Path>) -> Result<Option<ExifData>, String> {
    match read_container(path)? {
        Some(exif) => Ok(Some(normalize(&exif))),
        None => Ok(None),
    }
}

/// 读取图片的 EXIF 快照；没有 EXIF 时返回一个空的（`field_count()==0`）快照。
pub fn read_or_empty(path: impl AsRef<Path>) -> Result<ExifData, String> {
    Ok(read(path)?.unwrap_or_default())
}

/// 读取图片的 EXIF 并返回 JSON 字符串（前端直接吃）。
pub fn read_json(path: impl AsRef<Path>) -> Result<Option<String>, String> {
    match read(path)? {
        Some(data) => serde_json::to_string(&data)
            .map_err(|e| format!("序列化 EXIF 失败: {}", e))
            .map(Some),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// 写入
// ---------------------------------------------------------------------------

/// 一次 EXIF 写入操作。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExifEdit {
    /// 要写入/覆盖的字段（按标签名或标签值定位）
    #[serde(default)]
    pub set: Vec<ExifField>,
    /// 要删除的标签（名字或十六进制串，如 `"GPSLatitude"` 或 `"0x8825"`）
    #[serde(default)]
    pub remove: Vec<String>,
}

/// 写入 EXIF。
///
/// 语义（与「全量替换」相对）：
///   - 源文件**没有** EXIF ⇒ 新建一个，只写 `set` 里的字段。
///   - 源文件**有** EXIF ⇒ 在原基础上改：`set` 覆盖同名标签，其余原样保留
///     （包括 MakerNote 这类我们不认识的 tag），`remove` 里的标签被剔除。
///
/// 输出路径的扩展名决定容器格式（JPEG/TIFF/PNG/WebP 均可写）。
pub fn write(
    src: impl AsRef<Path>,
    dest_path: impl AsRef<Path>,
    edit: &ExifEdit,
) -> Result<(), String> {
    let src = src.as_ref();
    let dest = dest_path.as_ref();

    // 读出原 EXIF（可能没有）
    let original = read_container(src)?;

    // 构造要写的字段表：原字段 + 编辑指令
    let mut fields: Vec<Field> = match &original {
        Some(exif) => exif.fields().cloned().collect(),
        None => Vec::new(),
    };

    // 先删除
    if !edit.remove.is_empty() {
        fields.retain(|f| !matches_remove(f, &edit.remove));
    }

    // 再设置（同名覆盖）
    for want in &edit.set {
        let tag = resolve_tag(&want.name, want.tag)?;
        let value = build_value(&want.kind, &want.values)?;
        // 覆盖：先删掉同标签的旧值（Context 也须一致，否则 IFD0 的 0x8769
        // 指针与 SubIFD 的同号标签会互相覆盖）
        fields.retain(|f| !(f.tag == tag && f.tag.context() == tag.context()));
        fields.push(Field {
            tag,
            ifd_num: In::PRIMARY,
            value,
        });
    }

    // 像素先复制到目标（EXIF 注入是**在目标文件上改字节**，不改像素）
    copy_image(src, dest)?;

    let fields = if fields.is_empty() {
        // 全被删光 ⇒ 不写 EXIF 段
        Vec::new()
    } else {
        fields
    };

    write_fields(dest, &fields, original.as_ref().map(|e| e.little_endian()))
}

/// 清除图片的全部 EXIF（写出无 EXIF 的副本）。
///
/// 这是隐私刚需：分享照片前抹掉 GPS 与机型。
///
/// 同样是**字节级**操作：只摘掉容器里的 EXIF 段，像素字节一个不动
/// （走 image crate 重编码会顺手把图重压一遍，画质悄悄掉一档）。
pub fn strip(src: impl AsRef<Path>, dest_path: impl AsRef<Path>) -> Result<(), String> {
    let src = src.as_ref();
    let dest = dest_path.as_ref();

    copy_image(src, dest)?;

    // 目标格式不同 ⇒ 容器结构不同，只能重新编码（这时像素本来就会变）
    let same_format = crate::detect_format(src) == crate::detect_format(dest);
    if !same_format {
        return Ok(());
    }

    match crate::parse_format(&crate::detect_format(dest))? {
        image::ImageFormat::Jpeg => remove_jpeg_exif(dest),
        // PNG/WebP 的 EXIF 在专用 chunk 里，摘掉即可
        image::ImageFormat::Png => remove_png_exif(dest),
        image::ImageFormat::WebP => remove_webp_exif(dest),
        // TIFF 的 EXIF 就是整个文件 ⇒ 无 EXIF 版本没法从 TIFF 派生
        other => Err(format!(
            "暂不支持清除 {:?} 的 EXIF（支持: JPEG/PNG/WebP）",
            other
        )),
    }
}

/// 把源图复制到目标。
///
/// 源与目标**格式相同时字节级复制**（像素一个不动）；
/// 格式不同才重新编码（此时容器结构变了，本质上是另存为）。
fn copy_image(src: &Path, dest: &Path) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {}", e))?;
    }
    // **源与目标是同一个文件** ⇒ 不能复制（fs::copy 同路径会把源截断成空）。
    // 这种用法是「就地改 EXIF」，语义上直接改就行。
    if src == dest {
        return Ok(());
    }
    if crate::detect_format(src) == crate::detect_format(dest) {
        return fs::copy(src, dest)
            .map(|_| ())
            .map_err(|e| format!("复制文件失败: {}", e));
    }

    // 跨格式：解码再编码
    let img = image::open(src).map_err(|e| format!("打开图片失败: {}", e))?;
    let fmt = crate::parse_format(&crate::detect_format(dest))?;
    match fmt {
        image::ImageFormat::Jpeg => {
            let mut buf: Vec<u8> = Vec::new();
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 90);
            encoder
                .encode(
                    img.to_rgb8().as_raw(),
                    img.width(),
                    img.height(),
                    image::ExtendedColorType::Rgb8,
                )
                .map_err(|e| format!("编码 JPEG 失败: {}", e))?;
            fs::write(dest, &buf).map_err(|e| format!("写入文件失败: {}", e))?;
        }
        other => {
            img.save_with_format(dest, other)
                .map_err(|e| format!("保存图片失败: {}", e))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 内部实现
// ---------------------------------------------------------------------------

/// 把字段转成我们的结构化表示。
fn convert_field(field: &Field) -> ExifField {
    let kind = ExifValueKind::of(&field.value);
    let values = convert_value(&field.value);
    ExifField {
        name: tag_name(field.tag),
        tag: field.tag.number() as u32,
        kind,
        display: display_value(&field.value),
        values,
    }
}

/// 把 kamadak-exif 的 `Value` 转成 JSON 友好的类型化列表。
fn convert_value(value: &Value) -> Vec<ExifValue> {
    match value {
        Value::Ascii(strs) => strs
            .iter()
            .map(|s| {
                // ASCII 段以 NUL 结尾，kamadak-exif 已剥掉；这里再兜一层
                let text = String::from_utf8_lossy(s);
                ExifValue::Text(text.trim_end_matches('\0').to_string())
            })
            .collect(),
        Value::Rational(rs) => rs
            .iter()
            .map(|r| ExifValue::Rational(format!("{}/{}", r.num, r.denom)))
            .collect(),
        Value::Short(v) => v.iter().map(|x| ExifValue::Int(*x as i64)).collect(),
        Value::Long(v) => v.iter().map(|x| ExifValue::Int(*x as i64)).collect(),
        Value::Byte(v) => v.iter().map(|x| ExifValue::Int(*x as i64)).collect(),
        Value::SByte(v) => v.iter().map(|x| ExifValue::Int(*x as i64)).collect(),
        Value::SShort(v) => v.iter().map(|x| ExifValue::Int(*x as i64)).collect(),
        Value::SLong(v) => v.iter().map(|x| ExifValue::Int(*x as i64)).collect(),
        Value::Float(v) => v.iter().map(|x| ExifValue::Float(*x as f64)).collect(),
        Value::Double(v) => v.iter().map(|x| ExifValue::Float(*x)).collect(),
        Value::SRational(rs) => rs
            .iter()
            .map(|r| ExifValue::Rational(format!("{}/{}", r.num, r.denom)))
            .collect(),
        Value::Undefined(bytes, _) => vec![ExifValue::Bytes(to_hex(bytes))],
        Value::Unknown(ty, count, _) => {
            vec![ExifValue::Text(format!(
                "未知类型 {} (count={})",
                ty, count
            ))]
        }
    }
}

/// 生成人类可读的显示值。
///
/// 对 Rational 做「约分 + 小数化」的加工：
///   - 分母为 1 ⇒ 直接给整数（ISO 400）
///   - 分母是 10 的幂 ⇒ 给小数（光圈 28/10 ⇒ 2.8）
///   - 其余 ⇒ 给分数（曝光 1/250）
fn display_value(value: &Value) -> String {
    match value {
        Value::Ascii(strs) => strs
            .iter()
            .map(|s| {
                String::from_utf8_lossy(s)
                    .trim_end_matches('\0')
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join(", "),
        Value::Rational(rs) => rs
            .iter()
            .map(|r| {
                if r.denom == 0 {
                    return "∞".to_string();
                }
                if r.denom == 1 {
                    return r.num.to_string();
                }
                // 分母是 10 的幂 ⇒ 小数形式（28/10 ⇒ 2.8）
                let mut d = r.denom;
                let mut pow = 0u32;
                while d % 10 == 0 && d > 1 {
                    d /= 10;
                    pow += 1;
                }
                if d == 1 && pow > 0 {
                    let divisor = 10u64.pow(pow);
                    let v = r.num as f64 / divisor as f64;
                    let s = format!("{:.1}", v);
                    // 2.0 显示成 "2"，3.5 保持 "3.5"
                    return s.trim_end_matches('0').trim_end_matches('.').to_string();
                }
                // 可约分则约（2/4 ⇒ 1/2）
                let g = gcd(r.num, r.denom);
                if g > 1 {
                    format!("{}/{}", r.num / g, r.denom / g)
                } else {
                    format!("{}/{}", r.num, r.denom)
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
        Value::Short(v) => v
            .iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(", "),
        Value::Long(v) => v
            .iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(", "),
        Value::Byte(v) => v
            .iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(", "),
        Value::SByte(v) => v
            .iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(", "),
        Value::SShort(v) => v
            .iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(", "),
        Value::SLong(v) => v
            .iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(", "),
        Value::Float(v) => v
            .iter()
            .map(|x| format!("{:.4}", x))
            .collect::<Vec<_>>()
            .join(", "),
        Value::Double(v) => v
            .iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(", "),
        Value::SRational(rs) => rs
            .iter()
            .map(|r| {
                if r.denom == 0 {
                    return "∞".to_string();
                }
                format!("{}/{}", r.num, r.denom)
            })
            .collect::<Vec<_>>()
            .join(", "),
        Value::Undefined(bytes, _) => to_hex(bytes),
        Value::Unknown(ty, count, _) => format!("未知类型 {} (count={})", ty, count),
    }
}

/// 最大公约数。
fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    if a == 0 {
        1
    } else {
        a
    }
}

/// 字节数组转小写十六进制。
fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// 十六进制字符串转字节（解析失败返回 None）。
fn hex_to_bytes(hex: &str) -> Option<Vec<u8>> {
    let hex = hex.trim();
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let s = std::str::from_utf8(&bytes[i..i + 2]).ok()?;
        out.push(u8::from_str_radix(s, 16).ok()?);
    }
    Some(out)
}

/// 标签名（已知标签走 kamadak-exif 的字典，未知退化为 `Tag_0x…`）。
///
/// kamadak-exif 的 `Tag` 实现了 `Display`：已知标签给出规格里的名字，
/// 未知标签给出 Debug 形态。我们统一转成自己的命名风格。
fn tag_name(tag: Tag) -> String {
    let display = tag.to_string();
    // kamadak-exif 未知标签的 Display 形如 `Tag(Context::Tiff, 0x1234)`
    if display.starts_with("Tag(") {
        return format!("Tag_0x{:04x}", tag.number());
    }
    display
}

/// 由标签名反查 `Tag`（写回时用）。
///
/// `kamadak_exif::Tag` 是元组结构体 `Tag(Context, u16)` —— **标签值不是全局唯一的**，
/// 同一个编号在不同 IFD 里含义不同（0x8769 在 Tiff 是 Exif 指针，在别处另有所指）。
/// 所以解析必须同时确定「名字 → 标签」与「归属哪个 Context」。
///
/// 解析顺序：
///   1. 名字命中已知常量 ⇒ 用该常量（其 Context 自带，最权威）
///   2. 否则给出十六进制编号 ⇒ 按 GPS/Exif/Tiff 的编号区间猜 Context
///      （GPS IFD 用 0x0000..0x001f）
///   3. 都不行 ⇒ 报错
fn resolve_tag(name: &str, tag: u32) -> Result<Tag, String> {
    if let Some(mapped) = known_tag(name) {
        return Ok(mapped);
    }
    if tag <= 0xFFFF {
        // 未知标签：按编号区间推断 Context
        // GPS IFD 的标签编号范围是 0x0000..0x001f
        let ctx = if tag <= 0x001f {
            Context::Gps
        } else {
            Context::Tiff
        };
        return Ok(Tag(ctx, tag as u16));
    }
    Err(format!(
        "无法识别的 EXIF 标签: {}（请提供标签的十六进制值）",
        name
    ))
}

/// 已知标签名 → `Tag` 常量。
fn known_tag(name: &str) -> Option<Tag> {
    let t = match name.to_ascii_lowercase().as_str() {
        "make" => Tag::Make,
        "model" => Tag::Model,
        "software" => Tag::Software,
        "datetime" => Tag::DateTime,
        "datetimeoriginal" => Tag::DateTimeOriginal,
        "datetimedigitized" => Tag::DateTimeDigitized,
        "copyright" => Tag::Copyright,
        "artist" => Tag::Artist,
        "orientation" => Tag::Orientation,
        "exposuretime" => Tag::ExposureTime,
        "fnumber" => Tag::FNumber,
        "photographicsensitivity" => Tag::PhotographicSensitivity,
        "isospeed" => Tag::ISOSpeed,
        "focallength" => Tag::FocalLength,
        "whitebalance" => Tag::WhiteBalance,
        "flash" => Tag::Flash,
        "pixelxdimension" => Tag::PixelXDimension,
        "pixelydimension" => Tag::PixelYDimension,
        "lensmake" => Tag::LensMake,
        "lensmodel" => Tag::LensModel,
        "lensspecification" => Tag::LensSpecification,
        "exifversion" => Tag::ExifVersion,
        "usercomment" => Tag::UserComment,
        "imagedescription" => Tag::ImageDescription,
        "gpslatitude" => Tag::GPSLatitude,
        "gpslongitude" => Tag::GPSLongitude,
        "gpsaltitude" => Tag::GPSAltitude,
        "gpsaltituderef" => Tag::GPSAltitudeRef,
        "gpslatituderef" => Tag::GPSLatitudeRef,
        "gpslongituderef" => Tag::GPSLongitudeRef,
        "gpstimestamp" => Tag::GPSTimeStamp,
        "gpsdatestamp" => Tag::GPSDateStamp,
        "colorspace" => Tag::ColorSpace,
        "xresolution" => Tag::XResolution,
        "yresolution" => Tag::YResolution,
        "resolutionunit" => Tag::ResolutionUnit,
        "imagewidth" => Tag::ImageWidth,
        "imagelength" => Tag::ImageLength,
        _ => return None,
    };
    Some(t)
}

/// 判断字段是否应被删除。
fn matches_remove(field: &Field, remove: &[String]) -> bool {
    remove.iter().any(|r| {
        let r = r.trim();
        if let Some(hex) = r.strip_prefix("0x").or_else(|| r.strip_prefix("0X")) {
            if let Ok(v) = u32::from_str_radix(hex, 16) {
                if field.tag.number() as u32 == v {
                    return true;
                }
            }
        }
        if let Ok(v) = r.parse::<u32>() {
            if field.tag.number() as u32 == v {
                return true;
            }
        }
        let want = r.to_ascii_lowercase();
        tag_name(field.tag).to_ascii_lowercase() == want
    })
}

/// 按类型与值列表构造 `exif::Value`。
fn build_value(kind: &ExifValueKind, values: &[ExifValue]) -> Result<Value, String> {
    if values.is_empty() {
        return Err("EXIF 字段的值列表为空".to_string());
    }
    match kind {
        ExifValueKind::Ascii => {
            let mut out = Vec::new();
            for v in values {
                match v {
                    ExifValue::Text(s) => out.push(s.as_bytes().to_vec()),
                    other => return Err(format!("ASCII 类型需要文本值，收到: {:?}", other)),
                }
            }
            Ok(Value::Ascii(out))
        }
        ExifValueKind::Rational => {
            let mut out = Vec::new();
            for v in values {
                match v {
                    ExifValue::Rational(s) => {
                        let (n, d) = s
                            .split_once('/')
                            .ok_or_else(|| format!("有理数格式应为 num/den，收到: {}", s))?;
                        let num: u32 = n
                            .trim()
                            .parse()
                            .map_err(|_| format!("有理数分子不是整数: {}", n))?;
                        let den: u32 = d
                            .trim()
                            .parse()
                            .map_err(|_| format!("有理数分母不是整数: {}", d))?;
                        if den == 0 {
                            return Err("有理数分母不能为 0".to_string());
                        }
                        out.push(::exif::Rational { num, denom: den });
                    }
                    other => {
                        return Err(format!("Rational 类型需要 num/den 值，收到: {:?}", other))
                    }
                }
            }
            Ok(Value::Rational(out))
        }
        ExifValueKind::Short => Ok(Value::Short(
            collect_ints(values)?
                .into_iter()
                .map(|x| x as u16)
                .collect(),
        )),
        ExifValueKind::Long => Ok(Value::Long(
            collect_ints(values)?
                .into_iter()
                .map(|x| x as u32)
                .collect(),
        )),
        ExifValueKind::Byte => Ok(Value::Byte(
            collect_ints(values)?.into_iter().map(|x| x as u8).collect(),
        )),
        ExifValueKind::SByte => Ok(Value::SByte(
            collect_ints(values)?.into_iter().map(|x| x as i8).collect(),
        )),
        ExifValueKind::SShort => Ok(Value::SShort(
            collect_ints(values)?
                .into_iter()
                .map(|x| x as i16)
                .collect(),
        )),
        ExifValueKind::SLong => Ok(Value::SLong(
            collect_ints(values)?
                .into_iter()
                .map(|x| x as i32)
                .collect(),
        )),
        ExifValueKind::Float => Ok(Value::Float(
            collect_floats(values)?
                .into_iter()
                .map(|x| x as f32)
                .collect(),
        )),
        ExifValueKind::Double => Ok(Value::Double(collect_floats(values)?)),
        ExifValueKind::SRational => {
            let mut out = Vec::new();
            for v in values {
                match v {
                    ExifValue::Rational(s) => {
                        let (n, d) = s
                            .split_once('/')
                            .ok_or_else(|| format!("有理数格式应为 num/den，收到: {}", s))?;
                        let num: i32 = n
                            .trim()
                            .parse()
                            .map_err(|_| format!("有理数分子不是整数: {}", n))?;
                        let den: i32 = d
                            .trim()
                            .parse()
                            .map_err(|_| format!("有理数分母不是整数: {}", d))?;
                        if den == 0 {
                            return Err("有理数分母不能为 0".to_string());
                        }
                        out.push(::exif::SRational { num, denom: den });
                    }
                    other => {
                        return Err(format!("SRational 类型需要 num/den 值，收到: {:?}", other))
                    }
                }
            }
            Ok(Value::SRational(out))
        }
        ExifValueKind::Undefined => {
            let mut out = Vec::new();
            for v in values {
                match v {
                    ExifValue::Bytes(hex) => out.extend_from_slice(&parse_hex(hex)?),
                    ExifValue::Int(i) => out.push(*i as u8),
                    other => return Err(format!("Undefined 类型需要字节值，收到: {:?}", other)),
                }
            }
            Ok(Value::Undefined(out, 0))
        }
    }
}

/// 收集整数值。
fn collect_ints(values: &[ExifValue]) -> Result<Vec<u64>, String> {
    let mut out = Vec::with_capacity(values.len());
    for v in values {
        match v {
            ExifValue::Int(i) => {
                if *i < 0 {
                    return Err(format!("整数值不能为负: {}", i));
                }
                out.push(*i as u64);
            }
            other => return Err(format!("需要整数值，收到: {:?}", other)),
        }
    }
    Ok(out)
}

/// 收集浮点值。
fn collect_floats(values: &[ExifValue]) -> Result<Vec<f64>, String> {
    let mut out = Vec::with_capacity(values.len());
    for v in values {
        match v {
            ExifValue::Float(f) => out.push(*f),
            ExifValue::Int(i) => out.push(*i as f64),
            other => return Err(format!("需要数值，收到: {:?}", other)),
        }
    }
    Ok(out)
}

/// 解析十六进制字符串为字节。
fn parse_hex(hex: &str) -> Result<Vec<u8>, String> {
    let hex = hex.trim();
    if !hex.len().is_multiple_of(2) {
        return Err(format!("十六进制字节串长度必须是偶数: {}", hex));
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let s = std::str::from_utf8(&bytes[i..i + 2]).map_err(|e| e.to_string())?;
        let b = u8::from_str_radix(s, 16).map_err(|_| format!("非法十六进制: {}", s))?;
        out.push(b);
    }
    Ok(out)
}

/// 把字段写入目标容器。
fn write_fields(dest: &Path, fields: &[Field], little_endian: Option<bool>) -> Result<(), String> {
    // 目标格式由扩展名决定（与 crate 其余部分同一规则）
    let fmt = crate::parse_format(&crate::detect_format(dest))?;

    // 没有字段 ⇒ 不写 EXIF 段（容器里不塞空段）
    if fields.is_empty() {
        return Ok(());
    }

    let mut exif_buf: Vec<u8> = Vec::new();
    {
        let mut writer = ::exif::experimental::Writer::new();
        for f in fields {
            writer.push_field(f);
        }
        writer
            .write(
                &mut std::io::Cursor::new(&mut exif_buf),
                little_endian.unwrap_or(true),
            )
            .map_err(|e| format!("序列化 EXIF 失败: {}", e))?;
    }

    // 把 EXIF 段注入容器
    match fmt {
        image::ImageFormat::Jpeg => inject_jpeg_exif(dest, &exif_buf),
        image::ImageFormat::Tiff => {
            // TIFF 容器本身就是 EXIF 的家
            fs::write(dest, &exif_buf).map_err(|e| format!("写入文件失败: {}", e))?;
            Ok(())
        }
        image::ImageFormat::Png => inject_png_exif(dest, &exif_buf),
        image::ImageFormat::WebP => inject_webp_exif(dest, &exif_buf),
        other => Err(format!(
            "暂不支持向 {:?} 写入 EXIF（支持: JPEG/TIFF/PNG/WebP）",
            other
        )),
    }
}

/// 往 JPEG 里注入 APP1/Exif 段。
///
/// 做法：读原文件字节 → 找到 Exif 段并替换 → 写回。
/// 像素数据（从 SOI 之后的第一个非 APP 标记开始）原样保留。
fn inject_jpeg_exif(dest: &Path, exif_buf: &[u8]) -> Result<(), String> {
    let bytes = fs::read(dest).map_err(|e| format!("读取文件失败: {}", e))?;
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return Err("目标不是合法的 JPEG".to_string());
    }

    // 构造 "Exif\0\0" + TIFF 体
    let mut app1_payload = Vec::with_capacity(exif_buf.len() + 6);
    app1_payload.extend_from_slice(b"Exif\0\0");
    app1_payload.extend_from_slice(exif_buf);

    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() + app1_payload.len() + 4);
    out.extend_from_slice(&bytes[0..2]); // SOI

    let mut i = 2usize;
    let mut inserted = false;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xFF {
            // 不是标记 ⇒ 后面是像素数据，直接整体拷贝
            out.extend_from_slice(&bytes[i..]);
            break;
        }
        let marker = bytes[i + 1];
        // 填充字节
        if marker == 0xFF {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        // standalone 标记（无长度字段）
        if (0xD0..=0xD9).contains(&marker) || marker == 0x01 {
            out.extend_from_slice(&bytes[i..i + 2]);
            i += 2;
            if marker == 0xDA {
                // SOS ⇒ 后面全是像素
                out.extend_from_slice(&bytes[i..]);
                break;
            }
            continue;
        }
        let seg_len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        let seg_end = i + 2 + seg_len;
        if seg_end > bytes.len() {
            out.extend_from_slice(&bytes[i..]);
            break;
        }
        let is_app1 = marker == 0xE1;
        let is_exif_app1 = is_app1 && seg_len >= 8 && &bytes[i + 4..i + 10] == b"Exif\0\0";
        if is_exif_app1 {
            // 替换旧的 Exif 段
            if !inserted {
                let new_len = app1_payload.len() + 2;
                if new_len > u16::MAX as usize {
                    return Err("EXIF 段过大，超出 JPEG 段长度上限".to_string());
                }
                out.push(0xFF);
                out.push(0xE1);
                out.extend_from_slice(&(new_len as u16).to_be_bytes());
                out.extend_from_slice(&app1_payload);
                inserted = true;
            }
            i = seg_end;
            continue;
        }
        out.extend_from_slice(&bytes[i..seg_end]);
        i = seg_end;
    }

    if !inserted {
        // 没有旧 Exif 段 ⇒ 在 SOI 之后插入
        let mut head: Vec<u8> = Vec::new();
        head.extend_from_slice(&bytes[0..2]);
        let new_len = app1_payload.len() + 2;
        if new_len > u16::MAX as usize {
            return Err("EXIF 段过大，超出 JPEG 段长度上限".to_string());
        }
        head.push(0xFF);
        head.push(0xE1);
        head.extend_from_slice(&(new_len as u16).to_be_bytes());
        head.extend_from_slice(&app1_payload);
        head.extend_from_slice(&bytes[2..]);
        fs::write(dest, head).map_err(|e| format!("写入文件失败: {}", e))?;
        return Ok(());
    }

    fs::write(dest, out).map_err(|e| format!("写入文件失败: {}", e))?;
    Ok(())
}

/// 从 JPEG 里摘掉 Exif 段（像素字节一个不动）。
fn remove_jpeg_exif(dest: &Path) -> Result<(), String> {
    let bytes = fs::read(dest).map_err(|e| format!("读取文件失败: {}", e))?;
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return Err("目标不是合法的 JPEG".to_string());
    }

    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    out.extend_from_slice(&bytes[0..2]); // SOI

    let mut i = 2usize;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xFF {
            out.extend_from_slice(&bytes[i..]);
            break;
        }
        let marker = bytes[i + 1];
        if marker == 0xFF {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        if (0xD0..=0xD9).contains(&marker) || marker == 0x01 {
            out.extend_from_slice(&bytes[i..i + 2]);
            i += 2;
            if marker == 0xDA {
                out.extend_from_slice(&bytes[i..]);
                break;
            }
            continue;
        }
        let seg_len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        let seg_end = i + 2 + seg_len;
        if seg_end > bytes.len() {
            out.extend_from_slice(&bytes[i..]);
            break;
        }
        let is_exif_app1 = marker == 0xE1 && seg_len >= 8 && &bytes[i + 4..i + 10] == b"Exif\0\0";
        if !is_exif_app1 {
            out.extend_from_slice(&bytes[i..seg_end]);
        }
        i = seg_end;
    }

    fs::write(dest, out).map_err(|e| format!("写入文件失败: {}", e))?;
    Ok(())
}

/// 从 PNG 里摘掉 eXIf 块。
fn remove_png_exif(dest: &Path) -> Result<(), String> {
    let bytes = fs::read(dest).map_err(|e| format!("读取文件失败: {}", e))?;
    if bytes.len() < 8 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return Err("目标不是合法的 PNG".to_string());
    }

    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    out.extend_from_slice(&bytes[..8]);

    let mut i = 8usize;
    while i + 8 <= bytes.len() {
        let len = u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]) as usize;
        let ctype = &bytes[i + 4..i + 8];
        let chunk_end = i + 12 + len;
        if chunk_end > bytes.len() {
            break;
        }
        if ctype != b"eXIf" {
            out.extend_from_slice(&bytes[i..chunk_end]);
        }
        i = chunk_end;
    }

    fs::write(dest, out).map_err(|e| format!("写入文件失败: {}", e))?;
    Ok(())
}

/// 从 WebP 里摘掉 EXIF 块。
fn remove_webp_exif(dest: &Path) -> Result<(), String> {
    let bytes = fs::read(dest).map_err(|e| format!("读取文件失败: {}", e))?;
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return Err("目标不是合法的 WebP".to_string());
    }

    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    out.extend_from_slice(&bytes[..4]);
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&bytes[8..12]);

    let mut i = 12usize;
    while i + 8 <= bytes.len() {
        let fourcc = &bytes[i..i + 4];
        let size =
            u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]]) as usize;
        let chunk_end = i + 8 + size + (size % 2);
        if chunk_end > bytes.len() {
            break;
        }
        if fourcc != b"EXIF" {
            out.extend_from_slice(&bytes[i..chunk_end]);
        }
        i = chunk_end;
    }

    // 回填 RIFF 长度
    let riff_len = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&riff_len.to_le_bytes());

    fs::write(dest, out).map_err(|e| format!("写入文件失败: {}", e))?;
    Ok(())
}

/// 往 PNG 里注入 eXIf 块。
///
/// PNG 的 EXIF 走 `eXIf` chunk（PNG 1.5+ 扩展），载荷就是 TIFF 体（无 "Exif\0\0"）。
fn inject_png_exif(dest: &Path, exif_buf: &[u8]) -> Result<(), String> {
    let bytes = fs::read(dest).map_err(|e| format!("读取文件失败: {}", e))?;
    if bytes.len() < 8 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return Err("目标不是合法的 PNG".to_string());
    }

    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() + exif_buf.len() + 12);
    out.extend_from_slice(&bytes[..8]);

    let mut i = 8usize;
    let mut inserted = false;
    while i + 8 <= bytes.len() {
        let len = u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]) as usize;
        let ctype = &bytes[i + 4..i + 8];
        let chunk_end = i + 12 + len;
        if chunk_end > bytes.len() {
            break;
        }
        if ctype == b"eXIf" {
            // 替换已有 eXIf 块
            if !inserted {
                out.extend_from_slice(&build_png_chunk(b"eXIf", exif_buf));
                inserted = true;
            }
            i = chunk_end;
            continue;
        }
        // eXIf 必须排在 IDAT 之前
        if ctype == b"IDAT" && !inserted {
            out.extend_from_slice(&build_png_chunk(b"eXIf", exif_buf));
            inserted = true;
        }
        out.extend_from_slice(&bytes[i..chunk_end]);
        i = chunk_end;
    }
    if !inserted {
        out.extend_from_slice(&build_png_chunk(b"eXIf", exif_buf));
    }

    fs::write(dest, out).map_err(|e| format!("写入文件失败: {}", e))?;
    Ok(())
}

/// 构造一个 PNG chunk（长度 + 类型 + 载荷 + CRC）。
fn build_png_chunk(ctype: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 12);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(ctype);
    out.extend_from_slice(payload);
    let mut crc_input = Vec::with_capacity(payload.len() + 4);
    crc_input.extend_from_slice(ctype);
    crc_input.extend_from_slice(payload);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
    out
}

/// CRC-32（PNG 用的是 IEEE 多项式）。
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// 往 WebP 里注入 EXIF 块。
///
/// WebP 的 EXIF 走 `EXIF` chunk（RIFF 容器内的 FourCC）。
fn inject_webp_exif(dest: &Path, exif_buf: &[u8]) -> Result<(), String> {
    let bytes = fs::read(dest).map_err(|e| format!("读取文件失败: {}", e))?;
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return Err("目标不是合法的 WebP".to_string());
    }

    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() + exif_buf.len() + 8);
    out.extend_from_slice(&bytes[..4]); // "RIFF"
    out.extend_from_slice(&[0, 0, 0, 0]); // 长度占位
    out.extend_from_slice(&bytes[8..12]); // "WEBP"

    let mut i = 12usize;
    let mut inserted = false;
    while i + 8 <= bytes.len() {
        let fourcc = &bytes[i..i + 4];
        let size =
            u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]]) as usize;
        let chunk_end = i + 8 + size + (size % 2);
        if chunk_end > bytes.len() {
            break;
        }
        if fourcc == b"EXIF" {
            if !inserted {
                out.extend_from_slice(b"EXIF");
                out.extend_from_slice(&(exif_buf.len() as u32).to_le_bytes());
                out.extend_from_slice(exif_buf);
                if exif_buf.len() % 2 == 1 {
                    out.push(0);
                }
                inserted = true;
            }
            i = chunk_end;
            continue;
        }
        out.extend_from_slice(&bytes[i..chunk_end]);
        i = chunk_end;
    }
    if !inserted {
        out.extend_from_slice(b"EXIF");
        out.extend_from_slice(&(exif_buf.len() as u32).to_le_bytes());
        out.extend_from_slice(exif_buf);
        if exif_buf.len() % 2 == 1 {
            out.push(0);
        }
    }

    // 回填 RIFF 长度
    let riff_len = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&riff_len.to_le_bytes());

    fs::write(dest, out).map_err(|e| format!("写入文件失败: {}", e))?;
    Ok(())
}
