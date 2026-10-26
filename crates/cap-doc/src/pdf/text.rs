use crate::paths;
use crate::pdf::font::FontMap;
use crate::pdf::ops::load_doc;
use lopdf::{Document, Object};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// PDF元数据
#[derive(Debug, Serialize, Deserialize)]
pub struct PdfMeta {
    pub title: String,
    pub author: String,
    pub page_count: u32,
    pub creator: String,
}

/// 解码 PDF 字符串：规范里有 PDFDocEncoding、UTF-16BE（带/不带 BOM）和 UTF-8 三种常见写法。
pub(crate) fn decode_pdf_string(raw: &[u8]) -> String {
    if let Some(body) = raw.strip_prefix(&[0xFE, 0xFF]) {
        return utf16be_to_string(body);
    }
    // 不带 BOM 的 ASCII UTF-16BE（00 41 00 42 …）本身也是合法 UTF-8，必须先于 UTF-8 分支判断
    if raw.len() >= 4 && raw.len().is_multiple_of(2) && raw.iter().step_by(2).all(|&b| b == 0) {
        return utf16be_to_string(raw);
    }
    if let Ok(text) = std::str::from_utf8(raw) {
        if !text.contains('\0') {
            return text.to_string();
        }
    }
    // 中文标题常被写成无 BOM 的 UTF-16BE；解不出可读字符就退回逐字节
    if raw.len() >= 2 && raw.len().is_multiple_of(2) {
        let guess = utf16be_to_string(raw);
        if !guess.is_empty() && guess.chars().all(|c| !c.is_control()) {
            return guess;
        }
    }
    raw.iter().map(|&b| b as char).collect()
}

fn utf16be_to_string(raw: &[u8]) -> String {
    let units: Vec<u16> = raw
        .chunks(2)
        .filter(|c| c.len() == 2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

fn is_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    ) || byte.is_ascii_whitespace()
}

