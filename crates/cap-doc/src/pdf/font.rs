//! PDF 字体字符映射：把内容流里的字形码还原成 Unicode。
//!
//! 内容流里的字符串存的是字形编码而不是文本：同一串字节在 WinAnsi 下是 `Ô`、
//! 在 MacRoman 下却是 `“`，在子集字体里又完全另有所指。不查映射表，提取出来的
//! 文本就会在真实 PDF 上出现 `ÒLICENSEÓ` 这类看得懂却读不通的乱码。
//! 优先级：/ToUnicode CMap > /Encoding（标准表或 /Differences）。

use lopdf::{Dictionary, Document, Object, ObjectId};
use std::collections::HashMap;

/// 一个字体可用的字形码 → Unicode 映射。
pub(crate) struct FontMap {
    codes: HashMap<u32, String>,
    /// 存在双字节码（Identity-H / 2 字节 codespace）时必须先按两字节取码
    two_byte: bool,
}

impl FontMap {
    fn new(codes: HashMap<u32, String>) -> Option<Self> {
        Self::build(codes, 0)
    }

    /// `code_bytes` 来自 CMap 的 codespacerange：子集 Identity-H 字体的码位
    /// 可能全都 ≤ 0xFF，只看键值会漏判成单字节，把每个字符拆成半截丢掉。
    fn build(codes: HashMap<u32, String>, code_bytes: usize) -> Option<Self> {
        if codes.is_empty() {
            return None;
        }
        let two_byte = code_bytes >= 2 || codes.keys().any(|&code| code > 0xFF);
        Some(Self { codes, two_byte })
    }

    /// 解码字形码序列；查不到的码直接丢掉（宁可少字，不要错字）。
    pub(crate) fn decode(&self, raw: &[u8]) -> String {
        let mut out = String::new();
        let mut i = 0usize;
        while i < raw.len() {
            let (code, step) = if self.two_byte && i + 1 < raw.len() {
                (u32::from(u16::from_be_bytes([raw[i], raw[i + 1]])), 2)
            } else {
                (raw[i] as u32, 1)
            };
            if let Some(text) = self.codes.get(&code) {
                out.push_str(text);
            }
            i += step;
        }
        out
    }
}

/// 解析间接引用，拿到实际对象（`inheritable` 返回的是克隆值，所以这里统一成值）。
fn owned(doc: &Document, object: &Object) -> Option<Object> {
    match object {
        Object::Reference(id) => doc.get_object(*id).ok().cloned(),
        other => Some(other.clone()),
    }
}

/// 收集一页可用的字体映射，键是页面 Resources/Font 里的名字（如 `F1`）。
pub(crate) fn font_maps(doc: &Document, page: ObjectId) -> HashMap<Vec<u8>, FontMap> {
    let mut maps = HashMap::new();
    let Some(resources) = crate::pdf::ops::inheritable(doc, page, b"Resources")
        .and_then(|object| owned(doc, &object))
    else {
        return maps;
    };
    let Object::Dictionary(resources) = resources else {
        return maps;
    };
    let Ok(fonts_value) = resources.get(b"Font") else {
        return maps;
    };
    let Some(fonts_value) = owned(doc, fonts_value) else {
        return maps;
    };
    let Object::Dictionary(fonts) = fonts_value else {
        return maps;
    };
    for (name, reference) in fonts.iter() {
        let Some(font) = owned(doc, reference) else {
            continue;
        };
        let Object::Dictionary(dict) = font else {
            continue;
        };
        if let Some(map) = font_map(doc, &dict) {
            maps.insert(name.to_vec(), map);
        }
    }
    maps
}

fn font_map(doc: &Document, font: &Dictionary) -> Option<FontMap> {
    if let Some(map) = to_unicode_map(doc, font) {
        return Some(map);
    }
    let encoding = owned(doc, font.get(b"Encoding").ok()?)?;
    match encoding {
        Object::Name(name) => FontMap::new(standard_codes(&name)),
        Object::Dictionary(dict) => {
            // 规范：/BaseEncoding 缺省时按 StandardEncoding 解释
            let base = match dict.get(b"BaseEncoding") {
                Ok(Object::Name(name)) => name.to_vec(),
                _ => b"StandardEncoding".to_vec(),
            };
            let mut codes = standard_codes(&base);
            if codes.is_empty() {
                return None;
            }
            if let Ok(value) = dict.get(b"Differences") {
                if let Some(Object::Array(items)) = owned(doc, value) {
                    apply_differences(&mut codes, &items);
                }
            }
            FontMap::new(codes)
        }
        _ => None,
    }
}

