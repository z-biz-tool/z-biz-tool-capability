//! PDF 结构操作：列页、合并、拆分。
//!
//! 全部基于 lopdf 的真实对象模型。pdf_utils.rs 里那种按字节数 `/Type /Page`
//! 的做法在压缩对象流（现代 PDF 的常态）下会直接数错页数，所以这里重新实现，
//! 并把每一页可达的对象整体搬进新文档 —— 合并/拆分后祖先继承属性会失效，
//! 因此搬运前先把 MediaBox/Resources 这类继承值补成页面自己的显式属性。

use lopdf::{Dictionary, Document, Object, ObjectId};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// 前端 PdfTools 拆分表格直接吃这个结构
#[derive(Debug, Clone, Serialize)]
pub struct PdfPageInfo {
    pub page_number: u32,
    pub width: u32,
    pub height: u32,
    pub size_bytes: u64,
}

fn describe(what: &str, err: impl std::fmt::Debug) -> String {
    format!("{}: {:?}", what, err)
}

pub(crate) fn load_doc(path: &Path) -> Result<Document, String> {
    match Document::load(path) {
        Ok(doc) => Ok(doc),
        Err(error) => {
            // 修复不成功就把最初的报错还给用户，不要用二次错误盖掉真因
            let detail = describe("PDF 解析失败", error);
            let bytes = fs::read(path).map_err(|_| detail.clone())?;
            let repaired = match repair_startxref(&bytes) {
                Some(repaired) => repaired,
                None => return Err(detail),
            };
            Document::load_mem(&repaired).map_err(|_| detail)
        }
    }
}

/// 部分生成器把 startxref 和偏移量写在同一行（`startxref 4453432\n%%EOF`），
/// 本机那份 485 页 iBooks 真题就是这样，lopdf 要求严格换行于是整份文件报
/// `Xref(Start)`、页数/文本/合并全部不可用。这里只把那一段分隔空白补成规范
/// 写法再交给 lopdf —— 偏移量前后之外一个内容字节都不动。
fn repair_startxref(bytes: &[u8]) -> Option<Vec<u8>> {
    const KEY: &[u8] = b"startxref";
    const MARK: &[u8] = b"%%EOF";
    // lopdf 自己也只在末尾 512 字节里找这段
    let window = bytes.len().saturating_sub(512);
    let tail = &bytes[window..];
    let pos = tail.windows(KEY.len()).rposition(|w| w == KEY)?;
    let digits_start = skip_ws(tail, pos + KEY.len());
    let mut end = digits_start;
    while end < tail.len() && tail[end].is_ascii_digit() {
        end += 1;
    }
    if end == digits_start {
        return None;
    }
    let gap = skip_ws(tail, end);
    if gap == end || !tail[gap..].starts_with(MARK) {
        return None;
    }
    // 偏移量与 %%EOF 之间没有空白说明这不是排版偏差，别硬凑
    let mut out = Vec::with_capacity(bytes.len() + 2);
    out.extend_from_slice(&bytes[..window + pos]);
    out.extend_from_slice(b"startxref\n");
    out.extend_from_slice(&bytes[window + digits_start..window + end]);
    out.extend_from_slice(b"\n");
    out.extend_from_slice(&bytes[window + gap..]);
    Some(out)
}

fn skip_ws(input: &[u8], mut at: usize) -> usize {
    while at < input.len() && matches!(input[at], b' ' | b'\t' | b'\r' | b'\n' | 0x00 | 0x0C) {
        at += 1;
    }
    at
}

/// 页面上没有 MediaBox/Resources 时，PDF 允许从 Pages 祖先继承。
/// 合并、拆分之后祖先链就不存在了，所以要沿 Parent 链把值找出来。
pub(crate) fn inheritable(doc: &Document, page: ObjectId, key: &[u8]) -> Option<Object> {
    let mut current = page;
    for _ in 0..32 {
        let dict = doc
            .get_object(current)
            .ok()
            .and_then(|o| o.as_dict().ok())?;
        if let Ok(value) = dict.get(key) {
            return Some(value.clone());
        }
        current = dict.get(b"Parent").and_then(Object::as_reference).ok()?;
    }
    None
}