/// 读一个字面字符串 `(...)`，`i` 指向起始左括号。返回内容和右括号之后的位置。
/// 括号可以嵌套，转义序列按 PDF 规范处理（含八进制）。
fn read_literal(buf: &[u8], start: usize) -> (Vec<u8>, usize) {
    let mut out: Vec<u8> = Vec::new();
    let mut i = start + 1;
    let mut depth = 1usize;
    while i < buf.len() {
        match buf[i] {
            b'\\' => {
                i += 1;
                if i >= buf.len() {
                    break;
                }
                match buf[i] {
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0C),
                    b'(' | b')' | b'\\' => out.push(buf[i]),
                    digit @ b'0'..=b'7' => {
                        let mut value = (digit - b'0') as u32;
                        let mut taken = 1;
                        while taken < 3 && i + 1 < buf.len() && (b'0'..=b'7').contains(&buf[i + 1])
                        {
                            i += 1;
                            value = value * 8 + (buf[i] - b'0') as u32;
                            taken += 1;
                        }
                        out.push(value as u8);
                    }
                    // 反斜杠 + 换行是行延续，本身不产出字符
                    other if other != b'\n' && other != b'\r' => out.push(other),
                    _ => {}
                }
                i += 1;
            }
            b'(' => {
                depth += 1;
                out.push(b'(');
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    return (out, i);
                }
                out.push(b')');
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    (out, i)
}

/// 读一个十六进制字符串 `<...>`，`i` 指向起始 `<`。
fn read_hex(buf: &[u8], i: usize) -> Option<(Vec<u8>, usize)> {
    let end = (i + 1..buf.len()).find(|&j| buf[j] == b'>')?;
    let digits: Vec<u8> = buf[i + 1..end]
        .iter()
        .filter(|b| b.is_ascii_hexdigit())
        .copied()
        .collect();
    let mut out = Vec::with_capacity(digits.len() / 2);
    let pair = digits.chunks(2);
    for chunk in pair {
        if chunk.len() == 2 {
            let hi = (chunk[0] as char).to_digit(16)?;
            let lo = (chunk[1] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
        } else {
            // 奇数位补零尾，规范如此
            out.push(((chunk[0] as char).to_digit(16)? * 16) as u8);
        }
    }
    Some((out, end + 1))
}

/// 3×3 仿射矩阵的 6 个分量，沿用 PDF 的行向量写法 `[a b c d e f]`：
/// 点 (x,y) → (a·x + c·y + e, b·x + d·y + f)。
#[derive(Clone, Copy)]
struct Affine([f32; 6]);

impl Affine {
    const IDENTITY: Affine = Affine([1., 0., 0., 1., 0., 0.]);

    /// 先作用 `self` 再作用 `other`；Td/cm 这类"前乘"操作都走这里。
    fn then(self, other: Affine) -> Affine {
        let [a, b, c, d, e, f] = self.0;
        let [oa, ob, oc, od, oe, of] = other.0;
        Affine([
            a * oa + b * oc,
            a * ob + b * od,
            c * oa + d * oc,
            c * ob + d * od,
            e * oa + f * oc + oe,
            e * ob + f * od + of,
        ])
    }

    fn translate(tx: f32, ty: f32) -> Affine {
        Affine([1., 0., 0., 1., tx, ty])
    }
}

/// 一页内容流当前的文本/图形状态。折行判定要靠它：
/// 只有基线真的移动了才算换行，否则同一行的分片段会被拆成一堆碎行。
#[derive(Clone, Copy)]
struct TextState {
    /// 文本行矩阵（Tm/Td/TD/T* 累积出来的）
    line: Affine,
    /// 当前变换矩阵，`cm` 会改写它；q/Q 与文本状态一起保存/恢复
    ctm: Affine,
    leading: f32,
    size: f32,
}

impl TextState {
    fn new() -> Self {
        Self {
            line: Affine::IDENTITY,
            ctm: Affine::IDENTITY,
            leading: 0.,
            size: 0.,
        }
    }

    /// 当前文本行原点落到设备坐标后的位置，以及一个文字单位在设备下的长度。
    fn cursor(&self) -> ((f32, f32), f32) {
        let eff = self.line.then(self.ctm);
        let [a, b, _, _, x, y] = eff.0;
        let unit = (a * a + b * b).sqrt();
        // 竖排（旋转约 90°）时行与行沿 x 排布，"跨行/行内"两个方向要互换
        let (across, along) = if b.abs() > a.abs() { (x, y) } else { (y, x) };
        // 字高必须按 size × unit 算：Apple 许可证这类生产者写 `10 0 0 10 x y Tm /F 1 Tf`，
        // 字号是 1 而缩放藏在矩阵里。直接给字号设下限会把阈值放大十倍，整段文字糊成一行。
        ((across, along), self.size.max(1.) * unit)
    }
}

/// 上一段画下去的字落在哪里，外加这一行迄今的最大字高。
#[derive(Clone, Copy)]
struct LastDraw {
    /// "跨行"方向坐标（横排即基线 y）：与上一段比，差出半个字高就算另起一行
    across: f32,
    /// "行内"方向坐标（横排即 x）：明显倒退说明回到了行首或换了一栏
    prev: f32,
    /// 阈值按整行最大字高算：7pt 上标抬升 3.6pt 不该自成一行的
    /// （BERT[1] 会被切成三行），而 10pt 正文 12pt 的行距又必须断开
    height: f32,
}

/// 把一段文字接到 `out` 末尾：基线移动超过阈值才补换行。
/// 行内不插空格——中文按字绘制，插空格会把句子切断（空格本身由内容流里的 `( )` 提供）。
fn append_text(out: &mut String, state: &TextState, line: &mut Option<LastDraw>, text: &str) {
    if text.is_empty() {
        return;
    }
    let ((across, along), height) = state.cursor();
    let broke = match *line {
        Some(anchor) => {
            let limit = anchor.height.max(height);
            (across - anchor.across).abs() > limit * 0.5 || along - anchor.prev < -limit
        }
        None => false,
    };
    if broke && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(text);
    *line = match *line {
        Some(anchor) if !broke => Some(LastDraw {
            across,
            prev: along,
            height: anchor.height.max(height),
        }),
        _ => Some(LastDraw {
            across,
            prev: along,
            height,
        }),
    };
}

/// 从一页（已解码的）内容流里取出可见文本。
/// 展示文本的操作符（Tj、TJ、'、"）负责输出，定位操作符（Td、TD、T*、Tm、TL、cm）
/// 只更新文本状态，画字时再按基线是否移动决定折行。`Tf` 决定后续字符串按哪个字体的编码解释。
fn page_text(content: &[u8], fonts: &HashMap<Vec<u8>, FontMap>) -> String {
    let mut out = String::new();
    // 连续出现的字符串操作数；遇到展示操作符才被消费
    let mut pending: Vec<String> = Vec::new();
    // 数字操作数，按算符取用（Td 取末尾两个、Tm 取末尾六个）
    let mut operands: Vec<f32> = Vec::new();
    let mut state = TextState::new();
    // q/Q 保存恢复的是整个图形状态，文本矩阵也在其中
    let mut stack: Vec<TextState> = Vec::new();
    // 当前行的锚点（画字时更新）
    let mut line: Option<LastDraw> = None;
    let mut current_font: Option<Vec<u8>> = None;
    let mut last_name: Option<Vec<u8>> = None;
    let mut i = 0usize;

    // 取末尾 n 个数字；数量不足（畸形流）时返回 None，保持原状态不动
    macro_rules! tail {
        ($n:expr) => {
            if operands.len() >= $n {
                Some([operands[operands.len() - $n], operands[operands.len() - 1]])
            } else {
                None
            }
        };
    }

    while i < content.len() {
        let byte = content[i];
        if byte == b'(' {
            let (raw, next) = read_literal(content, i);
            pending.push(decode_glyphs(&raw, current_font.as_deref(), fonts));
            i = next;
            continue;
        }
        if byte == b'<' && content.get(i + 1) != Some(&b'<') {
            match read_hex(content, i) {
                Some((raw, next)) => {
                    pending.push(decode_glyphs(&raw, current_font.as_deref(), fonts));
                    i = next;
                    continue;
                }
                None => i += 1,
            }
            continue;
        }
        if byte == b'/' {
            let start = i + 1;
            let mut end = start;
            while end < content.len() && !is_delimiter(content[end]) {
                end += 1;
            }
            last_name = Some(content[start..end].to_vec());
            i = end;
            continue;
        }
        if byte.is_ascii_digit() || byte == b'-' || byte == b'+' || byte == b'.' {
            let start = i;
            while i < content.len() && matches!(content[i], b'0'..=b'9' | b'.' | b'-' | b'+') {
                i += 1;
            }
            if let Ok(value) = std::str::from_utf8(&content[start..i]) {
                if let Ok(number) = value.parse::<f32>() {
                    operands.push(number);
                }
            }
            continue;
        }
        if is_delimiter(byte) {
            i += 1;
            continue;
        }
        // 到这里是一个关键字：操作符，或字体内部编号之类
        let start = i;
        while i < content.len() && !is_delimiter(content[i]) {
            i += 1;
        }
        let op = &content[start..i];
        match op {
            b"Tf" => {
                current_font = last_name.take();
                // `/F1 12 Tf` 里只有字号是数字，字体名走 /Name 分支，
                // 按"末尾两个数字"取操作数会一个都取不到，字高永远停在默认值。
                if let Some([_, size]) = tail!(1) {
                    state.size = size;
                }
            }
            b"Tj" => {
                if let Some(text) = pending.pop() {
                    append_text(&mut out, &state, &mut line, &text);
                }
                pending.clear();
            }
            b"'" | b"\"" => {
                // 两者都隐含一次 T*：先换行再画字
                let moved = Affine::translate(0., -state.leading).then(state.line);
                state.line = moved;
                if let Some(text) = pending.pop() {
                    append_text(&mut out, &state, &mut line, &text);
                }
                pending.clear();
            }
            b"TJ" => {
                let texts: Vec<String> = std::mem::take(&mut pending);
                let joined = texts.concat();
                append_text(&mut out, &state, &mut line, &joined);
            }
            b"Td" => {
                if let Some([tx, ty]) = tail!(2) {
                    let moved = Affine::translate(tx, ty).then(state.line);
                    state.line = moved;
                }
                pending.clear();
            }
            b"TD" => {
                if let Some([tx, ty]) = tail!(2) {
                    state.leading = -ty;
                    let moved = Affine::translate(tx, ty).then(state.line);
                    state.line = moved;
                }
                pending.clear();
            }
            b"T*" => {
                let moved = Affine::translate(0., -state.leading).then(state.line);
                state.line = moved;
                pending.clear();
            }
            b"TL" => {
                if let Some([_, leading]) = tail!(1) {
                    state.leading = leading;
                }
            }
            b"Tm" => {
                if operands.len() >= 6 {
                    let six = &operands[operands.len() - 6..];
                    state.line = Affine([six[0], six[1], six[2], six[3], six[4], six[5]]);
                }
                pending.clear();
            }
            b"BT" => {
                state.line = Affine::IDENTITY;
                pending.clear();
            }
            b"cm" => {
                if operands.len() >= 6 {
                    let six = &operands[operands.len() - 6..];
                    let step = Affine([six[0], six[1], six[2], six[3], six[4], six[5]]);
                    state.ctm = step.then(state.ctm);
                }
            }
            b"q" => stack.push(state),
            b"Q" => {
                if let Some(saved) = stack.pop() {
                    state = saved;
                }
            }
            b"ET" => {
                pending.clear();
            }
            _ => {}
        }
        operands.clear();
    }
    out
}

/// 字形码 → 文本：有字体映射就走编码表/CMap，否则退回 PDF 字符串编码启发式。
fn decode_glyphs(raw: &[u8], font: Option<&[u8]>, fonts: &HashMap<Vec<u8>, FontMap>) -> String {
    let text = if let Some(map) = font.and_then(|name| fonts.get(name)) {
        let mapped = map.decode(raw);
        if mapped.is_empty() {
            decode_pdf_string(raw)
        } else {
            mapped
        }
    } else {
        decode_pdf_string(raw)
    };
    display_text(&text)
}

/// 去掉解出来但读者看不见的码位。
///
/// 子集字体常把不可见的排版标记（零宽占位之类）映射到 C0 码位，实测一份 WPS 导出的
/// PDF 里 "AI<0x01>Agent" 就是这样，软连字符也一样——它们进了前端文本面板只会变成
/// 破框、凭空多出的破折号或看似截断的空洞。
///
/// 换行与制表保留（那是内容流自己表达的排版），回车归一为换行；其余 C0、DEL、
/// 软连字符丢弃。 bidi/连字这类影响字形拼接的 Cf 标记保留，删了会改坏阿拉伯文排版。
fn scrub_invisible(text: &str) -> String {
    let dirty =
        |c: char| (c.is_control() || matches!(c, '\u{7f}' | '\u{ad}')) && !matches!(c, '\n' | '\t');
    if !text.chars().any(dirty) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\n' | '\t' => out.push(ch),
            '\r' => out.push('\n'),
            _ if dirty(ch) => {}
            _ => out.push(ch),
        }
    }
    out
}