fn to_unicode_map(doc: &Document, font: &Dictionary) -> Option<FontMap> {
    let reference = font.get(b"ToUnicode").ok()?;
    let Object::Stream(stream) = owned(doc, reference)? else {
        return None;
    };
    // 没有 /Filter 时 decompressed_content 会直接报错（filters() 要求至少有一个），
    // 未压缩的 CMap 因此必须走原文
    let bytes = if stream.dict.get(b"Filter").is_ok() {
        stream.decompressed_content().ok()?
    } else {
        stream.content.clone()
    };
    let (codes, code_bytes) = parse_cmap(&bytes);
    FontMap::build(codes, code_bytes)
}

fn standard_codes(name: &[u8]) -> HashMap<u32, String> {
    let mut codes = HashMap::new();
    match name {
        b"WinAnsiEncoding" => {
            for byte in 0x20..=0xFFu8 {
                if let Some(ch) = win_ansi(byte) {
                    codes.insert(byte as u32, ch.to_string());
                }
            }
        }
        b"MacRomanEncoding" => {
            for byte in 0x20..=0xFFu8 {
                if let Some(ch) = mac_roman(byte) {
                    codes.insert(byte as u32, ch.to_string());
                }
            }
        }
        b"StandardEncoding" => {
            for byte in 0x20..=0x7Fu8 {
                codes.insert(byte as u32, (byte as char).to_string());
            }
        }
        _ => {}
    }
    codes
}

/// WinAnsi 的 0xA0–0xFF 与 Latin-1 一致，只有 0x80–0x9F 这段需要查表。
const WIN_ANSI_C1: [Option<char>; 32] = [
    Some('\u{20AC}'),
    None,
    Some('\u{201A}'),
    Some('\u{0192}'),
    Some('\u{201E}'),
    Some('\u{2026}'),
    Some('\u{2020}'),
    Some('\u{2021}'),
    Some('\u{02C6}'),
    Some('\u{2030}'),
    Some('\u{0160}'),
    Some('\u{2039}'),
    Some('\u{0152}'),
    None,
    Some('\u{017D}'),
    None,
    None,
    Some('\u{2018}'),
    Some('\u{2019}'),
    Some('\u{201C}'),
    Some('\u{201D}'),
    Some('\u{2022}'),
    Some('\u{2013}'),
    Some('\u{2014}'),
    Some('\u{02DC}'),
    Some('\u{2122}'),
    Some('\u{0161}'),
    Some('\u{203A}'),
    Some('\u{0153}'),
    None,
    Some('\u{017E}'),
    Some('\u{0178}'),
];

fn win_ansi(byte: u8) -> Option<char> {
    match byte {
        0x80..=0x9F => WIN_ANSI_C1[(byte - 0x80) as usize],
        0xA0..=0xFF => char::from_u32(byte as u32),
        _ => Some(byte as char),
    }
}

