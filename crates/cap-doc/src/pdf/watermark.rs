//! 给 PDF 每一页叠一层文字水印。
//!
//! 不动原有内容流：PDF 允许 /Contents 是一串流按顺序拼接（后者画在上层），
//! 所以水印单独成一个流追加在末尾 —— 原页面内容是什么、有没有被压缩都不用碰，
//! 出错也不会把正文搞花。字体走非嵌入方案：纯拉丁用基 14 的 Helvetica，
//! 含中日韩字符用 Adobe 预置的 STSong-Light + UniGB-UCS2-H（字符串按 UCS-2 BE 编码），
//! 这两类阅读器都自带替代字形，不必往文件里塞几 MB 字体。

use lopdf::{Dictionary, Document, Object, ObjectId, Stream, StringFormat};

/// 45° 斜排：旋转矩阵就是 [cos sin -cos sin 0 0]
const DIAGONAL: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// 基 14 字体只覆盖 WinAnsi，超出这个范围必须换 CJK 字体，否则中文会变成空白
fn needs_cjk(text: &str) -> bool {
    text.chars().any(|character| character as u32 >= 0x100)
}

/// 十六进制串：省掉字面串里括号/反斜杠的转义坑，非 ASCII 也照样精确落字节
fn hex_string(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2 + 2);
    out.push('<');
    for byte in bytes {
        out.push_str(&format!("{:02X}", byte));
    }
    out.push('>');
    out
}

fn encoded_text(text: &str, cjk: bool) -> String {
    if cjk {
        // UniGB-UCS2-H 的输入就是 UCS-2，代理对由 encode_utf16 负责
        let mut bytes = Vec::with_capacity(text.len() * 2);
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
        hex_string(&bytes)
    } else {
        hex_string(&text.chars().map(|c| c as u8).collect::<Vec<_>>())
    }
}

/// 整串水印占多少个 em —— 居中与"放不放得下"都由它决定。
///
/// 走 CJK 字体时这是确定的：CIDFont 只给了 /DW 1000、没有 /W，
/// 阅读器就把拉丁和空格也按全角排（实测 MuPDF 每个字符前进都是 1.0 em），
/// 所以按字数直接算，再留 15% 给替换字体。
/// 纯拉丁走基 14 Helvetica，那才是半角比例字体。
fn text_em(text: &str, cjk: bool) -> f32 {
    if cjk {
        // 一个 UCS-2 编码单元落一个字宽，代理对也占两格，与渲染一致
        text.encode_utf16().count() as f32 * 1.15
    } else {
        text.chars().count() as f32 * 0.55 * 1.25
    }
    .max(0.5)
}

fn name(value: &str) -> Object {
    Object::Name(value.as_bytes().to_vec())
}

fn literal(value: &str) -> Object {
    Object::String(value.as_bytes().to_vec(), StringFormat::Literal)
}