pub(crate) fn number_of(doc: &Document, obj: &Object) -> f64 {
    match obj {
        Object::Integer(i) => *i as f64,
        Object::Real(r) => *r as f64,
        Object::Reference(id) => match doc.get_object(*id) {
            Ok(Object::Integer(i)) => *i as f64,
            Ok(Object::Real(r)) => *r as f64,
            _ => 0.0,
        },
        _ => 0.0,
    }
}

/// 页面尺寸取 MediaBox（缺失时退到 CropBox），坐标可能是间接引用
fn page_size(doc: &Document, page: ObjectId) -> (u32, u32) {
    let raw = inheritable(doc, page, b"MediaBox").or_else(|| inheritable(doc, page, b"CropBox"));
    let array = match raw {
        Some(Object::Array(a)) => a,
        Some(Object::Reference(id)) => match doc.get_object(id) {
            Ok(Object::Array(a)) => a.clone(),
            _ => return (0, 0),
        },
        _ => return (0, 0),
    };
    if array.len() < 4 {
        return (0, 0);
    }
    let width = (number_of(doc, &array[2]) - number_of(doc, &array[0])).abs();
    let height = (number_of(doc, &array[3]) - number_of(doc, &array[1])).abs();
    (width.round() as u32, height.round() as u32)
}

/// 把源文档里 `id` 指向的对象搬进 `out`，返回新 id。
/// `map` 是"源 id → 新 id"，所以多页共享的字体、图片只会搬一次；
/// 先插占位对象再改写内容，页面互相引用（如 /Parent 成环）时才不会无限递归。
fn graft_object(
    src: &Document,
    id: ObjectId,
    out: &mut Document,
    map: &mut BTreeMap<ObjectId, ObjectId>,
    skip: &[&[u8]],
) -> Result<ObjectId, String> {
    if let Some(&existing) = map.get(&id) {
        return Ok(existing);
    }
    let original = src
        .get_object(id)
        .map_err(|e| describe("PDF 对象缺失", e))?
        .clone();
    let new_id = out.add_object(Object::Null);
    map.insert(id, new_id);
    let rewritten = graft_value(src, &original, out, map, skip)?;
    out.set_object(new_id, rewritten);
    Ok(new_id)
}

fn graft_value(
    src: &Document,
    obj: &Object,
    out: &mut Document,
    map: &mut BTreeMap<ObjectId, ObjectId>,
    skip: &[&[u8]],
) -> Result<Object, String> {
    Ok(match obj {
        Object::Reference(target) => Object::Reference(graft_object(src, *target, out, map, &[])?),
        Object::Dictionary(dict) => Object::Dictionary(graft_dict(src, dict, out, map, skip)?),
        Object::Array(items) => {
            let mut copied = Vec::with_capacity(items.len());
            for item in items {
                copied.push(graft_value(src, item, out, map, skip)?);
            }
            Object::Array(copied)
        }
        Object::Stream(stream) => {
            // 内容流原样搬运：过滤方式写在 stream 字典里，字典已一起重写
            let dict = graft_dict(src, &stream.dict, out, map, &[])?;
            let mut copied = stream.clone();
            copied.dict = dict;
            Object::Stream(copied)
        }
        other => other.clone(),
    })
}

fn graft_dict(
    src: &Document,
    dict: &Dictionary,
    out: &mut Document,
    map: &mut BTreeMap<ObjectId, ObjectId>,
    skip: &[&[u8]],
) -> Result<Dictionary, String> {
    let mut copied = Dictionary::new();
    for (key, value) in dict.iter() {
        if skip.contains(&key.as_slice()) {
            continue;
        }
        copied.set(key.as_slice(), graft_value(src, value, out, map, skip)?);
    }
    Ok(copied)
}

