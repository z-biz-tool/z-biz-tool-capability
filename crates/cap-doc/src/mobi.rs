//! MOBI / AZW / AZW3 / PRC 解析。
//!
//! ## 为什么是手写而不是用现成 crate
//!
//! `mobi` crate 的结构解析没问题，但它在解码这一步只认 UTF-8 和 cp1252 ——
//! 源码注释写得很直白："There are only two supported encodings in mobi format"。
//! 而中文 MOBI（尤其早些年从 GBK 源打包的）`text_encoding` 是 936，走它的
//! `from_utf8_lossy` 会**把每个汉字换成 U+FFFD**，且原始字节当场丢弃、无法补救。
//! 正文解压是这条链路里唯一有损的一步，所以自己拿住字节、自己解码。
//!
//! ## 处理链路
//!
//! ```text
//! PalmDB 容器
//!   ├─ 记录表（78 字节头 + n×8 字节偏移）
//!   ├─ 记录 0：PalmDOC header(16) + MOBI header + EXTH + 书名
//!   ├─ 记录 1..first_non_book：正文（按 compression 解压）
//!   └─ 记录 first_huff..+count：HUFF 词表 + CDIC 字典
//! ```
//!
//! 解压有三种：不压缩(1) / PalmDOC LZ77(2) / HUFF-CDIC(17480)。
//! 三者解出来都是**字节流**，必须**全部拼完再统一解码** —— 逐记录解码会把
//! 跨 4096 字节记录边界的多字节字符劈成两半，每章结尾出现一个乱码方块。
//!
//! ## AZW / AZW3 的边界
//!
//! AZW 就是 MOBI。AZW3 是 KF8：同样是 PalmDB 容器，但正文按 skeleton+fragment
//! 组织，纯 KF8 文件没有 MOBI7 那段文本。双格式 AZW3 能读到 MOBI7 部分（旧排版），
//! 纯 KF8 会解出空正文 —— 这时给出明确的「用外部应用打开」而不是当作空书。

use std::fs;
use std::path::Path;

use encoding_rs::GB18030;

use crate::ebook::{Chapter, EpubBook};

// ───────────────────────── 格式常量 ─────────────────────────

/// PalmDB 固定头长度：之后紧跟 n 条 8 字节的记录信息
const PALMDB_HEADER_LEN: usize = 78;
const RECORD_INFO_LEN: usize = 8;

const COMPRESSION_NONE: u16 = 1;
const COMPRESSION_PALMDOC: u16 = 2;
const COMPRESSION_HUFF: u16 = 17480;

// ───────────────────────── 入口 ─────────────────────────

/// 解析 MOBI 家族，输出与 EPUB 相同的书籍结构。
pub fn parse(path: &str) -> Result<EpubBook, String> {
    let bytes = fs::read(Path::new(path)).map_err(|e| format!("读取失败: {}", e))?;
    if bytes.len() < PALMDB_HEADER_LEN {
        return Err("文件过小，不是有效的 MOBI".to_string());
    }

    let records = split_records(&bytes)?;
    if records.is_empty() {
        return Err("MOBI 里没有任何记录".to_string());
    }

    let header = MobiHeader::parse(records[0])?;
    if header.encryption != 0 {
        return Err("这本书带 DRM 保护，无法解析".to_string());
    }

    let raw = header.read_text(&records)?;
    let text = decode_bytes(&raw, header.text_encoding);
    if text.trim().is_empty() {
        return Err(
            "没有解出正文。这本书可能是纯 KF8（AZW3）排版，需要先在阅读器里转成 EPUB".to_string(),
        );
    }

    let title = if header.title.trim().is_empty() {
        Path::new(path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "未知书名".to_string())
    } else {
        header.title.trim().to_string()
    };

    let author = header
        .exth
        .author()
        .unwrap_or_else(|| "未知作者".to_string());

    let chapters = split_chapters(&text);

    Ok(EpubBook {
        title,
        author,
        chapters,
        cover_path: String::new(),
    })
}

// ───────────────────────── PalmDB 容器 ─────────────────────────