/// Kangxi 部首码位（U+2F00–U+2FD5）到正经汉字的映射，数组下标就是码位偏移，共 214 条。
///
/// 子集字体会把"文"这种字直接映射到部首码位（实测一份 WPS 导出的 PDF 里有 396 个），
/// 屏幕上是同一个字形，复制到面板外却搜不到、分词也断在中间。Unicode 把这 214 个码位
/// 定义为兼容字符，NFKC 归一结果就是下表；部首码位本身没有别的用途，替换不丢信息。
/// 代价是部首索引页（词典那种）会退化成普通汉字——真实语料里没出现过这种文档。
const KANGXI_IDEOGRAPH: [char; 214] = [
    '一', '丨', '丶', '丿', '乙', '亅', '二', '亠', '人', '儿', '入', '八', '冂', '冖', '冫', '几',
    '凵', '刀', '力', '勹', '匕', '匚', '匸', '十', '卜', '卩', '厂', '厶', '又', '口', '囗', '土',
    '士', '夂', '夊', '夕', '大', '女', '子', '宀', '寸', '小', '尢', '尸', '屮', '山', '巛', '工',
    '己', '巾', '干', '幺', '广', '廴', '廾', '弋', '弓', '彐', '彡', '彳', '心', '戈', '戶', '手',
    '支', '攴', '文', '斗', '斤', '方', '无', '日', '曰', '月', '木', '欠', '止', '歹', '殳', '毋',
    '比', '毛', '氏', '气', '水', '火', '爪', '父', '爻', '爿', '片', '牙', '牛', '犬', '玄', '玉',
    '瓜', '瓦', '甘', '生', '用', '田', '疋', '疒', '癶', '白', '皮', '皿', '目', '矛', '矢', '石',
    '示', '禸', '禾', '穴', '立', '竹', '米', '糸', '缶', '网', '羊', '羽', '老', '而', '耒', '耳',
    '聿', '肉', '臣', '自', '至', '臼', '舌', '舛', '舟', '艮', '色', '艸', '虍', '虫', '血', '行',
    '衣', '襾', '見', '角', '言', '谷', '豆', '豕', '豸', '貝', '赤', '走', '足', '身', '車', '辛',
    '辰', '辵', '邑', '酉', '釆', '里', '金', '長', '門', '阜', '隶', '隹', '雨', '靑', '非', '面',
    '革', '韋', '韭', '音', '頁', '風', '飛', '食', '首', '香', '馬', '骨', '高', '髟', '鬥', '鬯',
    '鬲', '鬼', '魚', '鳥', '鹵', '鹿', '麥', '麻', '黃', '黍', '黑', '黹', '黽', '鼎', '鼓', '鼠',
    '鼻', '齊', '齒', '龍', '龜', '龠',
];