/// 搬一页：丢掉 /Parent（新文档要挂到新的 Pages 节点上），
/// 并把从祖先继承来的属性固化成页面自己的。
fn graft_page(
    src: &Document,
    page: ObjectId,
    out: &mut Document,
    map: &mut BTreeMap<ObjectId, ObjectId>,
) -> Result<ObjectId, String> {
    let own = src
        .get_object(page)
        .ok()
        .and_then(|o| o.as_dict().ok())
        .cloned()
        .ok_or_else(|| "页对象不是字典".to_string())?;
    let mut inherited = Vec::new();
    for key in [
        b"MediaBox".as_slice(),
        b"CropBox".as_slice(),
        b"Resources".as_slice(),
        b"Rotate".as_slice(),
    ] {
        if own.get(key).is_err() {
            if let Some(value) = inheritable(src, page, key) {
                inherited.push((key.to_vec(), value));
            }
        }
    }
    let new_id = graft_object(src, page, out, map, &[b"Parent"])?;
    // 先把继承值重写完成，再一次性写回页字典：否则 out 会被同时可变借用两次
    let mut grafted = Vec::with_capacity(inherited.len());
    for (key, value) in inherited {
        grafted.push((key, graft_value(src, &value, out, map, &[])?));
    }
    let dict = out
        .get_object_mut(new_id)
        .and_then(Object::as_dict_mut)
        .map_err(|e| describe("页对象写入失败", e))?;
    for (key, value) in grafted {
        if dict.get(&key).is_err() {
            dict.set(key, value);
        }
    }
    Ok(new_id)
}

/// 把若干来源文档的指定页拼成一个新文档：重建 Pages 节点、Catalog 和 trailer
fn compose(sources: &[(Document, Vec<ObjectId>)]) -> Result<Document, String> {
    let mut out = Document::with_version("1.5");
    let mut kids: Vec<ObjectId> = Vec::new();
    for (src, pages) in sources {
        let mut map: BTreeMap<ObjectId, ObjectId> = BTreeMap::new();
        for page in pages {
            kids.push(graft_page(src, *page, &mut out, &mut map)?);
        }
    }
    let references: Vec<Object> = kids.iter().map(|id| Object::Reference(*id)).collect();
    let mut pages_dict = Dictionary::new();
    pages_dict.set("Type", Object::Name(b"Pages".to_vec()));
    pages_dict.set("Kids", Object::Array(references));
    pages_dict.set("Count", Object::Integer(kids.len() as i64));
    let pages_id = out.add_object(pages_dict);

    for page in &kids {
        let dict = out
            .get_object_mut(*page)
            .and_then(Object::as_dict_mut)
            .map_err(|e| describe("页对象写入失败", e))?;
        dict.set("Parent", Object::Reference(pages_id));
    }

    let mut catalog = Dictionary::new();
    catalog.set("Type", Object::Name(b"Catalog".to_vec()));
    catalog.set("Pages", Object::Reference(pages_id));
    let root_id = out.add_object(catalog);
    out.trailer.set("Root", Object::Reference(root_id));
    Ok(out)
}

fn page_ids(doc: &Document) -> Vec<ObjectId> {
    doc.get_pages().into_values().collect()
}

/// 输出落点：先过 path_guard，再建父目录 —— 反过来会先把敏感目录创建出来
pub(crate) fn save_doc(mut doc: Document, raw_dest: &str) -> Result<(), String> {
    let dest = crate::paths::output(raw_dest)?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建输出目录失败: {}", e))?;
    }
    doc.save(&dest).map_err(|e| describe("PDF 写入失败", e))?;
    Ok(())
}