/// 按记录表把整个文件切成切片。任何越界的记录都退化成空切片，
/// 不做「跳过」—— 记录下标要和 header 里引用的下标严格对齐。
fn split_records(bytes: &[u8]) -> Result<Vec<&[u8]>, String> {
    let count = be16(bytes, 76) as usize;
    if count == 0 {
        return Err("记录数为 0".to_string());
    }
    let table_end = PALMDB_HEADER_LEN + count * RECORD_INFO_LEN;
    if bytes.len() < table_end {
        return Err("记录表超出文件长度".to_string());
    }

    let mut offsets = Vec::with_capacity(count);
    for i in 0..count {
        let p = PALMDB_HEADER_LEN + i * RECORD_INFO_LEN;
        offsets.push(be32(bytes, p) as usize);
    }

    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let start = offsets[i];
        let end = if i + 1 < count {
            offsets[i + 1]
        } else {
            bytes.len()
        };
        if start >= end || start >= bytes.len() {
            out.push(&bytes[0..0]);
        } else {
            out.push(&bytes[start..end.min(bytes.len())]);
        }
    }
    Ok(out)
}

// ───────────────────────── 头部 ─────────────────────────

struct MobiHeader {
    compression: u16,
    encryption: u16,
    text_encoding: u32,
    text_length: u32,
    text_record_count: u16,
    first_non_book: usize,
    first_huff: usize,
    huff_count: usize,
    title: String,
    exth: Exth,
}

impl MobiHeader {
    fn parse(rec0: &[u8]) -> Result<Self, String> {
        if rec0.len() < 16 {
            return Err("记录 0 过短，不像 MOBI".to_string());
        }

        // PalmDOC header（任何 MOBI 都有这 16 字节）
        let compression = be16(rec0, 0);
        let text_length = be32(rec0, 4);
        let text_record_count = be16(rec0, 8);
        let encryption = be16(rec0, 12);

        let mut h = MobiHeader {
            compression,
            encryption,
            text_encoding: 0,
            text_length,
            text_record_count,
            first_non_book: 0,
            first_huff: 0,
            huff_count: 0,
            title: String::new(),
            exth: Exth::default(),
        };

        // MOBI header 从记录 0 的第 16 字节开始。老 PRC 没有这一段，
        // 只剩 PalmDOC 头 + 正文，那种情况下面的字段保持默认值即可。
        if rec0.len() >= 132 && rec0.get(16..20) == Some(b"MOBI") {
            let header_len = be32(rec0, 20) as usize;
            h.text_encoding = be32(rec0, 28);
            h.first_non_book = be32(rec0, 80) as usize;
            let name_off = be32(rec0, 84) as usize;
            let name_len = be32(rec0, 88) as usize;
            h.first_huff = be32(rec0, 112) as usize;
            h.huff_count = be32(rec0, 116) as usize;

            // 书名偏移是相对记录 0 起点的
            if name_len > 0 && name_off + name_len <= rec0.len() {
                h.title = decode_bytes(&rec0[name_off..name_off + name_len], h.text_encoding);
            }

            if header_len > 0 {
                h.exth = Exth::parse(rec0, 16 + header_len);
            }
        }

        Ok(h)
    }

    /// 取正文区间并解压。返回的字节流尚未解码 —— 编码必须在拼接完之后统一做。
    fn read_text(&self, records: &[&[u8]]) -> Result<Vec<u8>, String> {
        let end = self.text_end(records.len());
        if end <= 1 {
            return Err("没有正文记录".to_string());
        }

        let mut out = Vec::with_capacity(self.text_length as usize + 1024);

        match self.compression {
            COMPRESSION_NONE => {
                for r in &records[1..end] {
                    out.extend_from_slice(r);
                }
            }
            COMPRESSION_PALMDOC => {
                for r in &records[1..end] {
                    palmdoc_decompress(r, &mut out);
                }
            }
            COMPRESSION_HUFF => {
                let table = HuffTable::parse(records, self.first_huff, self.huff_count)?;
                for r in &records[1..end] {
                    table.decompress(r, &mut out)?;
                }
            }
            other => {
                return Err(format!("未知的压缩方式 {}，需要更新解析器", other));
            }
        }

        // text_length 是解压后的真实长度（不含记录补齐字节），能裁就裁。
        // 裁不掉也无所谓：多出来的尾部填充会在分章时被丢掉。
        let want = self.text_length as usize;
        if want > 0 && out.len() > want {
            out.truncate(want);
        }
        Ok(out)
    }