/// MacRomanEncoding 的 0x80–0xFF，逐字节取自 Apple 的 codec 表。
const MAC_ROMAN_HIGH: [Option<char>; 128] = [
    Some('\u{00C4}'),
    Some('\u{00C5}'),
    Some('\u{00C7}'),
    Some('\u{00C9}'),
    Some('\u{00D1}'),
    Some('\u{00D6}'),
    Some('\u{00DC}'),
    Some('\u{00E1}'),
    Some('\u{00E0}'),
    Some('\u{00E2}'),
    Some('\u{00E4}'),
    Some('\u{00E3}'),
    Some('\u{00E5}'),
    Some('\u{00E7}'),
    Some('\u{00E9}'),
    Some('\u{00E8}'),
    Some('\u{00EA}'),
    Some('\u{00EB}'),
    Some('\u{00ED}'),
    Some('\u{00EC}'),
    Some('\u{00EE}'),
    Some('\u{00EF}'),
    Some('\u{00F1}'),
    Some('\u{00F3}'),
    Some('\u{00F2}'),
    Some('\u{00F4}'),
    Some('\u{00F6}'),
    Some('\u{00F5}'),
    Some('\u{00FA}'),
    Some('\u{00F9}'),
    Some('\u{00FB}'),
    Some('\u{00FC}'),
    Some('\u{2020}'),
    Some('\u{00B0}'),
    Some('\u{00A2}'),
    Some('\u{00A3}'),
    Some('\u{00A7}'),
    Some('\u{2022}'),
    Some('\u{00B6}'),
    Some('\u{00DF}'),
    Some('\u{00AE}'),
    Some('\u{00A9}'),
    Some('\u{2122}'),
    Some('\u{00B4}'),
    Some('\u{00A8}'),
    Some('\u{2260}'),
    Some('\u{00C6}'),
    Some('\u{00D8}'),
    Some('\u{221E}'),
    Some('\u{00B1}'),
    Some('\u{2264}'),
    Some('\u{2265}'),
    Some('\u{00A5}'),
    Some('\u{00B5}'),
    Some('\u{2202}'),
    Some('\u{2211}'),
    Some('\u{220F}'),
    Some('\u{03C0}'),
    Some('\u{222B}'),
    Some('\u{00AA}'),
    Some('\u{00BA}'),
    Some('\u{03A9}'),
    Some('\u{00E6}'),
    Some('\u{00F8}'),
    Some('\u{00BF}'),
    Some('\u{00A1}'),
    Some('\u{00AC}'),
    Some('\u{221A}'),
    Some('\u{0192}'),
    Some('\u{2248}'),
    Some('\u{2206}'),
    Some('\u{00AB}'),
    Some('\u{00BB}'),
    Some('\u{2026}'),
    Some('\u{00A0}'),
    Some('\u{00C0}'),
    Some('\u{00C3}'),
    Some('\u{00D5}'),
    Some('\u{0152}'),
    Some('\u{0153}'),
    Some('\u{2013}'),
    Some('\u{2014}'),
    Some('\u{201C}'),
    Some('\u{201D}'),
    Some('\u{2018}'),
    Some('\u{2019}'),
    Some('\u{00F7}'),
    Some('\u{25CA}'),
    Some('\u{00FF}'),
    Some('\u{0178}'),
    Some('\u{2044}'),
    Some('\u{20AC}'),
    Some('\u{2039}'),
    Some('\u{203A}'),
    Some('\u{FB01}'),
    Some('\u{FB02}'),
    Some('\u{2021}'),
    Some('\u{00B7}'),
    Some('\u{201A}'),
    Some('\u{201E}'),
    Some('\u{2030}'),
    Some('\u{00C2}'),
    Some('\u{00CA}'),
    Some('\u{00C1}'),
    Some('\u{00CB}'),
    Some('\u{00C8}'),
    Some('\u{00CD}'),
    Some('\u{00CE}'),
    Some('\u{00CF}'),
    Some('\u{00CC}'),
    Some('\u{00D3}'),
    Some('\u{00D4}'),
    Some('\u{F8FF}'),
    Some('\u{00D2}'),
    Some('\u{00DA}'),
    Some('\u{00DB}'),
    Some('\u{00D9}'),
    Some('\u{0131}'),
    Some('\u{02C6}'),
    Some('\u{02DC}'),
    Some('\u{00AF}'),
    Some('\u{02D8}'),
    Some('\u{02D9}'),
    Some('\u{02DA}'),
    Some('\u{00B8}'),
    Some('\u{02DD}'),
    Some('\u{02DB}'),
    Some('\u{02C7}'),
];

fn mac_roman(byte: u8) -> Option<char> {
    match byte {
        0x80..=0xFF => MAC_ROMAN_HIGH[(byte - 0x80) as usize],
        _ => Some(byte as char),
    }
}