/// 列出每一页的页码、尺寸与内容流大小，供拆分界面渲染
pub fn get_pdf_pages(path: &str) -> Result<Vec<PdfPageInfo>, String> {
    let file = crate::paths::input(path)?;
    let doc = load_doc(&file)?;
    let pages = doc.get_pages();
    let mut infos = Vec::with_capacity(pages.len());
    for (index, page) in pages.values().enumerate() {
        let (width, height) = page_size(&doc, *page);
        // 单页字节数取解压后的内容流长度；字体图片多为跨页共享，摊到单页上只会是噪声
        let size = doc
            .get_page_content(*page)
            .map(|content| content.len() as u64)
            .unwrap_or_default();
        infos.push(PdfPageInfo {
            page_number: index as u32 + 1,
            width,
            height,
            size_bytes: size,
        });
    }
    Ok(infos)
}

/// 按给定顺序合并多个 PDF，返回合并后的总页数
pub fn merge_pdfs(input_paths: Vec<String>, output_path: String) -> Result<u32, String> {
    if input_paths.len() < 2 {
        return Err("合并至少需要两个 PDF".to_string());
    }
    let mut sources = Vec::with_capacity(input_paths.len());
    let mut total = 0u32;
    for raw in &input_paths {
        let path = crate::paths::input(raw)?;
        let doc = load_doc(&path)?;
        let pages = page_ids(&doc);
        if pages.is_empty() {
            return Err(format!("PDF 没有任何页面: {}", raw));
        }
        total += pages.len() as u32;
        sources.push((doc, pages));
    }
    save_doc(compose(&sources)?, &output_path)?;
    Ok(total)
}