    /// 正文记录的上界。
    ///
    /// 首选 MOBI header 的 `first_non_book_index`（图片/索引从这里开始），
    /// 它缺失时退回 PalmDOC 的 `record_count`，再不行就取到文件末尾。
    fn text_end(&self, total: usize) -> usize {
        if self.first_non_book > 1 && self.first_non_book <= total {
            return self.first_non_book;
        }
        if self.text_record_count > 0 {
            return (1 + self.text_record_count as usize).min(total);
        }
        total
    }
}

// ───────────────────────── EXTH 元数据 ─────────────────────────

#[derive(Default)]
struct Exth {
    author: Option<String>,
    publisher: Option<String>,
    description: Option<String>,
    isbn: Option<String>,
}

impl Exth {
    fn parse(rec0: &[u8], start: usize) -> Exth {
        let mut out = Exth::default();
        if rec0.len() < start + 12 || rec0.get(start..start + 4) != Some(b"EXTH") {
            return out;
        }
        let count = be32(rec0, start + 8);
        let mut p = start + 12;

        for _ in 0..count {
            if p + 8 > rec0.len() {
                break;
            }
            let kind = be32(rec0, p);
            let len = be32(rec0, p + 4) as usize;
            // 长度包含这 8 字节头，且必须落在记录内；异常就整段放弃，
            // 免得后面所有条目都按错位的偏移读出一堆垃圾。
            if len < 8 || p + len > rec0.len() {
                break;
            }
            let data = &rec0[p + 8..p + len];
            let text = String::from_utf8_lossy(data).trim().to_string();
            if !text.is_empty() {
                match kind {
                    100 => out.author = Some(text),
                    101 => out.publisher = Some(text),
                    103 => out.description = Some(text),
                    104 => out.isbn = Some(text),
                    _ => {}
                }
            }
            p += len;
        }
        out
    }

    fn author(&self) -> Option<String> {
        self.author.clone()
    }
}

// ───────────────────────── PalmDOC 解压 ─────────────────────────

/// PalmDOC LZ77 变体。输出**追加**到 `out`，以便多条记录拼成一条连续文本。
///
/// 关键点是回溯复制的**重叠**：`dist` 可能小于 `len`，此时后半个拷贝会读到
/// 刚写出去的字节 —— 这正是它用来压缩重复字符（缩进、分隔线）的手段，不是坏数据。
pub fn palmdoc_decompress(src: &[u8], out: &mut Vec<u8>) {
    let mut i = 0usize;
    while i < src.len() {
        let b = src[i];
        i += 1;
        match b {
            // 0x00 是字面 NUL，HTML 里不该出现但也不能吞
            0x00 => out.push(0),
            // 1..8：紧随其后的 n 个字节是字面量
            0x01..=0x08 => {
                let n = b as usize;
                let end = (i + n).min(src.len());
                out.extend_from_slice(&src[i..end]);
                i = end;
            }
            // 9..0x7f：单字节字面量
            0x09..=0x7f => out.push(b),
            // 0x80..0xbf：两字节构成 (距离, 长度)
            0x80..=0xbf => {
                if i >= src.len() {
                    break;
                }
                let pair = ((b as u16) << 8) | src[i] as u16;
                i += 1;
                let dist = ((pair >> 3) & 0x07ff) as usize;
                let len = ((pair & 0x07) + 3) as usize;
                if dist == 0 || dist > out.len() {
                    continue;
                }
                let start = out.len() - dist;
                for k in 0..len {
                    let c = out[start + k];
                    out.push(c);
                }
            }
            // 0xc0..0xff：空格 + (b ^ 0x80)，专为英文词间空格做的编码
            _ => {
                out.push(b' ');
                out.push(b ^ 0x80);
            }
        }
    }
}

// ───────────────────────── HUFF / CDIC 解压 ─────────────────────────