/// 建水印字体。CJK 那一路是 Type0 + 后代 CIDFont + FontDescriptor 三个对象。
/// 带 text 是为了生成 /ToUnicode —— 没有它，阅读器能画出水印却复制不到字。
fn add_font(doc: &mut Document, cjk: bool, text: &str) -> ObjectId {
    if !cjk {
        let mut font = Dictionary::new();
        font.set("Type", name("Font"));
        font.set("Subtype", name("Type1"));
        font.set("BaseFont", name("Helvetica"));
        font.set("Encoding", name("WinAnsiEncoding"));
        font.set(
            "ToUnicode",
            Object::Reference(add_to_unicode(doc, text, false)),
        );
        return doc.add_object(Object::Dictionary(font));
    }

    let mut system = Dictionary::new();
    system.set("Registry", literal("Adobe"));
    system.set("Ordering", literal("GB1"));
    system.set("Supplement", Object::Integer(5));
    let mut descendant = Dictionary::new();
    descendant.set("Type", name("Font"));
    descendant.set("Subtype", name("CIDFontType0"));
    descendant.set("BaseFont", name("STSong-Light"));
    descendant.set("CIDSystemInfo", Object::Dictionary(system));
    // 缺省字宽 1000/1000 em，即全角
    descendant.set("DW", Object::Integer(1000));
    // /FontDescriptor 在 CIDFont 字典里是必填项：缺了它，MuPDF 一类的阅读器
    // 会直接报 "missing font descriptor" 并放弃替换字形 —— 中文水印整条看不见
    let descriptor = doc.add_object(Object::Dictionary({
        let mut dict = Dictionary::new();
        dict.set("Type", name("FontDescriptor"));
        dict.set("FontName", name("STSong-Light"));
        // Flags 4 = Symbolic：中日韩字形不按拉丁的宽度和基线来量
        dict.set("Flags", Object::Integer(4));
        dict.set(
            "FontBBox",
            Object::Array(vec![
                Object::Integer(-34),
                Object::Integer(-226),
                Object::Integer(1007),
                Object::Integer(906),
            ]),
        );
        dict.set("ItalicAngle", Object::Integer(0));
        dict.set("Ascent", Object::Integer(880));
        dict.set("Descent", Object::Integer(-120));
        dict.set("CapHeight", Object::Integer(731));
        dict.set("StemV", Object::Integer(90));
        // 不嵌字体：/FontFile3 缺席，由阅读器用 Adobe-GB1 的预置字形替换
        dict
    }));
    descendant.set("FontDescriptor", Object::Reference(descriptor));
    let descendant = doc.add_object(Object::Dictionary(descendant));

    let mut font = Dictionary::new();
    font.set("Type", name("Font"));
    font.set("Subtype", name("Type0"));
    font.set("BaseFont", name("STSong-Light"));
    font.set("Encoding", name("UniGB-UCS2-H"));
    font.set(
        "DescendantFonts",
        Object::Array(vec![Object::Reference(descendant)]),
    );
    font.set(
        "ToUnicode",
        Object::Reference(add_to_unicode(doc, text, true)),
    );
    doc.add_object(Object::Dictionary(font))
}

/// /ToUnicode：水印字节是我们自己落的（UCS-2 BE 或 WinAnsi 单字节），
/// 所以码位到 Unicode 的映射就是恒等表，不必查任何外部 CMap。
fn add_to_unicode(doc: &mut Document, text: &str, cjk: bool) -> ObjectId {
    let width = if cjk { 4 } else { 2 };
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    if cjk {
        for unit in text.encode_utf16() {
            let code = format!("{:04X}", unit);
            if seen.insert(code.clone()) {
                pairs.push((code.clone(), code));
            }
        }
    } else {
        for character in text.chars() {
            let code = format!("{:02X}", character as u32 & 0xFF);
            if seen.insert(code.clone()) {
                pairs.push((code, format!("{:04X}", character as u32)));
            }
        }
    }

    let mut body = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
         /CMapName /Wm-UCS2 def\n/CMapType 2 def\n1 begincodespacerange\n",
    );
    if cjk {
        body.push_str("<0000> <FFFF>\n");
    } else {
        body.push_str("<00> <FF>\n");
    }
    body.push_str("endcodespacerange\n");
    // 规范限定一个 bfchar 块最多 100 条
    for chunk in pairs.chunks(100) {
        body.push_str(&format!("{} beginbfchar\n", chunk.len()));
        for (code, unicode) in chunk {
            body.push_str(&format!(
                "<{:>0width$}> <{}>\n",
                code,
                unicode,
                width = width
            ));
        }
        body.push_str("endbfchar\n");
    }
    body.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");

    doc.add_object(Object::Stream(Stream::new(
        Dictionary::new(),
        body.into_bytes(),
    )))
}

fn add_alpha_state(doc: &mut Document, opacity: f32) -> ObjectId {
    let mut state = Dictionary::new();
    state.set("Type", name("ExtGState"));
    // ca 管填充（文字走填充），CA 管描边，两个都给才不会只半透明一半
    state.set("ca", Object::Real(opacity));
    state.set("CA", Object::Real(opacity));
    doc.add_object(Object::Dictionary(state))
}