/// 按页码区间拆分，返回生成的文件路径；区间外页码一律拒绝而不是静默裁剪
pub fn split_pdf(
    input_path: String,
    output_dir: String,
    page_ranges: Vec<[u32; 2]>,
) -> Result<Vec<String>, String> {
    let file = crate::paths::input(&input_path)?;
    let doc = load_doc(&file)?;
    let pages = page_ids(&doc);
    let total = pages.len() as u32;
    if total == 0 {
        return Err("PDF 没有任何页面".to_string());
    }
    if page_ranges.is_empty() {
        return Err("没有指定拆分区间".to_string());
    }
    // 目录落点同样要过校验：output_dir 完全可能填成 ~/.ssh
    let dir = crate::paths::output(&output_dir)?;
    fs::create_dir_all(&dir).map_err(|e| format!("创建输出目录失败: {}", e))?;

    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "page".to_string());
    // 序号补零，保证拆出来的文件在访达里按页序排列
    let pad = page_ranges.len().to_string().len().max(2);
    let mut outputs = Vec::with_capacity(page_ranges.len());
    for (index, range) in page_ranges.iter().enumerate() {
        let (from, to) = (range[0], range[1]);
        if from == 0 || to < from || to > total {
            return Err(format!(
                "页码区间 {}-{} 超出范围（本文档共 {} 页）",
                from, to, total
            ));
        }
        let slice = pages[from as usize - 1..to as usize].to_vec();
        let seq = format!("{:0>pad$}", index + 1, pad = pad);
        let name = if from == to {
            format!("{}-{}.pdf", stem, seq)
        } else {
            format!("{}-{}-{}-{}.pdf", stem, seq, from, to)
        };
        let dest = dir.join(name);
        save_doc(compose(&[(doc.clone(), slice)])?, &dest.to_string_lossy())?;
        outputs.push(dest.to_string_lossy().to_string());
    }
    Ok(outputs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 手写一个最小但结构完整的 PDF：MediaBox 故意放在 Pages 祖先上，
    /// 用来验证"继承属性必须固化"这条路径；每页内容流带一个可搜索的 marker。
    fn make_pdf(markers: &[&str]) -> Vec<u8> {
        let n = markers.len();
        // 对象编号：1 Catalog，2 Pages，3 Font，之后每页两个对象（page、content）
        let pages_id: ObjectId = (2, 0);
        let font_id: ObjectId = (3, 0);
        let kids: Vec<String> = markers
            .iter()
            .enumerate()
            .map(|(i, _)| format!("{} 0 R", 4 + i * 2))
            .collect();

        let mut objects: Vec<String> = Vec::new();
        objects.push("<< /Type /Catalog /Pages 2 0 R >>".to_string());
        objects.push(format!(
            "<< /Type /Pages /Kids [{}] /Count {} /MediaBox [0 0 612 792] >>",
            kids.join(" "),
            n
        ));
        objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string());
        for (i, marker) in markers.iter().enumerate() {
            let content_id = 5 + i * 2;
            objects.push(format!(
                "<< /Type /Page /Parent {} 0 R /Resources << /Font << /F1 {} 0 R >> >> /Contents {} 0 R >>",
                pages_id.0,
                font_id.0,
                content_id
            ));
            let stream = format!("BT /F1 24 Tf 72 700 Td ({}) Tj ET", marker);
            objects.push(format!(
                "<< /Length {} >>\nstream\n{}\nendstream",
                stream.len(),
                stream
            ));
        }

        let mut body = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, text) in objects.iter().enumerate() {
            offsets.push(body.len());
            body.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", i + 1, text).as_bytes());
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

    fn write_fixture(dir: &Path, name: &str, markers: &[&str]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, make_pdf(markers)).unwrap();
        path
    }

    fn text_of(path: &Path) -> String {
        // 内容流是未压缩的，直接按字节搜 marker 就能证明页面可达
        String::from_utf8_lossy(&fs::read(path).unwrap()).replace('\n', " ")
    }

    /// 统计对象表里的 Font 字典个数。
    /// 不能拿 `text_of` 去数 "/BaseFont /Helvetica"：lopdf 写出的字典是紧凑形式
    /// `/BaseFont/Helvetica`，按带空格的写法匹配会恒为 0，断言等于没测。
    fn font_objects(doc: &Document) -> Vec<ObjectId> {
        doc.objects
            .iter()
            .filter(|(_, obj)| match obj {
                Object::Dictionary(d) => d
                    .get(b"Type")
                    .and_then(|t| t.as_name())
                    .map(|name| name == &b"Font"[..])
                    .unwrap_or(false),
                _ => false,
            })
            .map(|(id, _)| *id)
            .collect()
    }

    fn page_font_ref(doc: &Document, page: ObjectId) -> ObjectId {
        let d = doc.get_object(page).unwrap().as_dict().unwrap();
        let res = d.get(b"Resources").and_then(Object::as_dict).unwrap();
        let fonts = res.get(b"Font").and_then(Object::as_dict).unwrap();
        fonts.get(b"F1").and_then(Object::as_reference).unwrap()
    }

    #[test]
    fn pages_report_inherited_mediabox() {
        let dir = crate::test_support::TempDir::new("pdf-pages");
        let file = write_fixture(&dir, "a.pdf", &["AAA1", "AAA2"]);
        let pages = get_pdf_pages(file.to_str().unwrap()).unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].page_number, 1);
        // MediaBox 只写在 Pages 祖先上，能报出 612x792 才说明继承链走通了
        assert_eq!((pages[0].width, pages[0].height), (612, 792));
        assert_eq!((pages[1].width, pages[1].height), (612, 792));
        assert!(pages[0].size_bytes > 10, "内容流长度异常: {:?}", pages[0]);
    }
    #[test]
    fn merge_keeps_every_page_content_and_font() {
        let dir = crate::test_support::TempDir::new("pdf-merge");
        let a = write_fixture(&dir, "a.pdf", &["MARK_A1"]);
        let b = write_fixture(&dir, "b.pdf", &["MARK_B1", "MARK_B2"]);
        let out = dir.join("merged.pdf");

        let count = merge_pdfs(
            vec![
                a.to_string_lossy().to_string(),
                b.to_string_lossy().to_string(),
            ],
            out.to_string_lossy().to_string(),
        )
        .unwrap();
        assert_eq!(count, 3);

        let doc = load_doc(&out).expect("合并结果必须能被重新解析");
        assert_eq!(doc.get_pages().len(), 3);
        let body = text_of(&out);
        for marker in ["MARK_A1", "MARK_B1", "MARK_B2"] {
            assert!(body.contains(marker), "缺少页面内容 {}", marker);
        }
        // 字体是间接对象：同一来源内多页共享一份（B 的两页指向同一个 id），
        // 但不同来源各留一份 —— 跨文档无法证明两个字体对象等价
        let doc2 = load_doc(&out).unwrap();
        let mut fonts = font_objects(&doc2);
        fonts.sort();
        assert_eq!(fonts.len(), 2, "字体不该按页复制");
        let ids: Vec<ObjectId> = doc2.get_pages().into_values().collect();
        let font_of = |page: ObjectId| page_font_ref(&doc2, page);
        assert_eq!(
            font_of(ids[1]),
            font_of(ids[2]),
            "同一来源的两页必须共用一个字体对象"
        );
        assert_ne!(font_of(ids[0]), font_of(ids[1]));
        assert!(fonts.contains(&font_of(ids[0])) && fonts.contains(&font_of(ids[1])));
    }

    #[test]
    fn merged_pages_keep_their_own_size_and_parent() {
        let dir = crate::test_support::TempDir::new("pdf-merge-tree");
        let a = write_fixture(&dir, "a.pdf", &["MARK_A1"]);
        let b = write_fixture(&dir, "b.pdf", &["MARK_B1"]);
        let out = dir.join("merged.pdf");
        merge_pdfs(
            vec![
                a.to_string_lossy().to_string(),
                b.to_string_lossy().to_string(),
            ],
            out.to_string_lossy().to_string(),
        )
        .unwrap();

        let pages = get_pdf_pages(out.to_str().unwrap()).unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!((pages[0].width, pages[1].height), (612, 792));

        let doc = load_doc(&out).unwrap();
        let ids: Vec<ObjectId> = doc.get_pages().into_values().collect();
        let first = doc.get_object(ids[0]).unwrap().as_dict().unwrap();
        let parent = doc
            .get_object(first.get(b"Parent").and_then(Object::as_reference).unwrap())
            .unwrap()
            .as_dict()
            .unwrap();
        assert_eq!(parent.get(b"Type").unwrap().as_name().unwrap(), b"Pages");
        assert_eq!(parent.get(b"Count").unwrap().as_i64().unwrap(), 2);
        // 第二页也必须挂在同一个新 Pages 节点下
        let second = doc.get_object(ids[1]).unwrap().as_dict().unwrap();
        assert_eq!(
            second
                .get(b"Parent")
                .and_then(Object::as_reference)
                .unwrap(),
            first.get(b"Parent").and_then(Object::as_reference).unwrap()
        );
    }

    #[test]
    fn merge_requires_two_inputs() {
        let dir = crate::test_support::TempDir::new("pdf-merge-one");
        let a = write_fixture(&dir, "a.pdf", &["MARK_A1"]);
        let err = merge_pdfs(
            vec![a.to_string_lossy().to_string()],
            dir.join("x.pdf").to_string_lossy().to_string(),
        )
        .expect_err("单文件不该叫合并");
        assert!(err.contains("至少"), "{}", err);
    }
    #[test]
    fn split_writes_one_file_per_range() {
        let dir = crate::test_support::TempDir::new("pdf-split");
        let a = write_fixture(&dir, "src.pdf", &["P1", "P2", "P3"]);
        let out_dir = dir.join("parts");

        let files = split_pdf(
            a.to_string_lossy().to_string(),
            out_dir.to_string_lossy().to_string(),
            vec![[1, 2], [3, 3]],
        )
        .unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(get_pdf_pages(&files[0]).unwrap().len(), 2);
        assert_eq!(get_pdf_pages(&files[1]).unwrap().len(), 1);
        let body0 = text_of(Path::new(&files[0]));
        assert!(
            body0.contains("(P1)") && body0.contains("(P2)"),
            "{}",
            body0
        );
        assert!(!body0.contains("(P3)"), "区间外的页不该出现: {}", body0);
        let body1 = text_of(Path::new(&files[1]));
        assert!(
            body1.contains("(P3)") && !body1.contains("(P1)"),
            "{}",
            body1
        );
    }

    #[test]
    fn split_rejects_out_of_range_pages() {
        let dir = crate::test_support::TempDir::new("pdf-split-range");
        let a = write_fixture(&dir, "src.pdf", &["P1", "P2"]);
        let err = split_pdf(
            a.to_string_lossy().to_string(),
            dir.join("parts").to_string_lossy().to_string(),
            vec![[1, 5]],
        )
        .expect_err("越界区间必须报错");
        assert!(err.contains("超出范围"), "{}", err);
    }
    /// 合并产物要能被自己的读取路径再次当作输入解析（否则用户第二次操作就会坏）。
    #[test]
    fn merged_output_can_be_merged_again() {
        let dir = crate::test_support::TempDir::new("pdf-merge-twice");
        let a = write_fixture(&dir, "a.pdf", &["MARK_A1"]);
        let b = write_fixture(&dir, "b.pdf", &["MARK_B1"]);
        let c = write_fixture(&dir, "c.pdf", &["MARK_C1", "MARK_C2"]);

        let first = dir.join("step1.pdf");
        merge_pdfs(
            vec![
                a.to_string_lossy().to_string(),
                b.to_string_lossy().to_string(),
            ],
            first.to_string_lossy().to_string(),
        )
        .unwrap();

        let second = dir.join("step2.pdf");
        let count = merge_pdfs(
            vec![
                first.to_string_lossy().to_string(),
                c.to_string_lossy().to_string(),
            ],
            second.to_string_lossy().to_string(),
        )
        .unwrap();
        assert_eq!(count, 4);

        let body = text_of(&second);
        for marker in ["MARK_A1", "MARK_B1", "MARK_C1", "MARK_C2"] {
            assert!(body.contains(marker), "二次合并丢了内容 {}", marker);
        }
        let pages = get_pdf_pages(second.to_str().unwrap()).unwrap();
        assert_eq!(pages.len(), 4);
        // step1 本身已经带了 2 份字体，再加 c 的 1 份 = 3，
        // 说明 graft 的去重键是"来源对象"而不是"全局对象号"
        let doc = load_doc(&second).unwrap();
        assert_eq!(font_objects(&doc).len(), 3);
        let ids: Vec<ObjectId> = doc.get_pages().into_values().collect();
        assert_eq!(page_font_ref(&doc, ids[2]), page_font_ref(&doc, ids[3]));
        assert_ne!(page_font_ref(&doc, ids[0]), page_font_ref(&doc, ids[3]));
    }

    /// PdfTools.tsx 直接按下划线字段名取值，改名会在运行时才静默变成 undefined。
    #[test]
    fn page_info_keys_match_the_frontend_contract() {
        let dir = crate::test_support::TempDir::new("pdf-contract");
        let file = write_fixture(&dir, "a.pdf", &["P1"]);
        let pages = get_pdf_pages(file.to_str().unwrap()).unwrap();
        let obj = serde_json::to_value(&pages[0]).unwrap();
        for key in ["page_number", "width", "height", "size_bytes"] {
            assert!(
                obj.get(key).is_some(),
                "前端字段 {} 缺失，实际结构 {:?}",
                key,
                obj
            );
        }
        assert_eq!(obj.as_object().unwrap().len(), 4);
    }

    /// 把 lopdf 规范写好的 trailer 改成"startxref 和偏移量同一行"。
    fn same_line_trailer(canonical: &[u8]) -> Vec<u8> {
        let mut out = canonical.to_vec();
        let pos = out
            .windows(b"startxref".len())
            .rposition(|w| w == b"startxref")
            .expect("fixture 里必须有 startxref");
        assert_eq!(out[pos + 9], b'\n', "lopdf 写出的分隔符应该是换行");
        out[pos + 9] = b' ';
        out
    }

    /// 真实案例：一份 485 页的 iBooks 真题 PDF 就是这么写的，lopdf 报 Xref(Start)，
    /// 之前这个文件的页数、文本、合并、拆分全部不可用。
    #[test]
    fn trailer_with_startxref_on_one_line_is_recovered() {
        let dir = crate::test_support::TempDir::new("pdf-xref");
        let canonical = make_pdf(&["OK1", "OK2"]);
        let deviating = same_line_trailer(&canonical);
        // 注入必须先生效，否则这条测试什么都没验；规范写法作对照组
        assert!(Document::load_mem(&canonical).is_ok());
        assert!(
            Document::load_mem(&deviating).is_err(),
            "同排版式本该让 lopdf 读不出来"
        );

        let file = dir.join("one-line.pdf");
        fs::write(&file, &deviating).unwrap();
        let doc = load_doc(&file).unwrap();
        assert_eq!(doc.get_pages().len(), 2);
        let pages = get_pdf_pages(file.to_str().unwrap()).unwrap();
        assert_eq!(
            (pages[0].page_number, pages[1].page_number),
            (1, 2),
            "修复后列页必须照常可用"
        );
        let merged = merge_pdfs(
            vec![
                file.to_str().unwrap().to_string(),
                file.to_str().unwrap().to_string(),
            ],
            dir.join("out.pdf").to_str().unwrap().to_string(),
        )
        .unwrap();
        assert_eq!(merged, 4, "修复过的文件还要能进合并流程");
    }

    #[test]
    fn repair_rewrites_only_the_whitespace_between_tokens() {
        let canonical = make_pdf(&["ONLY"]);
        assert_eq!(
            repair_startxref(&canonical).as_deref(),
            Some(canonical.as_slice()),
            "规范写法应当原样返回"
        );
        let deviating = same_line_trailer(&canonical);
        let repaired = repair_startxref(&deviating).unwrap();
        assert_eq!(repaired.len(), deviating.len());
        let diff: Vec<usize> = repaired
            .iter()
            .zip(deviating.iter())
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(i, _)| i)
            .collect();
        let pos = deviating
            .windows(b"startxref".len())
            .rposition(|w| w == b"startxref")
            .unwrap();
        assert_eq!(diff, vec![pos + 9], "只允许动 startxref 后那个分隔符");
        assert_eq!(&repaired[pos..pos + 9], b"startxref");
        assert!(Document::load_mem(&repaired).is_ok());
    }

    #[test]
    fn repair_refuses_buffers_that_are_not_a_whitespace_deviation() {
        assert!(repair_startxref(b"%PDF-1.4\nno trailer here\n").is_none());
        assert!(
            repair_startxref(b"startxref 42").is_none(),
            "缺 %%EOF 不算排版偏差"
        );
        assert!(repair_startxref(b"startxref 12\n34%%EOF\n").is_none());
        assert!(
            repair_startxref(b"startxref\n%%EOF\n").is_none(),
            "偏移量都丢了就别猜"
        );
    }

    #[test]
    fn load_doc_keeps_the_original_error_when_nothing_can_be_repaired() {
        let dir = crate::test_support::TempDir::new("pdf-junk");
        let file = dir.join("junk.pdf");
        fs::write(&file, b"%PDF-1.4\nnot a pdf at all\n").unwrap();
        let err = load_doc(&file).unwrap_err();
        assert!(err.contains("PDF 解析失败"), "{}", err);
    }
}