/// HUFF 词表 + CDIC 字典。
///
/// 规范见 MobileRead 的 "Decompressing Huffman Records"：
/// 记录 0 是 HUFF（内含 256 项码表、32 组最小/最大码），其后的记录是 CDIC
/// （每项 16 位偏移指向一段短语，短语本身可能还是 PalmDOC 压缩的）。
struct HuffTable {
    /// 256 项：按码值高字节索引 → (码长, 是否终止, 该码长下的最大码)
    code_dict: Vec<(u8, bool, u32)>,
    /// 下标即码长，1..=32
    min_codes: [u32; 33],
    max_codes: [u32; 33],
    phrases: Vec<Vec<u8>>,
}

impl HuffTable {
    fn parse(records: &[&[u8]], start: usize, count: usize) -> Result<HuffTable, String> {
        if count == 0 || start == 0 || start + count > records.len() {
            return Err("HUFF 记录索引越界".to_string());
        }
        let huff = records[start];
        if huff.len() < 16 || huff.get(0..4) != Some(b"HUFF") {
            return Err("HUFF 记录头无效".to_string());
        }

        let dict_off = be32(huff, 8) as usize;
        let minmax_off = be32(huff, 12) as usize;

        // ── 256 项码表 ──
        let mut code_dict = Vec::with_capacity(256);
        for i in 0..256 {
            let v = be32(huff, dict_off + i * 4);
            let max_val = (v & 0x00ff_ffff) as u64;
            let term = (v >> 24) & 1 == 1;
            let code_len = ((v >> 27) & 0x1f) as u8;
            let max_code = if code_len == 0 {
                0
            } else {
                // ((max + 1) << (32 - len)) - 1，用 u64 算防止左移溢出
                (((max_val + 1) << (32 - code_len as u32)).wrapping_sub(1)) as u32
            };
            code_dict.push((code_len, term, max_code));
        }

        // ── 32 组最小/最大码，下标 1..=32 就是码长 ──
        let mut min_codes = [0u32; 33];
        let mut max_codes = [0xffff_ffffu32; 33];
        for len in 1..=32usize {
            let p = minmax_off + (len - 1) * 8;
            let mn = be32(huff, p) as u64;
            let mx = be32(huff, p + 4) as u64;
            let shift = 32 - len as u32;
            min_codes[len] = (mn << shift) as u32;
            max_codes[len] = if len == 32 {
                // 32 位码长时区间顶到满，避免 (x+1)<<0 的边界写法
                0xffff_ffff
            } else {
                (((mx + 1) << shift).wrapping_sub(1)) as u32
            };
        }

        // ── CDIC 短语表 ──
        let mut phrases: Vec<Vec<u8>> = Vec::new();
        for r in &records[start + 1..start + count] {
            if r.len() < 16 || r.get(0..4) != Some(b"CDIC") {
                continue;
            }
            let num = be32(r, 8) as usize;
            let bits = be32(r, 12) as usize;
            // bits 是「本记录能贡献多少条的指数」，超过 15 说明数据坏了
            if bits > 15 {
                continue;
            }
            // 每条 CDIC 记录只补「还差多少条」那么多，不能各记录都按 2^bits 读满
            let want = (1usize << bits).min(num.saturating_sub(phrases.len()));

            for i in 0..want {
                let off = be16(r, 16 + i * 2) as usize;
                if off + 2 > r.len() {
                    phrases.push(Vec::new());
                    continue;
                }
                let v = be16(r, off);
                let compressed = (v & 0x8000) != 0;
                let len = (v & 0x7fff) as usize;
                let s = off + 2;
                let e = (s + len).min(r.len());
                if s > e {
                    phrases.push(Vec::new());
                    continue;
                }
                let slice = &r[s..e];
                if compressed {
                    let mut buf = Vec::new();
                    palmdoc_decompress(slice, &mut buf);
                    phrases.push(buf);
                } else {
                    phrases.push(slice.to_vec());
                }
            }
        }

        if phrases.is_empty() {
            return Err("CDIC 字典为空".to_string());
        }

        Ok(HuffTable {
            code_dict,
            min_codes,
            max_codes,
            phrases,
        })
    }