/// 找到该页能用的 Resources 对象：页面自带的优先，否则顺着 Parent 向上找祖先的，
/// 都没有才新建。返回引用号，新建的字体与透明状态都挂到它身上。
fn ensure_resources(doc: &mut Document, page: ObjectId) -> Result<ObjectId, String> {
    let mut current = page;
    for _ in 0..32 {
        let object = match doc.get_object(current) {
            Ok(object) => object,
            Err(_) => return Err("页树引用无效".to_string()),
        };
        let declared = object
            .as_dict()
            .ok()
            .and_then(|dict| dict.get(b"Resources").ok().cloned());
        if let Some(value) = declared {
            return match value {
                Object::Reference(id) => Ok(id),
                Object::Dictionary(dict) => {
                    // 内联字典提升成独立对象，后面的写入才有个稳定的引用号可用
                    let id = doc.add_object(Object::Dictionary(dict));
                    if let Ok(object) = doc.get_object_mut(current) {
                        if let Ok(dict) = object.as_dict_mut() {
                            dict.set("Resources", Object::Reference(id));
                        }
                    }
                    Ok(id)
                }
                _ => Err("Resources 类型异常".to_string()),
            };
        }
        let parent = object
            .as_dict()
            .ok()
            .and_then(|dict| dict.get(b"Parent").ok())
            .and_then(|value| value.as_reference().ok());
        match parent {
            Some(id) => current = id,
            None => break,
        }
    }
    // 整棵树上都没有 Resources：给这一页新建一个
    let id = doc.add_object(Object::Dictionary(Dictionary::new()));
    if let Ok(object) = doc.get_object_mut(page) {
        if let Ok(dict) = object.as_dict_mut() {
            dict.set("Resources", Object::Reference(id));
        }
    }
    Ok(id)
}

/// 取出 Resources 下某个子字典（/Font、/ExtGState 都可能是引用或内联）
fn sub_dict(doc: &Document, owner: ObjectId, key: &[u8]) -> Dictionary {
    let Some(value) = doc
        .get_object(owner)
        .ok()
        .and_then(|object| object.as_dict().ok())
        .and_then(|dict| dict.get(key).ok())
        .cloned()
    else {
        return Dictionary::new();
    };
    match value {
        Object::Dictionary(dict) => dict,
        Object::Reference(id) => doc
            .get_object(id)
            .ok()
            .and_then(|object| object.as_dict().ok())
            .cloned()
            .unwrap_or_default(),
        _ => Dictionary::new(),
    }
}

/// 往 Resources 里挂 /Fwm（字体）与 /Gwm（透明状态）。
/// 多页共用祖先 Resources 时这一步是幂等的：同名同引用，合一次就够了。
fn attach_resources(
    doc: &mut Document,
    page: ObjectId,
    font: ObjectId,
    state: ObjectId,
) -> Result<(), String> {
    let resources = ensure_resources(doc, page)?;
    // 先只读地把现有子字典抄出来，再整体写回：
    // 持有 get_object_mut 的借用同时去读别的对象会直接编译不过
    let mut fonts = sub_dict(doc, resources, b"Font");
    fonts.set(b"Fwm".to_vec(), Object::Reference(font));
    let mut states = sub_dict(doc, resources, b"ExtGState");
    states.set(b"Gwm".to_vec(), Object::Reference(state));

    let dict = doc
        .get_object_mut(resources)
        .map_err(|_| "Resources 对象无法写入".to_string())?
        .as_dict_mut()
        .map_err(|_| "Resources 不是字典".to_string())?;
    dict.set("Font", Object::Dictionary(fonts));
    dict.set("ExtGState", Object::Dictionary(states));
    Ok(())
}