/// 解析 /ToUnicode CMap 的 bfchar 与 bfrange（含数组形式）。
/// 第二项是 codespacerange 声明的码字节宽，0 表示 CMap 里没写。
pub(crate) fn parse_cmap(stream: &[u8]) -> (HashMap<u32, String>, usize) {
    let mut codes = HashMap::new();
    let text = String::from_utf8_lossy(stream);
    let code_bytes = codespace_width(&text);
    for (keyword, end_keyword) in [("beginbfchar", "endbfchar"), ("beginbfrange", "endbfrange")] {
        let mut search = 0usize;
        while let Some(found) = text[search..].find(keyword) {
            let start = search + found + keyword.len();
            let Some(stopped) = text[start..].find(end_keyword) else {
                break;
            };
            let block = &text[start..start + stopped];
            for line in block.lines() {
                let items = hex_items(line);
                if keyword == "beginbfchar" && items.len() >= 2 {
                    if let Some(code) = single_code(&items[0]) {
                        let value = utf16_hex(&items[1]);
                        if !value.is_empty() {
                            codes.insert(code, value);
                        }
                    }
                } else if keyword == "beginbfrange" && items.len() >= 3 {
                    let (Some(low), Some(high)) = (single_code(&items[0]), single_code(&items[1]))
                    else {
                        continue;
                    };
                    if let Some(list) = items[2].strip_prefix('[') {
                        // 直接把带括号的整体再丢给 hex_items，会把中括号当成一个元素吞掉
                        let list = list.trim_end_matches(']');
                        for (offset, hex) in hex_items(list).iter().enumerate() {
                            let code = low + offset as u32;
                            if code > high {
                                break;
                            }
                            let value = utf16_hex(hex);
                            if !value.is_empty() {
                                codes.insert(code, value);
                            }
                        }
                    } else if let Some(base) = single_code(&items[2]) {
                        for offset in 0..=(high - low) {
                            if let Some(ch) = char::from_u32(base + offset) {
                                codes.insert(low + offset, ch.to_string());
                            }
                        }
                    }
                }
            }
            search = start + stopped + end_keyword.len();
        }
    }
    (codes, code_bytes)
}

/// codespacerange 里最大的码字节宽：`<0000> <FFFF>` 就是 2 字节码。
fn codespace_width(text: &str) -> usize {
    const BEGIN: &str = "begincodespacerange";
    const END: &str = "endcodespacerange";
    let mut width = 0usize;
    let mut search = 0usize;
    while let Some(found) = text[search..].find(BEGIN) {
        let start = found + BEGIN.len();
        let Some(stopped) = text[start..].find(END) else {
            break;
        };
        for line in text[start..start + stopped].lines() {
            for hex in hex_items(line) {
                width = width.max(hex.trim().len() / 2);
            }
        }
        search = start + stopped + END.len();
    }
    width
}

/// 取出一行里所有 `<...>` 段；数组整体作为一个元素返回（带方括号）。
fn hex_items(line: &str) -> Vec<String> {
    let mut items = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'<' => {
                let Some(end) = bytes[i + 1..].iter().position(|&b| b == b'>') else {
                    break;
                };
                items.push(line[i + 1..i + 1 + end].to_string());
                i += end + 2;
            }
            b'[' => {
                let Some(end) = bytes[i..].iter().position(|&b| b == b']') else {
                    break;
                };
                items.push(line[i..i + end + 1].to_string());
                i += end + 1;
            }
            _ => i += 1,
        }
    }
    items
}

fn single_code(hex: &str) -> Option<u32> {
    let digits: String = hex.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if digits.is_empty() || digits.len() > 8 {
        return None;
    }
    u32::from_str_radix(&digits, 16).ok()
}