    /// 逐码解压。一次读 32 位做前缀匹配，命中码长后整体前进。
    fn decompress(&self, data: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
        let total_bits = data.len() * 8;
        let mut bit = 0usize;

        while bit < total_bits {
            let code = read_bits32(data, bit);
            let (mut code_len, term, mut max_code) = self.code_dict[(code >> 24) as usize];

            if !term {
                // 该长度的区间装不下这个码，就往更长的码长找，
                // 直到找到第一个「最小码 <= code」的区间
                while (code_len as usize) < 32 && code < self.min_codes[code_len as usize] {
                    code_len += 1;
                }
                max_code = self.max_codes[code_len as usize];
            }
            if code_len == 0 {
                return Err("HUFF 码长为 0，词表已损坏".to_string());
            }

            // index = (max_code - code) >> (32 - code_len)
            let index = (max_code.wrapping_sub(code) >> (32 - code_len as u32)) as usize;
            let phrase = self
                .phrases
                .get(index)
                .ok_or_else(|| format!("HUFF 字典索引 {} 越界", index))?;
            out.extend_from_slice(phrase);
            bit += code_len as usize;
        }
        Ok(())
    }
}

/// 从 `bit_pos` 起读 32 位，MSB 在前，越过末尾的位补 0。
///
/// 一次取 5 个字节拼成 40 位再移位，避免逐位循环 —— 一本书要解几百万个码，
/// 逐位写法在几 MB 的书上会明显拖时间。
fn read_bits32(data: &[u8], bit_pos: usize) -> u32 {
    let byte = bit_pos / 8;
    let shift = bit_pos % 8;
    let mut acc: u64 = 0;
    for i in 0..5 {
        acc = (acc << 8) | data.get(byte + i).copied().unwrap_or(0) as u64;
    }
    ((acc >> (8 - shift)) & 0xffff_ffff) as u32
}

// ───────────────────────── 编码 ─────────────────────────

/// 把正文字节解成字符串。
///
/// 先试严格 UTF-8：MOBI 声明里 `1252` 但正文其实是 UTF-8 的情况并不少见
/// （calibre 某些版本这么写），而「严格 UTF-8 能通过」基本就排除了单字节
/// 编码的可能。不通过再按声明走，`936` 用 GB18030（GBK 的超集）兜底 ——
/// 对中文读者来说，把 cp1252 错当 GBK 顶多是乱码，把 GBK 错当 cp1252
/// 会变成一串带重音的拉丁字母，两种都读不了，但后者更没法猜。
fn decode_bytes(bytes: &[u8], declared: u32) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    match declared {
        65001 => String::from_utf8_lossy(bytes).to_string(),
        1200 => encoding_rs::UTF_16LE.decode(bytes).0.into_owned(),
        1201 => encoding_rs::UTF_16BE.decode(bytes).0.into_owned(),
        1252 => encoding_rs::WINDOWS_1252.decode(bytes).0.into_owned(),
        // 936=GBK、54936=GB18030。encoding_rs 只提供 GB18030，它包含 GBK。
        _ => GB18030.decode(bytes).0.into_owned(),
    }
}

// ───────────────────────── 轻量元数据 ─────────────────────────