/// 页面可画区域。原点必须一起带上：MediaBox 未必从 (0,0) 起算，
/// 带出血的印刷件常见 `[36 36 559 777]`，只按宽高居中的话整条水印会偏出页面。
struct PageBox {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

fn page_box(doc: &Document, page: ObjectId) -> PageBox {
    // 显示与裁切都按 CropBox 走，缺省才回落到 MediaBox
    let raw = crate::pdf::ops::inheritable(doc, page, b"CropBox")
        .or_else(|| crate::pdf::ops::inheritable(doc, page, b"MediaBox"));
    let array = match raw {
        Some(Object::Array(array)) => array,
        Some(Object::Reference(id)) => match doc.get_object(id) {
            Ok(Object::Array(array)) => array.clone(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    if array.len() < 4 {
        return PageBox {
            x: 0.0,
            y: 0.0,
            width: 595.0,
            height: 842.0,
        };
    }
    let value = |index: usize| crate::pdf::ops::number_of(doc, &array[index]) as f32;
    let (x0, y0, x1, y1) = (value(0), value(1), value(2), value(3));
    PageBox {
        x: x0.min(x1),
        y: y0.min(y1),
        width: (x1 - x0).abs(),
        height: (y1 - y0).abs(),
    }
}

fn overlay_ops(box_: &PageBox, text: &str, cjk: bool) -> Vec<u8> {
    // 沿页面自己的对角线排：45° 在竖版 A4 上会被左右两边先裁掉，
    // 长水印的尾部字形直接消失（实测 "CONFIDENTIAL" 只剩 "CONFIDE"）
    let diagonal = {
        let value = box_.width.hypot(box_.height);
        // 退化页（零宽零高的 MediaBox）按 45° 走，别把旋转矩阵算成 0
        if value > 1.0 {
            value
        } else {
            std::f32::consts::SQRT_2
        }
    };
    let (cos, sin) = if box_.width + box_.height > 0.0 {
        (box_.width / diagonal, box_.height / diagonal)
    } else {
        (DIAGONAL, DIAGONAL)
    };

    // 字号是估出来的，而阅读器多半用替换字形画非嵌入字体：
    // 实测 Adobe-GB1 替换字体的实际行宽是估算值的 1.12 倍，
    // 所以估算宽度再乘 1.25 才敢当"占多宽"用，否则尾字会被裁到页面外
    let em = text_em(text, cjk);
    let preferred = (box_.width.min(box_.height) / 10.0).clamp(12.0, 72.0);
    // 放不下时只能缩字号 —— 小一点总比缺几个字强
    let size = preferred.min(diagonal * 0.85 / em).clamp(4.0, 72.0);
    let half = em * size / 2.0;
    format!(
        "\nq\n\
         1 0 0 1 {:.1} {:.1} cm\n\
         {:.4} {:.4} {:.4} {:.4} 0 0 cm\n\
         BT\n\
         /Fwm {:.2} Tf\n\
         0.45 0.45 0.45 rg\n\
         /Gwm gs\n\
         -{:.2} 0 Td\n\
         {} Tj\n\
         ET\n\
         Q\n",
        box_.x + box_.width / 2.0,
        box_.y + box_.height / 2.0,
        cos,
        sin,
        -sin,
        cos,
        size,
        half,
        encoded_text(text, cjk)
    )
    .into_bytes()
}

/// 把水印流追加到页面内容末尾：原来单个流会变成 [原流, 水印流]
fn append_content(doc: &mut Document, page: ObjectId, ops: Vec<u8>) -> Result<(), String> {
    let stream = doc.add_object(Object::Stream(Stream::new(Dictionary::new(), ops)));
    let existing = doc
        .get_object(page)
        .ok()
        .and_then(|object| object.as_dict().ok())
        .and_then(|dict| dict.get(b"Contents").ok().cloned());
    let contents = match existing {
        None | Some(Object::Null) => Object::Reference(stream),
        Some(value @ Object::Reference(_)) => Object::Array(vec![value, Object::Reference(stream)]),
        Some(Object::Array(mut items)) => {
            items.push(Object::Reference(stream));
            Object::Array(items)
        }
        Some(_) => return Err("Contents 类型无法识别".to_string()),
    };
    let dict = doc
        .get_object_mut(page)
        .map_err(|_| "页面对象无法写入".to_string())?
        .as_dict_mut()
        .map_err(|_| "页面不是字典".to_string())?;
    dict.set("Contents", contents);
    Ok(())
}
pub fn watermark_pdf_blocking(
    input_path: String,
    output_path: String,
    text: String,
    opacity: f64,
) -> Result<u32, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("水印文字不能为空".to_string());
    }
    // 0 不透明度等于没画，与其悄悄输出一个"成功但看不见"的文件，不如夹到看得见的最小值
    let opacity = (opacity as f32).clamp(0.02, 1.0);

    let file = crate::paths::input(&input_path)?;
    let mut doc = crate::pdf::ops::load_doc(&file)?;
    let pages: Vec<ObjectId> = doc.get_pages().values().copied().collect();
    if pages.is_empty() {
        return Err("PDF 没有任何页面".to_string());
    }
    let cjk = needs_cjk(text);
    let font = add_font(&mut doc, cjk, text);
    let state = add_alpha_state(&mut doc, opacity);

    let total = pages.len();
    for page in pages {
        let bounds = page_box(&doc, page);
        let ops = overlay_ops(&bounds, text, cjk);
        attach_resources(&mut doc, page, font, state)?;
        append_content(&mut doc, page, ops)?;
    }
    crate::pdf::ops::save_doc(doc, &output_path)?;
    Ok(total as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_cjk_only_triggers_above_latin_one() {
        assert!(!needs_cjk("CONFIDENTIAL 2026 ©"));
        assert!(needs_cjk("机密 2026"));
        assert!(needs_cjk("café 中文"));
    }

    #[test]
    fn cjk_text_is_encoded_as_ucs2_big_endian() {
        assert_eq!(encoded_text("中A", true), "<4E2D0041>");
        assert_eq!(encoded_text("Ab", false), "<4162>");
    }

    #[test]
    fn hex_string_avoids_literal_escaping_entirely() {
        // 字面串里未转义的 ( ) \ 会直接破坏 PDF 语法，十六进制串没有这个问题
        assert_eq!(hex_string(b"()\\\n"), "<28295C0A>");
        assert!(encoded_text("100% (see \\ notes)", false).starts_with('<'));
    }

    #[test]
    fn empty_text_is_refused() {
        let err = watermark_pdf_blocking(
            "in.pdf".to_string(),
            "out.pdf".to_string(),
            "   ".to_string(),
            0.3,
        )
        .unwrap_err();
        assert!(err.contains("不能为空"), "{}", err);
    }

    /// 用 lopdf 造一份两页文档：第一页自带 Resources（且已有一个 ExtGState 条目），
    /// 第二页靠祖先继承，两条路径的 /Fwm 挂载都要走到
    fn two_page_fixture(dir: &std::path::Path, media: [i64; 4]) -> std::path::PathBuf {
        let mut doc = Document::new();
        let helvetica = doc.add_object(Object::Dictionary({
            let mut dict = Dictionary::new();
            dict.set("Type", name("Font"));
            dict.set("Subtype", name("Type1"));
            dict.set("BaseFont", name("Helvetica"));
            dict
        }));
        let existing_state = doc.add_object(Object::Dictionary({
            let mut dict = Dictionary::new();
            dict.set("Type", name("ExtGState"));
            dict.set("ca", Object::Real(1.0));
            dict
        }));
        let own_resources = doc.add_object(Object::Dictionary({
            let mut fonts = Dictionary::new();
            fonts.set(b"F1".to_vec(), Object::Reference(helvetica));
            let mut states = Dictionary::new();
            states.set(b"G0".to_vec(), Object::Reference(existing_state));
            let mut dict = Dictionary::new();
            dict.set("Font", Object::Dictionary(fonts));
            dict.set("ExtGState", Object::Dictionary(states));
            dict
        }));
        let first = doc.add_object(Object::Stream(Stream::new(
            Dictionary::new(),
            b"BT /F1 24 Tf 60 720 Td (FirstPage) Tj ET".to_vec(),
        )));
        let second = doc.add_object(Object::Stream(Stream::new(
            Dictionary::new(),
            b"BT /F1 24 Tf 60 720 Td (SecondPage) Tj ET".to_vec(),
        )));
        let page_one = doc.add_object(Object::Dictionary({
            let mut dict = Dictionary::new();
            dict.set("Type", name("Page"));
            dict.set(
                "MediaBox",
                Object::Array(media.iter().map(|v| Object::Integer(*v)).collect()),
            );
            dict.set("Resources", Object::Reference(own_resources));
            dict.set("Contents", Object::Reference(first));
            dict
        }));
        let page_two = doc.add_object(Object::Dictionary({
            let mut dict = Dictionary::new();
            dict.set("Type", name("Page"));
            dict.set("Contents", Object::Reference(second));
            dict
        }));
        let pages = doc.add_object(Object::Dictionary({
            let mut dict = Dictionary::new();
            dict.set("Type", name("Pages"));
            dict.set(
                "Kids",
                Object::Array(vec![
                    Object::Reference(page_one),
                    Object::Reference(page_two),
                ]),
            );
            dict.set("Count", Object::Integer(2));
            // 只有继承这一页才有 Font 资源
            let mut fonts = Dictionary::new();
            fonts.set(b"F1".to_vec(), Object::Reference(helvetica));
            let mut resources = Dictionary::new();
            resources.set("Font", Object::Dictionary(fonts));
            dict.set("Resources", Object::Dictionary(resources));
            dict
        }));
        if let Ok(Object::Dictionary(dict)) = doc.get_object_mut(page_two) {
            dict.set("Parent", Object::Reference(pages));
        }
        if let Ok(Object::Dictionary(dict)) = doc.get_object_mut(page_one) {
            dict.set("Parent", Object::Reference(pages));
        }
        let catalog = doc.add_object(Object::Dictionary({
            let mut dict = Dictionary::new();
            dict.set("Type", name("Catalog"));
            dict.set("Pages", Object::Reference(pages));
            dict
        }));
        doc.trailer.set("Root", Object::Reference(catalog));
        let path = dir.join("two-page.pdf");
        doc.save(&path).unwrap();
        path
    }

    /// 取页面追加的那个内容流（/Contents 数组的最后一项）
    fn appended_ops(doc: &Document, page: ObjectId) -> Vec<u8> {
        let contents = doc
            .get_object(page)
            .ok()
            .and_then(|object| object.as_dict().ok())
            .and_then(|dict| dict.get(b"Contents").ok())
            .cloned()
            .unwrap();
        let id = match contents {
            Object::Array(items) => match items.last().and_then(|item| item.as_reference().ok()) {
                Some(id) => id,
                None => panic!("Contents 数组最后一项应该是水印流"),
            },
            other => panic!("Contents 应该变成数组，实际 {:?}", other),
        };
        doc.get_object(id)
            .ok()
            .and_then(|object| object.as_stream().ok())
            .map(|stream| stream.content.clone())
            .unwrap()
    }

    fn font_of(doc: &Document, page: ObjectId) -> Object {
        let resources = crate::pdf::ops::inheritable(doc, page, b"Resources").unwrap();
        let dict = match resources {
            Object::Reference(id) => doc.get_object(id).unwrap().as_dict().unwrap().clone(),
            Object::Dictionary(dict) => dict,
            other => panic!("Resources 类型 {:?}", other),
        };
        dict.get(b"Font")
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Fwm")
            .unwrap()
            .clone()
    }

    #[test]
    fn cjk_watermark_uses_a_type0_font_and_reaches_both_pages() {
        let dir = crate::test_support::TempDir::new("wm-cjk");
        let input = two_page_fixture(&dir, [0, 0, 400, 600]);
        let output = dir.join("out.pdf");
        let pages = watermark_pdf_blocking(
            input.to_string_lossy().to_string(),
            output.to_string_lossy().to_string(),
            "机密 DRAFT".to_string(),
            0.3,
        )
        .unwrap();
        assert_eq!(pages, 2);

        let doc = Document::load(&output).unwrap();
        let ids: Vec<ObjectId> = doc.get_pages().values().copied().collect();
        assert_eq!(ids.len(), 2);

        // 非嵌入 CJK：Type0 + UniGB-UCS2-H，后代 CIDFont 得真的挂在 DescendantFonts 上
        let font_ref = font_of(&doc, ids[0]);
        let font_id = font_ref.as_reference().unwrap();
        let font = doc.get_object(font_id).unwrap().as_dict().unwrap();
        assert_eq!(font.get(b"Subtype").unwrap().as_name().unwrap(), b"Type0");
        assert_eq!(
            font.get(b"Encoding").unwrap().as_name().unwrap(),
            b"UniGB-UCS2-H"
        );
        let descendant = font.get(b"DescendantFonts").unwrap().as_array().unwrap()[0]
            .as_reference()
            .unwrap();
        assert_eq!(
            doc.get_object(descendant)
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"Subtype")
                .unwrap()
                .as_name()
                .unwrap(),
            b"CIDFontType0"
        );

        // 文字按 UCS-2 BE 落进十六进制串：机=4E2D… 密=5BC6，拉丁也要凑成两字节
        let ops = appended_ops(&doc, ids[0]);
        let text = String::from_utf8_lossy(&ops).to_string();
        // UCS-2 BE 里连空格也要占两字节，漏掉 0020 就会把 "机密 DRAFT" 写成 "机密DRAFT"
        // 机=U+673A、密=U+5BC6
        assert!(
            text.contains("<673A5BC6002000440052004100460054>"),
            "{}",
            text
        );
        assert!(text.contains("Tj") && text.contains("/Fwm") && text.contains("/Gwm"));
        // 旋转 + 平移到页面中心，斜排水印才不会被裁掉
        assert!(text.contains("200.0 300.0 cm"), "{}", text);

        // 原来的正文一个字都不能动
        let original = doc
            .get_object(
                match doc
                    .get_object(ids[0])
                    .unwrap()
                    .as_dict()
                    .unwrap()
                    .get(b"Contents")
                    .unwrap()
                {
                    Object::Array(items) => items[0].as_reference().unwrap(),
                    other => panic!("{:?}", other),
                },
            )
            .unwrap()
            .as_stream()
            .unwrap();
        assert_eq!(
            original.content,
            b"BT /F1 24 Tf 60 720 Td (FirstPage) Tj ET".to_vec()
        );

        // 第二页没有自己的 Resources，靠继承也要拿到 /Fwm
        assert!(font_of(&doc, ids[1]).as_reference().is_ok());
        assert!(appended_ops(&doc, ids[1]).contains(&b'T'));
    }

    #[test]
    fn latin_watermark_stays_on_helvetica_and_alpha_state_is_wired() {
        let dir = crate::test_support::TempDir::new("wm-latin");
        let input = two_page_fixture(&dir, [0, 0, 400, 600]);
        let output = dir.join("out.pdf");
        watermark_pdf_blocking(
            input.to_string_lossy().to_string(),
            output.to_string_lossy().to_string(),
            "CONFIDENTIAL".to_string(),
            0.25,
        )
        .unwrap();

        let doc = Document::load(&output).unwrap();
        let first = *doc.get_pages().values().next().unwrap();
        let font_id = font_of(&doc, first).as_reference().unwrap();
        let font = doc.get_object(font_id).unwrap().as_dict().unwrap();
        assert_eq!(font.get(b"Subtype").unwrap().as_name().unwrap(), b"Type1");
        assert_eq!(
            font.get(b"BaseFont").unwrap().as_name().unwrap(),
            b"Helvetica"
        );

        let ops = String::from_utf8_lossy(&appended_ops(&doc, first)).to_string();
        assert!(ops.contains("<434F4E46"), "{}", ops);
        // /Gwm 必须能在继承到的 Resources 里查到，否则透明度不生效
        let resources = crate::pdf::ops::inheritable(&doc, first, b"Resources").unwrap();
        let dict = match resources {
            Object::Reference(id) => doc.get_object(id).unwrap().as_dict().unwrap().clone(),
            Object::Dictionary(dict) => dict,
            other => panic!("{:?}", other),
        };
        let state = dict
            .get(b"ExtGState")
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Gwm")
            .unwrap()
            .as_reference()
            .unwrap();
        let state = doc.get_object(state).unwrap().as_dict().unwrap();
        assert_eq!(state.get(b"ca").unwrap().as_f32().unwrap(), 0.25);
        assert_eq!(state.get(b"CA").unwrap().as_f32().unwrap(), 0.25);

        // 挂 /Gwm 不能把页面原有的 /G0 挤掉，否则原正文的透明度就丢了
        let existing = dict
            .get(b"ExtGState")
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"G0")
            .unwrap()
            .as_reference()
            .unwrap();
        assert_eq!(
            doc.get_object(existing)
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"Type")
                .unwrap()
                .as_name()
                .unwrap(),
            b"ExtGState"
        );
        assert!(
            dict.get(b"Font")
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"F1")
                .is_ok(),
            "原有的 /F1 不能丢"
        );
    }

    /// MediaBox 从 (36,36) 起算（带出血的印刷件），水印得按原点+宽高居中
    #[test]
    fn watermark_is_centered_on_the_media_box_not_the_page_origin() {
        let dir = crate::test_support::TempDir::new("wm-bleed");
        let input = two_page_fixture(&dir, [36, 36, 631, 831]);
        let output = dir.join("out.pdf");
        watermark_pdf_blocking(
            input.to_string_lossy().to_string(),
            output.to_string_lossy().to_string(),
            "DRAFT".to_string(),
            0.3,
        )
        .unwrap();

        let doc = Document::load(&output).unwrap();
        let ids: Vec<ObjectId> = doc.get_pages().values().copied().collect();
        let ops = String::from_utf8_lossy(&appended_ops(&doc, ids[0])).to_string();
        // 36 + 595/2 = 333.5，36 + 795/2 = 433.5；只按宽高算会落到 297.5 397.5
        assert!(ops.contains("1 0 0 1 333.5 433.5 cm"), "{}", ops);

        // 完全没有 MediaBox 的页面按 A4 居中，而不是贴着 (0,0) 画到裁切区外
        let orphan = String::from_utf8_lossy(&appended_ops(&doc, ids[1])).to_string();
        assert!(orphan.contains("1 0 0 1 297.5 421.0 cm"), "{}", orphan);
    }

    /// 字号放不下时必须整体缩小，而不是让尾字溢出页面被裁掉
    #[test]
    fn long_watermark_shrinks_to_fit_instead_of_being_clipped() {
        let text = "内部资料 机密 CONFIDENTIAL";
        let narrow = PageBox {
            x: 0.0,
            y: 0.0,
            width: 300.0,
            height: 300.0,
        };
        let ops = String::from_utf8_lossy(&overlay_ops(&narrow, text, true)).to_string();

        let size: f32 = ops
            .lines()
            .find(|line| line.contains("/Fwm"))
            .unwrap()
            .split(' ')
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let half: f32 = ops
            .lines()
            .find(|line| line.ends_with(" Td"))
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .trim_start_matches('-')
            .parse()
            .unwrap();
        let diagonal = 300.0f32.hypot(300.0);
        // 朴素做法会给到 min(w,h)/10 = 30pt，整条 20 字全角就是 600pt > 对角线
        assert!(size < 30.0, "放不下却没缩字号: {}", size);
        assert!(
            half * 2.0 <= diagonal * 0.9,
            "线段长 {} 超出对角线 {}",
            half * 2.0,
            diagonal
        );
        // 20 个 UCS-2 单元 × 1.15 安全系数：钉住"每字一个全角"这条实测结论
        assert!(
            ((half * 2.0) / size - 20.0 * 1.15).abs() < 0.6,
            "字宽模型变了，尾字会被裁: {}",
            half * 2.0 / size
        );

        // 旋转角跟着页面自己的对角线走，而不是死写 45°
        let wide = PageBox {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 400.0,
        };
        let ops = String::from_utf8_lossy(&overlay_ops(&wide, "DRAFT", true)).to_string();
        assert!(
            ops.contains("0.8944 0.4472 -0.4472 0.8944 0 0 cm"),
            "{}",
            ops
        );
    }
}