/// 目标码位串：一到多个 16 位 UTF-16 单元，可能带代理对。
fn utf16_hex(hex: &str) -> String {
    let units: Vec<u16> = hex
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<Vec<char>>()
        .chunks(4)
        .filter(|chunk| chunk.len() == 4)
        .map(|chunk| {
            let text: String = chunk.iter().collect();
            u16::from_str_radix(&text, 16).unwrap_or(0xFFFD)
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// 字形名 → Unicode，用于 /Encoding 的 /Differences 数组。
/// `uniXXXX`、`uXXXX…` 是子集字体的主流写法，其余只列真实文档里常见的标准名。
pub(crate) fn glyph_to_unicode(name: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(name).ok()?;
    if let Some(hex) = text.strip_prefix("uni") {
        if !hex.is_empty() && hex.len() % 4 == 0 {
            let value = utf16_hex(hex);
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    if let Some(hex) = text.strip_prefix('u') {
        if !hex.is_empty() && hex.len() <= 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            let code = u32::from_str_radix(hex, 16).ok()?;
            return char::from_u32(code).map(|c| c.to_string());
        }
    }
    let mapped = match text {
        "space" => ' ',
        "exclam" => '!',
        "quotedbl" => '"',
        "numbersign" => '#',
        "dollar" => '$',
        "percent" => '%',
        "ampersand" => '&',
        "quotesingle" => '\'',
        "parenleft" => '(',
        "parenright" => ')',
        "asterisk" => '*',
        "plus" => '+',
        "comma" => ',',
        "hyphen" => '-',
        "period" => '.',
        "slash" => '/',
        "colon" => ':',
        "semicolon" => ';',
        "less" => '<',
        "equal" => '=',
        "greater" => '>',
        "question" => '?',
        "at" => '@',
        "bracketleft" => '[',
        "backslash" => '\\',
        "bracketright" => ']',
        "asciicircum" => '^',
        "underscore" => '_',
        "grave" => '`',
        "braceleft" => '{',
        "bar" => '|',
        "braceright" => '}',
        "asciitilde" => '~',
        "quotedblleft" => '\u{201C}',
        "quotedblright" => '\u{201D}',
        "quoteleft" => '\u{2018}',
        "quoteright" => '\u{2019}',
        "quotedblbase" => '\u{201E}',
        "quotesinglbase" => '\u{201A}',
        "endash" => '\u{2013}',
        "emdash" => '\u{2014}',
        "ellipsis" => '\u{2026}',
        "bullet" => '\u{2022}',
        "ff" => '\u{FB00}',
        "fi" => '\u{FB01}',
        "fl" => '\u{FB02}',
        "ffi" => '\u{FB03}',
        "ffl" => '\u{FB04}',
        "copyright" => '\u{00A9}',
        "registered" => '\u{00AE}',
        "trademark" => '\u{2122}',
        "section" => '\u{00A7}',
        "paragraph" => '\u{00B6}',
        "degree" => '\u{00B0}',
        "plusminus" => '\u{00B1}',
        "multiply" => '\u{00D7}',
        "divide" => '\u{00F7}',
        "cent" => '\u{00A2}',
        "sterling" => '\u{00A3}',
        "yen" => '\u{00A5}',
        "euro" => '\u{20AC}',
        "onequarter" => '\u{00BC}',
        "onehalf" => '\u{00BD}',
        "threequarters" => '\u{00BE}',
        "ordfeminine" => '\u{00AA}',
        "ordmasculine" => '\u{00BA}',
        "zero" => '0',
        "one" => '1',
        "two" => '2',
        "three" => '3',
        "four" => '4',
        "five" => '5',
        "six" => '6',
        "seven" => '7',
        "eight" => '8',
        "nine" => '9',
        single if single.chars().count() == 1 => single.chars().next()?,
        _ => return None,
    };
    Some(mapped.to_string())
}

/// 应用 /Differences：`[ 1 /one 2 /two ]`，数字设定码位，其后连续的名字依次占位。
pub(crate) fn apply_differences(base: &mut HashMap<u32, String>, items: &[Object]) {
    let mut code = 0u32;
    for item in items {
        match item {
            Object::Integer(number) => code = (*number).max(0) as u32,
            Object::Name(name) => {
                if let Some(text) = glyph_to_unicode(name) {
                    base.insert(code, text);
                }
                code += 1;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmap_covers_bfchar_and_both_bfrange_forms() {
        let stream = br#"
/CIDInit /ProcSet findresource begin
12 dict begin
begincmap
1 begincodespacerange <0000> <FFFF> endcodespacerange
2 beginbfchar
<0001> <0041>
<0002> <004C 0049>
endbfchar
2 beginbfrange
<0010> <0012> <0061>
<0020> <0021> [<4E2D> <6587>]
endbfrange
endcmap
"#;
        let (codes, code_bytes) = parse_cmap(stream);
        assert_eq!(code_bytes, 2, "codespacerange 声明的是两字节码");
        assert_eq!(codes.get(&1).map(String::as_str), Some("A"));
        assert_eq!(codes.get(&2).map(String::as_str), Some("LI"));
        assert_eq!(codes.get(&0x10).map(String::as_str), Some("a"));
        assert_eq!(codes.get(&0x12).map(String::as_str), Some("c"));
        assert_eq!(codes.get(&0x20).map(String::as_str), Some("中"));
        assert_eq!(codes.get(&0x21).map(String::as_str), Some("文"));
        // 码值全都 ≤ 0xFF，只按键值判断会漏判成单字节，必须按 codespace 宽度取码
        let map = FontMap::build(codes, code_bytes).unwrap();
        assert!(map.two_byte);
        assert_eq!(map.decode(&[0x00, 0x01, 0x00, 0x12]), "Ac");
        assert_eq!(map.decode(&[0x00, 0x20, 0x00, 0x21]), "中文");
    }

    #[test]
    fn unmapped_codes_are_dropped_and_two_byte_packs_first() {
        let mut codes = HashMap::new();
        codes.insert(0x1234u32, "字".to_string());
        codes.insert(0x5678u32, "幕".to_string());
        let map = FontMap::new(codes).unwrap();
        assert_eq!(map.decode(&[0x12, 0x34, 0x56, 0x78]), "字幕");
        // 落单的半个码位不该拼出任何东西
        assert_eq!(map.decode(&[0x12]), "");
    }

    #[test]
    fn encoding_tables_fix_the_real_mojibake_cases() {
        // license.pdf 里的 (ÒLICENSEÓ)：MacRoman 下 0xD2/0xD3 其实是左右双引号
        assert_eq!(mac_roman(0xD2), Some('\u{201C}'));
        assert_eq!(mac_roman(0xD3), Some('\u{201D}'));
        // 同一字节在 WinAnsi 下才是带重音的拉丁字母
        assert_eq!(win_ansi(0xD2), Some('Ò'));
        assert_eq!(win_ansi(0x93), Some('\u{201C}'));
        assert_eq!(win_ansi(0x81), None, "WinAnsi 未定义位必须留空，不能猜");
        assert_eq!(win_ansi(b'A'), Some('A'));
        assert_eq!(mac_roman(b'A'), Some('A'));
    }

    #[test]
    fn glyph_names_cover_subset_and_typographic_forms() {
        assert_eq!(glyph_to_unicode(b"uni4E2D").as_deref(), Some("中"));
        assert_eq!(glyph_to_unicode(b"u1F600").as_deref(), Some("\u{1F600}"));
        // 代理对要成对才能拼出字符；落单的代理位只能替换成 U+FFFD
        assert_eq!(
            glyph_to_unicode(b"uniD83DDE00").as_deref(),
            Some("\u{1F600}")
        );
        assert_eq!(glyph_to_unicode(b"uniD83D").as_deref(), Some("\u{FFFD}"));
        assert_eq!(
            glyph_to_unicode(b"quotedblleft").as_deref(),
            Some("\u{201C}")
        );
        assert_eq!(glyph_to_unicode(b"endash").as_deref(), Some("\u{2013}"));
        assert_eq!(glyph_to_unicode(b"fi").as_deref(), Some("\u{FB01}"));
        assert_eq!(glyph_to_unicode(b"g").as_deref(), Some("g"));
        assert_eq!(glyph_to_unicode(b"space").as_deref(), Some(" "));
        assert_eq!(glyph_to_unicode(b"someUnknownGlyphName"), None);
    }

    #[test]
    fn differences_remap_codes_on_top_of_the_base_encoding() {
        let mut codes = standard_codes(b"MacRomanEncoding");
        assert_eq!(codes.get(&0x41).map(String::as_str), Some("A"));
        let items = vec![
            Object::Integer(65),
            Object::Name(b"bracketleft".to_vec()),
            Object::Name(b"bracketright".to_vec()),
        ];
        apply_differences(&mut codes, &items);
        // /Differences 覆盖了 0x41 起的两个码位
        assert_eq!(codes.get(&65).map(String::as_str), Some("["));
        assert_eq!(codes.get(&66).map(String::as_str), Some("]"));
        assert_eq!(
            codes.get(&67).map(String::as_str),
            Some("C"),
            "表外的码位保持原编码"
        );
    }
}