/// 只读书名与作者，不解压正文。
///
/// 扫描书库时会逐本调用：几千本书如果每本都跑完整 [`parse`]（解压全书），
/// 扫描会从「几秒」变成「几分钟」。record 0 在文件开头附近，读前 256 KB
/// 就够 —— EXTH 和书名都挂在那里。
pub fn read_meta(path: &str) -> Option<(String, String)> {
    use std::io::{Read, Seek, SeekFrom};

    let mut f = fs::File::open(Path::new(path)).ok()?;
    let total = f.metadata().ok()?.len() as usize;
    let head_len = total.min(256 * 1024);
    let mut head = vec![0u8; head_len];
    f.read_exact(&mut head).ok()?;

    if head.len() < PALMDB_HEADER_LEN {
        return None;
    }
    let count = be16(&head, 76) as usize;
    if count < 2 {
        return None;
    }
    let rec0_start = be32(&head, PALMDB_HEADER_LEN) as usize;
    let rec0_end = be32(&head, PALMDB_HEADER_LEN + RECORD_INFO_LEN) as usize;
    if rec0_start >= rec0_end {
        return None;
    }

    // record 0 可能超出已读窗口（极少见，比如前面塞了大量封面）
    let rec0: Vec<u8> = if rec0_end <= head.len() {
        head[rec0_start..rec0_end].to_vec()
    } else {
        let mut buf = vec![0u8; rec0_end - rec0_start];
        f.seek(SeekFrom::Start(rec0_start as u64)).ok()?;
        f.read_exact(&mut buf).ok()?;
        buf
    };

    let h = MobiHeader::parse(&rec0).ok()?;
    let title = h.title.trim().to_string();
    let author = h.exth.author().unwrap_or_default();
    if title.is_empty() && author.is_empty() {
        return None;
    }
    Some((title, author))
}

// ───────────────────────── 分章 ─────────────────────────

/// MOBI 正文是一整块 HTML，靠 `<mbp:pagebreak/>` 分页。
/// 没有分页符的书（少数）、以及单页过长的情况，再按标题或长度切。
fn split_chapters(raw: &str) -> Vec<Chapter> {
    let cleaned = sanitize_mobi_html(raw);
    let pages = split_on_pagebreak(&cleaned);

    let mut out = Vec::new();
    for page in pages {
        let page = page.trim();
        if page.is_empty() {
            continue;
        }
        // 一页太长就按标题再切：几千字的单页在翻页阅读器里会变成「一页读到吐」
        let blocks: Vec<String> = if page.len() > 6000 {
            split_on_headings(page)
        } else {
            vec![page.to_string()]
        };
        for b in blocks {
            let b = b.trim();
            if b.is_empty() {
                continue;
            }
            // 只有标签没有字的空壳（老文件的 <html><body> 残留）不该占一章。
            // 但「整本书只有一句话」时不能把它也滤掉，所以只在已有章节时才丢。
            if !out.is_empty() && strip_tags(b).chars().count() < 5 {
                continue;
            }
            let title = first_heading_text(b).unwrap_or_else(|| format!("第 {} 节", out.len() + 1));
            out.push(Chapter {
                title,
                content: b.to_string(),
                index: out.len(),
            });
        }
    }

    if out.is_empty() {
        out.push(Chapter {
            title: "全文".to_string(),
            content: "<p>这本书没有可显示的正文。</p>".to_string(),
            index: 0,
        });
    }
    out
}

fn split_on_pagebreak(html: &str) -> Vec<String> {
    const MARK: &str = "<mbp:pagebreak";
    if !html.contains(MARK) {
        return vec![html.to_string()];
    }
    let mut parts = Vec::new();
    let mut rest = html;
    while let Some(pos) = rest.find(MARK) {
        parts.push(rest[..pos].to_string());
        // 跳过这个标签本身（到下一个 '>'）
        let after = &rest[pos..];
        match after.find('>') {
            Some(gt) => rest = &after[gt + 1..],
            None => {
                rest = "";
                break;
            }
        }
    }
    if !rest.is_empty() {
        parts.push(rest.to_string());
    }
    parts
}

/// 按 h1..h3 切块。h1/h2/h3 在电子书里就是章节边界。
fn split_on_headings(html: &str) -> Vec<String> {
    let mut marks: Vec<usize> = Vec::new();
    for tag in ["<h1", "<h2", "<h3"] {
        let mut from = 0;
        while let Some(p) = html[from..].find(tag) {
            let abs = from + p;
            // 排除 <h10 这种不存在的标签，也排除 <hr
            let next = html[abs + tag.len()..].chars().next();
            if matches!(next, Some(c) if c == '>' || c == ' ' || c.is_whitespace()) {
                marks.push(abs);
            }
            from = abs + tag.len();
        }
    }
    if marks.len() < 2 {
        return vec![html.to_string()];
    }
    marks.sort_unstable();
    marks.dedup();

    let mut out = Vec::new();
    if marks[0] > 0 {
        let head = html[..marks[0]].trim();
        if !head.is_empty() {
            out.push(head.to_string());
        }
    }
    for (i, &m) in marks.iter().enumerate() {
        let end = marks.get(i + 1).copied().unwrap_or(html.len());
        let seg = html[m..end].trim();
        if !seg.is_empty() {
            out.push(seg.to_string());
        }
    }
    out
}