/// 抽取出来的文本 → 面板可读文本：丢掉看不见的码位，再把兼容字形归一成正经汉字。
fn display_text(text: &str) -> String {
    let cleaned = scrub_invisible(text);
    if !cleaned
        .chars()
        .any(|c| matches!(c, '\u{2f00}'..='\u{2fd5}'))
    {
        return cleaned;
    }
    cleaned
        .chars()
        .map(|c| match c {
            '\u{2f00}'..='\u{2fd5}' => KANGXI_IDEOGRAPH[c as usize - 0x2F00],
            other => other,
        })
        .collect()
}

/// 提取PDF全部文本
pub fn extract_pdf_text(path: &str) -> Result<String, String> {
    let file = paths::input(path)?;
    let doc = load_doc(&file)?;
    let mut pages = Vec::new();
    for (_, id) in doc.get_pages() {
        let content = doc
            .get_page_content(id)
            .map_err(|e| format!("读取页面内容失败: {:?}", e))?;
        let text = page_text(&content, &crate::pdf::font::font_maps(&doc, id));
        let trimmed = text.trim_matches(|c| c == '\n' || c == ' ');
        if !trimmed.is_empty() {
            pages.push(trimmed.to_string());
        }
    }

    let result = pages.join("\n");
    if result.trim().is_empty() {
        Ok("无法提取PDF文本内容。该PDF可能是扫描件或加密文件。".to_string())
    } else {
        Ok(result)
    }
}

fn info_string(doc: &Document, key: &[u8]) -> Option<String> {
    let info_ref = doc.trailer.get(b"Info").ok()?;
    let info_id = info_ref.as_reference().ok()?;
    let info_obj = doc.get_object(info_id).ok()?;
    let dict = info_obj.as_dict().ok()?;
    match dict.get(key).ok()? {
        Object::String(bytes, _) => {
            let text = display_text(&decode_pdf_string(bytes));
            if text.trim().is_empty() {
                None
            } else {
                Some(text)
            }
        }
        _ => None,
    }
}

/// 获取PDF元数据
pub fn get_pdf_metadata(path: &str) -> Result<PdfMeta, String> {
    let file = paths::input(path)?;
    let doc = load_doc(&file)?;

    let title = info_string(&doc, b"Title").unwrap_or_else(|| {
        file.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "未知".to_string())
    });
    let author = info_string(&doc, b"Author").unwrap_or_else(|| "未知".to_string());
    let creator = info_string(&doc, b"Creator").unwrap_or_default();
    // 页数只能数页树：/Type /Pages 里含 "/Type /Page" 子串，按字节匹配必然多算，
    // 而现代 PDF 的页对象常藏在压缩对象流里，按字节又根本扫不到。
    let page_count = doc.get_pages().len() as u32;

    Ok(PdfMeta {
        title,
        author,
        page_count,
        creator,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// 手写一个结构完整、内容流可选 Flate 压缩的 PDF。
    /// 页面上画的每一行文字来自 `lines`，用于验证文本提取。
    fn make_pdf(lines: &[&str], compressed: bool) -> Vec<u8> {
        let stream_plain = {
            let mut buf = String::from("BT /F1 24 Tf 72 700 Td ");
            for line in lines {
                buf.push_str(&format!("({}) Tj 0 -28 Td ", line));
            }
            buf.push_str("ET");
            buf.into_bytes()
        };
        let (content, filter) = if compressed {
            use flate2::write::ZlibEncoder;
            use flate2::Compression;
            use std::io::Write;
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&stream_plain).unwrap();
            (encoder.finish().unwrap(), " /Filter /FlateDecode")
        } else {
            (stream_plain, "")
        };

        let mut objects: Vec<Vec<u8>> = Vec::new();
        objects.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
        objects.push(b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 612 792] >>".to_vec());
        objects.push(
            b"<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
        );
        objects.push(format!("<< /Length {}{} >>", content.len(), filter).into_bytes());
        objects.push(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec());

        let mut body = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, text) in objects.iter().enumerate() {
            offsets.push(body.len());
            body.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            body.extend_from_slice(text);
            body.extend_from_slice(b"\n");
            if i == 3 {
                body.extend_from_slice(b"stream\n");
                body.extend_from_slice(&content);
                body.extend_from_slice(b"\nendstream\n");
            }
            body.extend_from_slice(b"endobj\n");
        }
        let start = body.len();
        let count = objects.len() + 1;
        body.extend_from_slice(format!("xref\n0 {}\n", count).as_bytes());
        body.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            body.extend_from_slice(format!("{:010} 00000 n \n", offset).as_bytes());
        }
        body.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
                count, start
            )
            .as_bytes(),
        );
        body
    }

    fn write_fixture(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn text_extracts_from_plain_content_stream() {
        let dir = crate::test_support::TempDir::new("pdftext-plain");
        let file = write_fixture(
            &dir,
            "plain.pdf",
            &make_pdf(&["Hello PDF", "second line"], false),
        );
        let text = extract_pdf_text(file.to_str().unwrap()).unwrap();
        assert!(text.contains("Hello PDF"), "{}", text);
        assert!(text.contains("second line"), "{}", text);
        // 两个 Td 之间要折行，否则整页文字会糊成一行
        assert!(
            text.contains("Hello PDF\nsecond line"),
            "应该按定位操作符换行: {:?}",
            text
        );
    }

    /// 现代 PDF 的内容流几乎都压缩过；旧实现直接按字节扫原文，这类文件一个字符都取不出。
    #[test]
    fn text_extracts_from_flate_compressed_stream() {
        let dir = crate::test_support::TempDir::new("pdftext-zip");
        let file = write_fixture(
            &dir,
            "comp.pdf",
            &make_pdf(&["Compressed Hidden Text"], true),
        );
        let raw = fs::read(&file).unwrap();
        assert!(
            !String::from_utf8_lossy(&raw).contains("Compressed Hidden Text"),
            "fixture 必须真的是压缩过的，否则这条测试没有意义"
        );
        let text = extract_pdf_text(file.to_str().unwrap()).unwrap();
        assert_eq!(text, "Compressed Hidden Text");
    }

    #[test]
    fn text_reports_scan_or_encrypted_pdfs() {
        let dir = crate::test_support::TempDir::new("pdftext-empty");
        // 页面上一个字都没有（等价于扫描件：内容流只有绘制指令）
        let file = write_fixture(&dir, "blank.pdf", &make_pdf(&[], false));
        let text = extract_pdf_text(file.to_str().unwrap()).unwrap();
        assert!(text.contains("无法提取"), "{}", text);
    }
    #[test]
    fn metadata_pages_and_info_fields() {
        let dir = crate::test_support::TempDir::new("pdfmeta-info");
        // 3 页文档：/Type /Pages 节点本身含 "/Type /Page" 子串，旧的字节计数会数成 4
        let file = write_fixture(&dir, "meta.pdf", &make_meta_pdf(3, true));
        let meta = get_pdf_metadata(file.to_str().unwrap()).unwrap();
        assert_eq!(meta.page_count, 3);
        assert_eq!(meta.author, "张三");
        assert_eq!(meta.title, "年度报告");
        assert_eq!(meta.creator, "WPS");

        // 标题缺失时回退到文件名
        let no_title = write_fixture(&dir, "untitled.pdf", &make_meta_pdf(1, false));
        let meta = get_pdf_metadata(no_title.to_str().unwrap()).unwrap();
        assert_eq!(meta.title, "untitled");
        assert_eq!(meta.page_count, 1);
    }

    /// 多页 + 带 Info（UTF-16BE 中文）的最小 PDF
    fn make_meta_pdf(pages: usize, with_title: bool) -> Vec<u8> {
        let kids: Vec<String> = (0..pages).map(|i| format!("{} 0 R", 3 + i)).collect();
        let mut objects: Vec<String> = Vec::new();
        objects.push("<< /Type /Catalog /Pages 2 0 R >>".to_string());
        objects.push(format!(
            "<< /Type /Pages /Kids [{}] /Count {} /MediaBox [0 0 612 792] >>",
            kids.join(" "),
            pages
        ));
        for i in 0..pages {
            objects.push(format!(
                "<< /Type /Page /Parent 2 0 R /Contents {} 0 R >>",
                3 + pages + i
            ));
        }
        for i in 0..pages {
            let stream = format!("BT ({}) Tj ET", i + 1);
            objects.push(format!(
                "<< /Length {} >>\nstream\n{}\nendstream",
                stream.len(),
                stream
            ));
        }
        // Info：中文按 UTF-16BE + BOM 写（真实生产者的常见写法）
        let title = if with_title {
            format!("/Title <{}> ", utf16be_hex("年度报告"))
        } else {
            String::new()
        };
        objects.push(format!(
            "<< {} /Author <{}> /Creator (WPS) >>",
            title,
            utf16be_hex("张三")
        ));

        let mut body = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, text) in objects.iter().enumerate() {
            offsets.push(body.len());
            body.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", i + 1, text).as_bytes());
        }
        let info_number = objects.len(); // Info 是最后一个对象
        let start = body.len();
        let count = objects.len() + 1;
        body.extend_from_slice(format!("xref\n0 {}\n", count).as_bytes());
        body.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            body.extend_from_slice(format!("{:010} 00000 n \n", offset).as_bytes());
        }
        body.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R /Info {} 0 R >>\nstartxref\n{}\n%%EOF\n",
                count, info_number, start
            )
            .as_bytes(),
        );
        body
    }

    fn utf16be_hex(text: &str) -> String {
        let mut hex = String::from("FEFF");
        for unit in text.encode_utf16() {
            hex.push_str(&format!("{:04X}", unit));
        }
        hex
    }

    #[test]
    fn decoding_handles_the_three_string_encodings() {
        assert_eq!(decode_pdf_string(b"plain"), "plain");
        assert_eq!(
            decode_pdf_string(&[0xFE, 0xFF, 0x4E, 0x2D, 0x65, 0x87]),
            "中文"
        );
        assert_eq!(decode_pdf_string(&utf16be_bytes("中文")), "中文");
        assert_eq!(decode_pdf_string(&utf16be_bytes("ascii")), "ascii");
        assert_eq!(
            decode_pdf_string("中文".as_bytes()),
            "中文",
            "UTF-8 字面量不该被当成 UTF-16"
        );
    }

    fn utf16be_bytes(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for unit in text.encode_utf16() {
            out.push((unit >> 8) as u8);
            out.push((unit & 0xFF) as u8);
        }
        out
    }

    #[test]
    fn literal_parsing_respects_escapes_and_nesting() {
        let buf = b"(a\\(b\\)c\\n\\101) Tj";
        let (raw, next) = read_literal(buf, 0);
        assert_eq!(decode_pdf_string(&raw), "a(b)c\nA");
        assert_eq!(next, 15);
        // 嵌套括号不需要转义
        let nested = b"(x (y) z) Tj";
        let (raw, _) = read_literal(nested, 0);
        assert_eq!(decode_pdf_string(&raw), "x (y) z");
    }

    #[test]
    fn hex_string_parsing() {
        let buf = b"<48656C6C6F> Tj";
        let (raw, next) = read_hex(buf, 0).unwrap();
        assert_eq!(decode_pdf_string(&raw), "Hello");
        assert_eq!(next, 12);
        // 奇数位补零尾
        let short = b"<414> Tj";
        let (raw, _) = read_hex(short, 0).unwrap();
        assert_eq!(raw, vec![0x41, 0x40]);
    }

    #[test]
    fn tj_array_concatenates_its_pieces() {
        let stream = b"BT /F1 24 Tf [(He) -500 (llo) ( world)] TJ ET";
        assert_eq!(page_text(stream, &HashMap::new()).trim(), "Hello world");
    }

    /// `Tf` 只有字号是数字（字体名走 /Name）。取"末尾两个数字"会一个都取不到，
    /// 字高停在默认值，上标这种 3.6pt 的抬升就被当成了换行。
    #[test]
    fn font_size_from_tf_drives_the_break_threshold() {
        let joined =
            b"BT /F1 10 Tf 1 0 0 1 340 458 Tm (model BERT) Tj ET                BT /F2 7 Tf 1 0 0 1 365 461.955 Tm ([1]) Tj ET                BT /F1 10 Tf 1 0 0 1 374 458 Tm (, and on) Tj ET";
        assert_eq!(
            page_text(joined, &HashMap::new()).trim(),
            "model BERT[1], and on"
        );
    }

    /// Apple 许可证的真实写法：`10 0 0 10 40 374 Tm /TT2 1 Tf`，字号 1、缩放在矩阵里。
    /// 字高必须按 size × 矩阵缩放算，给字号设下限会把阈值放大十倍，整段糊成一行。
    #[test]
    fn font_size_scaled_in_the_matrix_is_not_overestimated() {
        let stream = b"BT 10 0 0 10 40 386 Tm /TT2 1 Tf (Line one) Tj ET \
                       BT 10 0 0 10 40 374 Tm /TT2 1 Tf (Line two) Tj ET";
        assert_eq!(
            page_text(stream, &HashMap::new()).trim(),
            "Line one\nLine two"
        );
        let same = b"BT 10 0 0 10 40 374 Tm /TT2 1 Tf (So) Tj ET \
                     BT 10 0 0 10 60 374 Tm /TT2 1 Tf (ft) Tj ET";
        assert_eq!(page_text(same, &HashMap::new()).trim(), "Soft");
    }

    /// 真实生产者会把一行拆成若干片段、只用 Td 沿 x 推进来排版。
    /// 旧实现见 Td 就折行，把 "SOFTWARE LICENSE" 切成一堆两三字的碎行。
    #[test]
    fn pieces_sharing_a_baseline_join_into_one_line() {
        let stream = b"BT /F1 24 Tf 72 700 Td (SOFT) Tj 30 0 Td (WARE) Tj 24 0 Td ( LICENSE) Tj ET";
        assert_eq!(
            page_text(stream, &HashMap::new()).trim(),
            "SOFTWARE LICENSE"
        );
    }

    /// 基线抖动（浮点定位、0.5pt 以内的偏移）不算换行
    #[test]
    fn sub_point_baseline_noise_stays_on_the_same_line() {
        let stream = b"BT /F1 24 Tf 72 700 Td (Wi) Tj 12 0.4 Td (dth) Tj ET";
        assert_eq!(page_text(stream, &HashMap::new()).trim(), "Width");
    }

    /// 基线真的移动了才折行：阈值取字高的 30%，24pt 字体下 28pt 的行距必须断开
    #[test]
    fn a_real_baseline_step_breaks_the_line() {
        let stream = b"BT /F1 24 Tf 72 700 Td (first) Tj 0 -28 Td (second) Tj ET";
        assert_eq!(page_text(stream, &HashMap::new()).trim(), "first\nsecond");
    }

    /// ' 与 " 隐含一次 T*：先按 leading 移到下一行，再画字
    #[test]
    fn quote_operators_imply_a_line_move() {
        let stream = b"BT /F1 12 Tf 14 TL 72 700 Td (one) Tj (two) ' (three) \" ET";
        assert_eq!(page_text(stream, &HashMap::new()).trim(), "one\ntwo\nthree");
    }

    /// TD 除了移动还设定 leading（取 -ty），后续 T* 要用它
    #[test]
    fn td_sets_the_leading_used_by_star() {
        let stream = b"BT /F1 12 Tf 72 700 TD (a) Tj 0 -20 TD (b) Tj T* (c) Tj T* (d) Tj ET";
        assert_eq!(page_text(stream, &HashMap::new()).trim(), "a\nb\nc\nd");
    }

    /// Tm 直接给定文本矩阵；同一基线上换字体/换矩阵不该拆行
    #[test]
    fn tm_positions_the_baseline() {
        let joined = b"BT /F1 12 Tf 1 0 0 1 72 700 Tm (Left ) Tj ET \
                       BT 1 0 0 1 120 700 Tm (Right) Tj ET";
        assert_eq!(page_text(joined, &HashMap::new()).trim(), "Left Right");
        let broken = b"BT /F1 12 Tf 1 0 0 1 72 700 Tm (a) Tj ET \
                       BT 1 0 0 1 72 680 Tm (b) Tj ET";
        assert_eq!(page_text(broken, &HashMap::new()).trim(), "a\nb");
    }

    /// 整页常包在 q … cm … Q 里（如从别处合并进来的页面）。
    /// 不跟踪 CTM 就看不到 cm 造成的位移，两行会被并成一行。
    #[test]
    fn ctm_translation_moves_the_baseline_too() {
        let stream = b"BT /F1 24 Tf 72 700 Td (Line1) Tj ET \
                       q 1 0 0 1 0 -30 cm BT 72 700 Td (Line2) Tj ET Q";
        assert_eq!(page_text(stream, &HashMap::new()).trim(), "Line1\nLine2");
        // 只有 x 位移时仍算同一行
        let side = b"BT /F1 24 Tf 72 700 Td (So) Tj ET \
                     q 1 0 0 1 30 0 cm BT 72 700 Td (ft) Tj ET Q";
        assert_eq!(page_text(side, &HashMap::new()).trim(), "Soft");
    }

    /// Q 之后要回到保存前的矩阵，否则后面的行会带着上一次的位移
    #[test]
    fn restore_state_puts_the_baseline_back() {
        let stream = b"BT /F1 24 Tf 72 700 Td (A) Tj ET q 1 0 0 1 0 -30 cm \
                       BT 72 700 Td (B) Tj ET Q BT 72 700 Td (C) Tj ET";
        assert_eq!(page_text(stream, &HashMap::new()).trim(), "A\nB\nC");
    }

    /// 行内不该插空格：空格由内容流自己画。中文逐字绘制时插空格会把句子切断。
    #[test]
    fn no_space_is_invented_between_same_line_pieces() {
        let stream = b"BT /F1 24 Tf 72 700 Td (\\344\\275\\240) Tj 12 0 Td (\\345\\245\\275) Tj ET";
        let text = page_text(stream, &HashMap::new());
        assert_eq!(text, "你好");
        assert!(!text.contains(' '), "{:?}", text);
    }

    /// 把若干对象拼成可直接 `Document::load` 的 PDF；对象号即数组下标 + 1。
    fn flat_pdf(objects: &[String]) -> Vec<u8> {
        raw_pdf(
            &objects
                .iter()
                .map(|o| o.as_bytes().to_vec())
                .collect::<Vec<_>>(),
        )
    }

    /// 字节版对象体，供压缩流这类含二进制的对象使用。
    fn raw_pdf(objects: &[Vec<u8>]) -> Vec<u8> {
        let mut body = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, text) in objects.iter().enumerate() {
            offsets.push(body.len());
            body.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            body.extend_from_slice(text);
            body.extend_from_slice(b"\nendobj\n");
        }
        let start = body.len();
        let count = objects.len() + 1;
        body.extend_from_slice(format!("xref\n0 {}\n", count).as_bytes());
        body.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            body.extend_from_slice(format!("{:010} 00000 n \n", offset).as_bytes());
        }
        body.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
                count, start
            )
            .as_bytes(),
        );
        body
    }

    fn zlib(input: &[u8]) -> Vec<u8> {
        use flate2::write::ZlibEncoder;
        use std::io::Write;
        let mut encoder = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(input).unwrap();
        encoder.finish().unwrap()
    }

    /// 一个 FlateDecode 流对象体；真实 PDF 的 /ToUnicode 几乎都是压缩过的。
    fn flate_stream(text: &str) -> Vec<u8> {
        let compressed = zlib(text.as_bytes());
        let mut body = format!(
            "<< /Length {} /Filter /FlateDecode >>\nstream\n",
            compressed.len()
        )
        .into_bytes();
        body.extend_from_slice(&compressed);
        body.extend_from_slice(b"\nendstream");
        body
    }

    fn content_stream(text: &str) -> String {
        format!("<< /Length {} >>\nstream\n{}\nendstream", text.len(), text)
    }

    /// Catalog / Pages / Page 三件套，字体固定引用 5 号对象、内容引用 4 号
    const PAGE_SKELETON: [&str; 3] = [
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 612 792] >>",
        "<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
    ];

    fn page_objects(stream: &str) -> Vec<String> {
        let mut objects: Vec<String> = PAGE_SKELETON.iter().map(|body| body.to_string()).collect();
        objects.push(content_stream(stream));
        objects
    }

    /// license.pdf 的真实问题：MacRoman 的 0xD2/0xD3 是左右双引号，
    /// 不查编码表就会解成 "ÒLICENSEÓ"。
    #[test]
    fn mac_roman_bytes_decode_to_their_real_glyphs() {
        let dir = crate::test_support::TempDir::new("pdftext-macroman");
        let mut objects = page_objects("BT /F1 24 Tf 72 700 Td (\\322LICENSE\\323) Tj ET");
        objects.push(
            "<< /Type /Font /Subtype /Type1 /BaseFont /Test /Encoding /MacRomanEncoding >>"
                .to_string(),
        );
        let file = write_fixture(&dir, "mac.pdf", &flat_pdf(&objects));
        let text = extract_pdf_text(file.to_str().unwrap()).unwrap();
        assert_eq!(text, "\u{201C}LICENSE\u{201D}");
    }

    #[test]
    fn font_differences_override_the_base_encoding() {
        let dir = crate::test_support::TempDir::new("pdftext-diff");
        let mut objects = page_objects("BT /F1 24 Tf 72 700 Td (AB) Tj ET");
        objects.push(
            "<< /Type /Font /Subtype /Type1 /BaseFont /Test /Encoding << /BaseEncoding /MacRomanEncoding /Differences [65 /endash 66 /ellipsis] >> >>"
                .to_string(),
        );
        let file = write_fixture(&dir, "diff.pdf", &flat_pdf(&objects));
        let text = extract_pdf_text(file.to_str().unwrap()).unwrap();
        assert_eq!(text, "\u{2013}\u{2026}");
    }

    #[test]
    fn tounicode_cmap_wins_over_the_encoding_tables() {
        let dir = crate::test_support::TempDir::new("pdftext-cmap");
        let mut objects = page_objects("BT /F1 24 Tf 72 700 Td <0102> Tj ET");
        objects.push(
            "<< /Type /Font /Subtype /TrueType /BaseFont /Test /ToUnicode 6 0 R /Encoding /WinAnsiEncoding >>"
                .to_string(),
        );
        let cmap = "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
                    1 begincodespacerange\n<00> <FF>\nendcodespacerange\n\
                    2 beginbfchar\n<01> <4E2D>\n<02> <6587>\nendbfchar\nendcmap";
        objects.push(content_stream(cmap));
        let file = write_fixture(&dir, "cmap.pdf", &flat_pdf(&objects));
        let text = extract_pdf_text(file.to_str().unwrap()).unwrap();
        assert_eq!(text, "中文");
    }

    /// 未压缩流没有 /Filter，lopdf 的 `decompressed_content()` 会直接报错，
    /// 上面那条测试走的是"原文"分支；真实 PDF 走的是压缩分支，两条都得覆盖。
    #[test]
    fn tounicode_cmap_works_on_a_flate_compressed_stream() {
        let dir = crate::test_support::TempDir::new("pdftext-cmap-zip");
        let cmap = "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
                    1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n\
                    2 beginbfchar\n<0001> <4E2D>\n<0002> <6587>\nendbfchar\nendcmap";
        let mut objects: Vec<Vec<u8>> = page_objects("BT /F1 24 Tf 72 700 Td <00010002> Tj ET")
            .into_iter()
            .map(|body| body.into_bytes())
            .collect();
        objects.push(
            b"<< /Type /Font /Subtype /Type0 /BaseFont /Test /ToUnicode 6 0 R /Encoding /Identity-H >>"
                .to_vec(),
        );
        let compressed = flate_stream(cmap);
        assert!(
            !String::from_utf8_lossy(&compressed).contains("beginbfchar"),
            "fixture 必须真的压缩过，否则这条测试走不到解压分支"
        );
        objects.push(compressed);
        let file = write_fixture(&dir, "cmapzip.pdf", &raw_pdf(&objects));
        let text = extract_pdf_text(file.to_str().unwrap()).unwrap();
        assert_eq!(text, "中文");
    }

    /// 子集字体常把零宽占位之类的排版标记映射到 C0 码位，实测一份 WPS 导出的 PDF 里
    /// "AI<0x01>Agent" 就是这样。这些码位肉眼看不见，却会让前端文本面板出现破框/截断，
    /// 所以从真实文件一路走到 extract_pdf_text 验证它们不会外泄。
    #[test]
    fn invisible_control_codes_never_reach_the_text_panel() {
        let dir = crate::test_support::TempDir::new("pdftext-ctrl");
        let soh = char::from(1);
        let del = char::from(0x7f);
        let stream = format!(
            "BT /F1 24 Tf 72 700 Td (AI{soh}Agent{del}) Tj 0 -28 Td (second{soh}line) Tj ET"
        );
        let mut objects = page_objects(&stream);
        objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string());
        let file = write_fixture(&dir, "ctrl.pdf", &flat_pdf(&objects));
        assert!(
            fs::read(&file).unwrap().contains(&1u8),
            "fixture 必须真的带上控制字符，否则这条测试什么都没测"
        );
        let text = extract_pdf_text(file.to_str().unwrap()).unwrap();
        assert_eq!(text, format!("AIAgent{}secondline", char::from(10)));
        assert!(
            !text.chars().any(|c| c.is_control() && c != char::from(10)),
            "{:?}",
            text
        );
    }

    /// 走 ToUnicode 映射的分支也要清：映射表本身就可能指向 C0 码位。
    /// 期望值 "A" 同时证明走的是映射分支（退回 PDF 字符串编码的话整串会被清成空，命令直接报错）。
    #[test]
    fn control_codes_from_the_tounicode_map_are_dropped() {
        let dir = crate::test_support::TempDir::new("pdftext-ctrl-cmap");
        let mut objects = page_objects("BT /F1 24 Tf 72 700 Td <0102> Tj ET");
        objects.push(
            "<< /Type /Font /Subtype /TrueType /BaseFont /Test /ToUnicode 6 0 R /Encoding /WinAnsiEncoding >>"
                .to_string(),
        );
        let cmap = "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
                    1 begincodespacerange\n<00> <FF>\nendcodespacerange\n\
                    2 beginbfchar\n<01> <0041>\n<02> <0001>\nendbfchar\nendcmap";
        objects.push(content_stream(cmap));
        let file = write_fixture(&dir, "ctrlcmap.pdf", &flat_pdf(&objects));
        let text = extract_pdf_text(file.to_str().unwrap()).unwrap();
        assert_eq!(text, "A");
    }

    /// WPS/旧排版器的子集字体会把汉字映射到 Kangxi 部首码位：屏幕上是同一个字形，
    /// 复制出去却搜不到原字。实测一份 PDF 里 396 个字符属于这类码位。
    #[test]
    fn kangxi_radical_codes_become_real_han_characters() {
        let dir = crate::test_support::TempDir::new("pdftext-kangxi");
        let mut objects = page_objects("BT /F1 24 Tf 72 700 Td <010203> Tj ET");
        objects.push(
            "<< /Type /Font /Subtype /TrueType /BaseFont /Test /ToUnicode 6 0 R /Encoding /WinAnsiEncoding >>"
                .to_string(),
        );
        // <01>→U+2F42(⽂) <02>→U+2F00(⼀) <03>→U+2F83(⾃)：都是兼容码位
        let cmap = "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
                    1 begincodespacerange\n<00> <FF>\nendcodespacerange\n\
                    3 beginbfchar\n<01> <2F42>\n<02> <2F00>\n<03> <2F83>\nendbfchar\nendcmap";
        objects.push(content_stream(cmap));
        let file = write_fixture(&dir, "kangxi.pdf", &flat_pdf(&objects));
        let text = extract_pdf_text(file.to_str().unwrap()).unwrap();
        assert_eq!(text, "文一自");
        assert!(
            !text.chars().any(|c| matches!(c, '\u{2f00}'..='\u{2fd5}')),
            "还留着部首码位: {:?}",
            text
        );
    }

    /// 表是手抄进来的，所以既要能整表自检（每条都落在CJK统一汉字区、顺序不错位），
    /// 也要在首/中/尾三个点上当场对答案。
    #[test]
    fn the_kangxi_table_stays_in_the_ideograph_block() {
        assert_eq!(KANGXI_IDEOGRAPH.len(), 214);
        for (offset, target) in KANGXI_IDEOGRAPH.iter().enumerate() {
            let radical = char::from_u32(0x2F00 + offset as u32).unwrap();
            assert!(
                matches!(target, '\u{4e00}'..='\u{9fff}'),
                "{} 映射到了 {:?}，不在统一汉字区",
                radical,
                target
            );
        }
        assert_eq!(
            display_text(&char::from_u32(0x2F00).unwrap().to_string()),
            "一"
        );
        assert_eq!(
            display_text(&char::from_u32(0x2F42).unwrap().to_string()),
            "文"
        );
        assert_eq!(
            display_text(&char::from_u32(0x2FD5).unwrap().to_string()),
            "龠"
        );
        // 正经汉字、以及部首补充区(U+2E80起，不在归一范围)都不该被改动
        assert_eq!(display_text("本文⻛"), "本文⻛");
    }

    /// 换行与制表是内容流自己表达的排版信息，清洗时必须原样保留；回车归一成换行。
    #[test]
    fn line_breaks_survive_the_scrub() {
        let (soh, del, cr, tab, lf) = (
            char::from(1),
            char::from(0x7f),
            char::from(13),
            char::from(9),
            char::from(10),
        );
        let soft_hyphen = '\u{ad}';
        assert_eq!(
            scrub_invisible(&format!("a{soh}b{del}c{cr}{tab}d{soft_hyphen}e")),
            format!("abc{lf}{tab}de")
        );
        // 影响字形拼接的 Cf 标记不能删：阿拉伯文的数字前缀标记要原样留着
        let arabic = '\u{600}';
        assert_eq!(scrub_invisible(&format!("{arabic}١")), format!("{arabic}١"));
        // 干净文本走快速路径：内容一字不改
        let clean = "普通文本 already fine";
        assert_eq!(scrub_invisible(clean), clean);
    }
}