fn first_heading_text(html: &str) -> Option<String> {
    for tag in ["h1", "h2", "h3", "title"] {
        let open = format!("<{}", tag);
        let close = format!("</{}", tag);
        let Some(s) = html.find(&open) else { continue };
        let Some(gt) = html[s..].find('>') else {
            continue;
        };
        let inner_start = s + gt + 1;
        let Some(e) = html[inner_start..].find(&close) else {
            continue;
        };
        let text = strip_tags(&html[inner_start..inner_start + e]);
        let text = text.trim();
        if !text.is_empty() {
            return Some(truncate_chars(text, 60));
        }
    }
    None
}

// ───────────────────────── HTML 清洗 ─────────────────────────

/// 去掉 MOBI 私有标签与无法解析的引用，留下能直接渲染的 HTML。
///
/// - `<mbp:pagebreak>`：分章已经用过了，留着只会多出一条横线
/// - `<img recindex="...">`：图片在独立记录里，本模块没导出，留着就是裂图
/// - `<script>/<style>`：`dangerouslySetInnerHTML` 会把 `<style>` 插进**当前文档**，
///   一本书写了 `body{background:#000}` 就能把整个应用变黑（EPUB 那边踩过这个坑）
fn sanitize_mobi_html(html: &str) -> String {
    const VOID_DROP: &[&str] = &["img", "image", "mbp:pagebreak", "mbp:pagebreak/"];
    // 成对标签：连内容一起丢。<head> 里是 <title> 和引用，渲染出来只会多一行书名
    const PAIR_DROP: &[&str] = &["script", "style", "head"];
    // 只丢标签、保留内容：正文其实就在 body 里，但 html/body 的标签本身是噪音
    const WRAP_KEEP: &[&str] = &["html", "body"];

    let mut out = String::with_capacity(html.len());
    let mut i = 0usize;

    while i < html.len() {
        let ch = match html[i..].chars().next() {
            Some(c) => c,
            None => break,
        };
        if ch != '<' {
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }

        let Some(gt) = html[i..].find('>') else {
            // 没有闭合的 '<'，剩下的当纯文本
            out.push_str(&html[i..]);
            break;
        };
        let end = i + gt + 1;
        let tag = &html[i..end];
        let name = tag_name(tag);

        if PAIR_DROP.iter().any(|d| name.eq_ignore_ascii_case(d)) {
            // 成对标签：连同内容一起丢掉
            if tag.starts_with("</") {
                i = end;
                continue;
            }
            let close = format!("</{}", name);
            if let Some(p) = html[end..].find(&close).map(|p| end + p) {
                let close_end = html[p..].find('>').map(|g| p + g + 1).unwrap_or(html.len());
                i = close_end;
            } else {
                i = end;
            }
            continue;
        }

        if VOID_DROP.iter().any(|d| name.eq_ignore_ascii_case(d)) {
            i = end;
            continue;
        }

        if WRAP_KEEP.iter().any(|d| name.eq_ignore_ascii_case(d)) {
            i = end;
            continue;
        }

        // <a filepos=...> 是 MOBI 站内跳转，跳到的位置在应用里没有对应，
        // 剥成纯链接文本更合适
        if name.eq_ignore_ascii_case("a") && !tag.starts_with("</") {
            let lower = tag.to_ascii_lowercase();
            if lower.contains("filepos") {
                i = end;
                continue;
            }
        }
        if name.eq_ignore_ascii_case("a") && tag.starts_with("</") {
            i = end;
            continue;
        }

        out.push_str(tag);
        i = end;
    }

    out
}

fn tag_name(tag: &str) -> &str {
    let inner = tag
        .trim_start_matches('<')
        .trim_start_matches('/')
        .trim_start_matches('!');
    let end = inner
        .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
        .unwrap_or(inner.len());
    &inner[..end]
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push('…');
    out
}

// ───────────────────────── 小工具 ─────────────────────────

fn be16(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2)
        .and_then(|s| <[u8; 2]>::try_from(s).ok())
        .map(u16::from_be_bytes)
        .unwrap_or(0)
}

fn be32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .and_then(|s| <[u8; 4]>::try_from(s).ok())
        .map(u32::from_be_bytes)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PalmDOC 的三类编码各验一次：字面量、回溯复制、空格异或
    #[test]
    fn palmdoc_roundtrip_basics() {
        // "abc" 直接是字面量 9..0x7f
        let mut out = Vec::new();
        palmdoc_decompress(b"abc", &mut out);
        assert_eq!(out, b"abc");

        // 0x82 0x20：pair = 0x8220 → dist = (0x8220>>3)&0x7ff = 0x44, len = 3
        // 先放 0x44 个 'x'，再让它回溯复制 3 个
        let mut out = Vec::new();
        palmdoc_decompress(&[b'x'; 0x44], &mut out);
        palmdoc_decompress(&[0x82, 0x20], &mut out);
        assert_eq!(out.len(), 0x44 + 3);
        assert!(out[0x44..].iter().all(|&c| c == b'x'));

        // 0xc1 → 空格 + 0x41 = " A"
        let mut out = Vec::new();
        palmdoc_decompress(&[0xc1], &mut out);
        assert_eq!(out, b" A");
    }

    /// LZ77 的重叠复制：dist=1 时长度 10 的复制相当于把同一个字节重复 10 次。
    ///
    /// pair 的首字节必须 ≥0x80 才会被当成回溯指令，所以内部值
    /// `(dist<<3)|(len-3)` 只占 14 位，`0x80 0x0F` 解出 dist=1 / len=10。
    #[test]
    fn palmdoc_overlapping_copy() {
        let mut out = vec![b'z'];
        palmdoc_decompress(&[0x80, 0x0f], &mut out);
        assert_eq!(out, b"zzzzzzzzzzz");
    }

    /// 1..=8 的「后面 n 个字节是字面量」分支
    #[test]
    fn palmdoc_literal_run() {
        let mut out = Vec::new();
        palmdoc_decompress(&[0x03, b'a', b'b', b'c'], &mut out);
        assert_eq!(out, b"abc");
    }

    #[test]
    fn encoding_prefers_utf8_even_if_declared_cp1252() {
        // 声明 1252 但字节是 UTF-8 的中文，这种情况要按 UTF-8 解
        let s = "第一章 测试";
        assert_eq!(decode_bytes(s.as_bytes(), 1252), s);
    }

    #[test]
    fn encoding_falls_back_to_gb18030() {
        // "中文" 的 GBK 编码：D6 D0 CE C4
        let gbk = [0xd6u8, 0xd0, 0xce, 0xc4];
        assert_eq!(decode_bytes(&gbk, 936), "中文");
    }

    #[test]
    fn sanitize_drops_style_block_and_images() {
        let html = r#"<p>前</p><style>body{background:#000}</style><img recindex="1"/><p>后</p><mbp:pagebreak/>"#;
        let out = sanitize_mobi_html(html);
        assert!(!out.contains("background"));
        assert!(!out.contains("<img"));
        assert!(!out.contains("pagebreak"));
        assert!(out.contains("前") && out.contains("后"));
    }

    #[test]
    fn pagebreak_splits_pages() {
        let html = "A<mbp:pagebreak/>B<mbp:pagebreak/>C";
        let parts = split_on_pagebreak(html);
        assert_eq!(parts, vec!["A", "B", "C"]);
    }

    #[test]
    fn read_bits32_reads_msb_first_and_pads() {
        // 0b1010_1100 ... 取前 8 位应以 0xAC 开头
        let data = [0xacu8, 0x12, 0x34, 0x56, 0x78];
        assert_eq!(read_bits32(&data, 0), 0xac12_3456);
        // 移 4 位后应是 0xc123_4567（低 4 位补 0）
        assert_eq!(read_bits32(&data, 4), 0xc123_4567);
    }
}
