use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::Path;

/// EPUB/MOBI 章节结构
#[derive(Debug, Serialize, Deserialize)]
pub struct Chapter {
    pub title: String,
    pub content: String,
    pub index: usize,
}

/// EPUB/MOBI 书籍结构
#[derive(Debug, Serialize, Deserialize)]
pub struct EpubBook {
    pub title: String,
    pub author: String,
    pub chapters: Vec<Chapter>,
    pub cover_path: String,
}

/// 解析EPUB文件（使用zip直接解析，不依赖epub crate的具体API）
pub fn parse_epub(path: &str) -> Result<EpubBook, String> {
    let file_path = Path::new(path);
    if !file_path.exists() {
        return Err(format!("文件不存在: {}", path));
    }

    let file = fs::File::open(file_path).map_err(|e| format!("打开EPUB失败: {}", e))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("解压EPUB失败: {}", e))?;

    // 1. 解析container.xml找到OPF路径
    let mut opf_path = String::new();
    for i in 0..archive.len() {
        let mut zf = archive
            .by_index(i)
            .map_err(|e| format!("读取ZIP条目失败: {}", e))?;
        // zip 9 起 name() 返回 ZipResult<Cow<str>>：条目名可能压根不是 UTF-8。
        // EPUB 规范要求名字是 ASCII/UTF-8，所以名字解不出来的条目一律当"不是要找的那个"。
        if zf.name().is_ok_and(|n| n.ends_with("container.xml")) {
            let mut content = String::new();
            zf.read_to_string(&mut content).ok();
            // 简单提取rootfile路径
            if let Some(start) = content.find("full-path=\"") {
                let s = start + 11;
                if let Some(end) = content[s..].find('"') {
                    opf_path = content[s..s + end].to_string();
                    break;
                }
            }
        }
    }

    let file_stem = file_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "未知书名".to_string());

    let mut title = file_stem.clone();
    let mut author = "未知作者".to_string();
    let mut spine_order: Vec<String> = Vec::new();
    let mut manifest: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();

    // 2. 解析OPF文件
    if !opf_path.is_empty() {
        let opf_dir = opf_path.rfind('/').map(|i| &opf_path[..i]).unwrap_or("");
        if let Ok(mut zf) = archive.by_name(&opf_path) {
            let mut opf_content = String::new();
            zf.read_to_string(&mut opf_content).ok();

            // 提取标题
            if let Some(s) = opf_content.find("<dc:title>") {
                if let Some(e) = opf_content[s + 10..].find("</dc:title>") {
                    let t = opf_content[s + 10..s + 10 + e].trim().to_string();
                    if !t.is_empty() {
                        title = t;
                    }
                }
            }

            // 提取作者
            if let Some(s) = opf_content.find("<dc:creator") {
                if let Some(gt) = opf_content[s..].find('>') {
                    if let Some(e) = opf_content[s + gt + 1..].find("</dc:creator>") {
                        let a = opf_content[s + gt + 1..s + gt + 1 + e].trim().to_string();
                        if !a.is_empty() {
                            author = a;
                        }
                    }
                }
            }

            // 解析manifest
            for part in opf_content.split("<item ") {
                let mut id = String::new();
                let mut href = String::new();
                let mut media_type = String::new();
                for attr in part.splitn(20, '"').collect::<Vec<_>>().chunks(2) {
                    if attr.len() == 2 {
                        let key = attr[0].trim().trim_end_matches('=').trim();
                        let val = attr[1];
                        match key {
                            "id" => id = val.to_string(),
                            "href" => href = val.to_string(),
                            "media-type" => media_type = val.to_string(),
                            _ => {}
                        }
                    }
                }
                if !id.is_empty() && !href.is_empty() {
                    let full_href = if href.starts_with("http") {
                        href.clone()
                    } else if !opf_dir.is_empty() {
                        format!("{}/{}", opf_dir, href)
                    } else {
                        href.clone()
                    };
                    manifest.insert(id, (full_href, media_type));
                }
            }

            // 解析spine
            if let Some(spine_start) = opf_content.find("<spine") {
                if let Some(spine_end) = opf_content[spine_start..].find("</spine>") {
                    let spine = &opf_content[spine_start..spine_start + spine_end];
                    for itemref in spine.split("<itemref") {
                        if let Some(idref_start) = itemref.find("idref=\"") {
                            let s = idref_start + 7;
                            if let Some(e) = itemref[s..].find('"') {
                                spine_order.push(itemref[s..s + e].to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    // 3. 把 zip 里所有"资源文件"（图片/字体/CSS 等非 XHTML/HTML 文本）解到临时目录，
    //    这样章节 XHTML 里的 <img src="相对路径"> 可以被改写成绝对路径，
    //    配合 tauri 的 asset:// 协议让 webview 能直接加载。
    //    临时目录基于源 epub 路径的 hash 命名，重复打开同一本书不重复解压。
    let temp_root =
        extract_epub_assets(&mut archive, file_path).unwrap_or_else(|_| std::env::temp_dir());

    // 4. 按spine顺序读取章节
    let mut chapters = Vec::new();
    let mut index = 0;

    for item_id in &spine_order {
        if let Some((href, _media_type)) = manifest.get(item_id) {
            match archive.by_name(href) {
                Ok(mut zf) => {
                    let mut content = String::new();
                    if zf.read_to_string(&mut content).is_ok() && !content.trim().is_empty() {
                        // 把 XHTML 里所有 <img src> / <image href> 改写为绝对路径
                        let rewritten = rewrite_image_paths(&content, href, &temp_root);
                        let ch_title = extract_html_title(&content)
                            .unwrap_or_else(|| format!("章节 {}", index + 1));
                        chapters.push(Chapter {
                            title: ch_title,
                            content: rewritten,
                            index,
                        });
                        index += 1;
                    }
                }
                Err(_) => continue,
            }
        }
    }

    // 回退：如果没有找到章节，扫描所有HTML文件
    if chapters.is_empty() {
        let mut html_entries: Vec<(String, String)> = Vec::new();
        for i in 0..archive.len() {
            let mut zf = archive
                .by_index(i)
                .map_err(|e| format!("读取ZIP条目失败: {}", e))?;
            let Some(name) = zf.name().ok().map(|n| n.to_string()) else {
                continue;
            };
            if name.ends_with(".html") || name.ends_with(".xhtml") || name.ends_with(".htm") {
                let mut content = String::new();
                if zf.read_to_string(&mut content).is_ok() && !content.trim().is_empty() {
                    html_entries.push((name, content));
                }
            }
        }
        for (_, content) in html_entries {
            let ch_title =
                extract_html_title(&content).unwrap_or_else(|| format!("章节 {}", index + 1));
            chapters.push(Chapter {
                title: ch_title,
                content,
                index,
            });
            index += 1;
        }
    }

    if chapters.is_empty() {
        chapters.push(Chapter {
            title: "全文".to_string(),
            content: format!("<p>无法从EPUB中提取章节内容。书名: {}</p>", title),
            index: 0,
        });
    }

    Ok(EpubBook {
        title,
        author,
        chapters,
        cover_path: String::new(),
    })
}

/// 从HTML内容中提取标题
fn extract_html_title(html: &str) -> Option<String> {
    if let Some(start) = html.find("<title>") {
        if let Some(end) = html.find("</title>") {
            let title = html[start + 7..end].trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    if let Some(start) = html.find("<h1") {
        if let Some(tag_end) = html[start..].find('>') {
            let h1_start = start + tag_end + 1;
            if let Some(h1_end) = html[h1_start..].find("</h1>") {
                let title = strip_html_tags(&html[h1_start..h1_start + h1_end]);
                if !title.is_empty() {
                    return Some(title);
                }
            }
        }
    }
    if let Some(start) = html.find("<h2") {
        if let Some(tag_end) = html[start..].find('>') {
            let h2_start = start + tag_end + 1;
            if let Some(h2_end) = html[h2_start..].find("</h2>") {
                let title = strip_html_tags(&html[h2_start..h2_start + h2_end]);
                if !title.is_empty() {
                    return Some(title);
                }
            }
        }
    }
    None
}

/// 把 epub 内的非内容文件（图片/字体/媒体）解到临时目录。
/// 临时目录按源 epub 路径的 hash 命名：
/// - 同本书多次打开：复用已解压的文件，不重复 IO
/// - 不同书：互不污染
///
/// 临时目录路径会回传，用于把 XHTML 里的 <img src="相对"> 改写为绝对路径。
fn extract_epub_assets<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    source_epub: &Path,
) -> Result<std::path::PathBuf, String> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    // 临时目录名：z-tool-epub-{hash16}
    let mut hasher = DefaultHasher::new();
    source_epub.to_string_lossy().hash(&mut hasher);
    let hash = format!("{:016x}", hasher.finish());
    let temp_root = std::env::temp_dir().join(format!("z-tool-epub-{}", hash));

    // 已解压过：直接复用
    if temp_root.exists() {
        return Ok(temp_root);
    }
    fs::create_dir_all(&temp_root).map_err(|e| format!("创建临时目录失败: {}", e))?;

    // 文本/容器类文件不需要解压（webview 不直接渲染它们）
    let skip_exts: HashSet<&str> = [
        "opf", "ncx", "xml", "xhtml", "html", "htm", "css", "js", "ttxt",
    ]
    .iter()
    .copied()
    .collect();

    for i in 0..archive.len() {
        let mut zf = archive
            .by_index(i)
            .map_err(|e| format!("读取 ZIP 条目失败: {}", e))?;
        // 名字解不出来就跳过：下面要拿它拼落盘路径，空名字会拼到 temp_root 自己身上
        let Some(name) = zf.name().ok().map(|n| n.to_string()) else {
            continue;
        };
        // 跳过目录
        if name.ends_with('/') {
            continue;
        }
        // 按扩展名判断是否需要解压
        let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
        if skip_exts.contains(ext.as_str()) {
            continue;
        }
        // 解到 temp_root 下（保持 zip 内相对路径）
        let out_path = temp_root.join(&name);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let mut out = match fs::File::create(&out_path) {
            Ok(f) => f,
            Err(_) => continue, // 单个资源失败不阻塞整本书
        };
        let _ = std::io::copy(&mut zf, &mut out);
    }

    Ok(temp_root)
}

/// 把 XHTML 里的 <img src="..."> / <image href="..."> 改写为本地绝对路径。
/// - 入参 `chapter_href` 是这个章节文件在 zip 里的相对路径（用于推算图片相对基准）
/// - `temp_root` 是 `extract_epub_assets` 返回的临时根目录
///
/// 处理：找到 src/href，resolve 到 temp_root 下的绝对路径，替换为 file:// URL
fn rewrite_image_paths(xhtml: &str, chapter_href: &str, temp_root: &Path) -> String {
    let chapter_dir = chapter_href.rsplit_once('/').map(|(d, _)| d).unwrap_or("");

    // 用极简状态机：扫描每个标签，找到 src="..." 或 href="..." 替换
    let lower = xhtml.to_ascii_lowercase();
    // 检查是否包含 <img 或 <image（否则直接返回原文）
    if !lower.contains("<img") && !lower.contains("<image") {
        return xhtml.to_string();
    }

    // 简单的属性替换：扫描 <img ...> 和 <image ...>，替换 src=/href=
    let bytes = xhtml.as_bytes();
    let mut out = String::with_capacity(bytes.len() + 256);
    let mut i = 0;
    while i < bytes.len() {
        // 找下一个 <img 或 <image
        let rest = &xhtml[i..];
        let rest_lower = &lower[i..];
        let img_pos = rest_lower.find("<img");
        let image_pos = rest_lower.find("<image");
        let next_tag = match (img_pos, image_pos) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        match next_tag {
            None => {
                out.push_str(rest);
                break;
            }
            Some(p) => {
                // 复制 <img / <image 之前的所有内容
                out.push_str(&rest[..p]);
                // 找该标签的结束 > （注意属性值里可能含 >，但属性都用引号包所以简单找首个 >）
                let after_tag = &rest[p..];
                let tag_end = after_tag.find('>').unwrap_or(after_tag.len());
                let tag_str = &after_tag[..tag_end];
                let tag_end_abs = i + p + tag_end + 1;
                // 替换 src="..." 和 href="..."（xlink:href 走 href 同一处理）
                let rewritten = rewrite_tag_attributes(tag_str, chapter_dir, temp_root);
                out.push_str(&rewritten);
                out.push('>');
                i = tag_end_abs;
            }
        }
    }
    out
}

/// 在一个 <img / <image 标签字符串里替换 src="..." 和 href="..." 路径
fn rewrite_tag_attributes(tag: &str, chapter_dir: &str, temp_root: &Path) -> String {
    let mut out = String::with_capacity(tag.len() + 64);
    // 切分出 <img 后的属性段
    let attr_start = tag.find(' ').map(|i| i + 1).unwrap_or(tag.len());
    out.push_str(&tag[..attr_start]);
    let attrs = &tag[attr_start..];
    let mut j = 0;
    let attr_names = ["src", "href"];
    while j < attrs.len() {
        // 跳过空白
        if attrs.as_bytes()[j].is_ascii_whitespace() {
            out.push(attrs.as_bytes()[j] as char);
            j += 1;
            continue;
        }
        // 读取属性名
        let name_start = j;
        while j < attrs.len()
            && !attrs.as_bytes()[j].is_ascii_whitespace()
            && attrs.as_bytes()[j] != b'='
        {
            j += 1;
        }
        let name = &attrs[name_start..j];
        // 跳过空白
        while j < attrs.len() && attrs.as_bytes()[j].is_ascii_whitespace() {
            out.push(attrs.as_bytes()[j] as char);
            j += 1;
        }
        if j >= attrs.len() || attrs.as_bytes()[j] != b'=' {
            // 布尔属性，直接写
            out.push_str(name);
            continue;
        }
        out.push('=');
        j += 1;
        // 跳过 = 后的空白
        while j < attrs.len() && attrs.as_bytes()[j].is_ascii_whitespace() {
            out.push(attrs.as_bytes()[j] as char);
            j += 1;
        }
        if j >= attrs.len() {
            break;
        }
        // 解析值：引号包裹 or 裸值
        let q = attrs.as_bytes()[j];
        if q == b'"' || q == b'\'' {
            j += 1;
            let val_start = j;
            while j < attrs.len() && attrs.as_bytes()[j] != q {
                j += 1;
            }
            let val = &attrs[val_start..j];
            if j < attrs.len() {
                j += 1; // 跳闭合引号
            }
            // 决定是否替换
            let lower_name = name.to_ascii_lowercase();
            // 真正关心的：img 的 src；image（svg:image）的 href 或 xlink:href
            let is_target = (attr_names.contains(&lower_name.as_str())
                || lower_name == "xlink:href")
                && !val.is_empty()
                && !val.starts_with("http")
                && !val.starts_with("data:");
            if is_target {
                let abs = resolve_resource_path(chapter_dir, val, temp_root);
                // Tauri 2 资源协议：macOS 用 asset://localhost/<path> 形式
                // (Tauri 文档：https://tauri.app/v1/guides/features/resources#protocol)
                // WKWebView 默认拦截 file://，所以走 Tauri 内置的 asset 协议，
                // 配合 tauri.conf.json 里 assetProtocol.scope: ["**"] 让 webview 放行
                let abs_str = abs.to_string_lossy();
                let url = format!("asset://localhost{}", abs_str);
                out.push(q as char);
                out.push_str(&url);
                out.push(q as char);
            } else {
                out.push(q as char);
                out.push_str(val);
                out.push(q as char);
            }
        } else {
            // 裸属性值（不合法，但兼容）
            let val_start = j;
            while j < attrs.len() && !attrs.as_bytes()[j].is_ascii_whitespace() {
                j += 1;
            }
            out.push_str(&attrs[val_start..j]);
        }
    }
    out
}

/// 把 XHTML 里的相对路径 resolve 到 temp_root 下的绝对路径
fn resolve_resource_path(chapter_dir: &str, rel: &str, temp_root: &Path) -> std::path::PathBuf {
    use std::path::PathBuf;
    // 处理 .. 和 . ，相对章节所在目录
    let base = if chapter_dir.is_empty() {
        PathBuf::from("")
    } else {
        PathBuf::from(chapter_dir)
    };
    let combined = base.join(rel);
    // 标准化（去 .. 和 .）
    let normalized = normalize_path(&combined);
    temp_root.join(normalized)
}

/// 简易 path 标准化：处理 "." 和 ".."
fn normalize_path(p: &std::path::Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn strip_html_tags(html: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;
    for ch in html.chars() {
        if ch == '<' {
            in_tag = true;
        } else if ch == '>' {
            in_tag = false;
        } else if !in_tag {
            result.push(ch);
        }
    }
    result.trim().to_string()
}

/// 解析 MOBI 家族（.mobi / .azw / .azw3 / .prc）。
///
/// 真正的解析在 [`crate::mobi`]：这里之所以单独留一层，是因为**这条转发不是多余的**。
/// 早先这里是「不解压直接当 UTF-8 读」的简化实现，对任何真实 MOBI 都只输出乱码，
/// 而调用方看不出区别（命令成功、有章节、就是不认字）。留一层薄壳能让
/// `parse_ebook` 的分派表保持一行一项，不必知道 MOBI 的细节。
pub fn parse_mobi(path: &str) -> Result<EpubBook, String> {
    crate::mobi::parse(path)
}

// ───────────────────────── 统一入口 ─────────────────────────

/// 按扩展名分派到具体解析器。
///
/// 前端只需要记一个命令：这本书是 epub 还是 mobi，由这里判断。让每个调用点
/// 各抄一份扩展名表，那份表早晚会和实际实现长歪（KB 那边就出过这种情况）。
pub fn parse_ebook(path: &str) -> Result<EpubBook, String> {
    let ext = Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "epub" => parse_epub(path),
        "mobi" | "azw" | "azw3" | "prc" => parse_mobi(path),
        "txt" => parse_txt(path),
        "fb2" => parse_fb2(path),
        other => Err(format!("`{}` 不是可解析的电子书格式", other)),
    }
}

// ───────────────────────── TXT ─────────────────────────

/// 文本文件的一个章节。`body` 是**纯文本**，转 HTML 由 [`txt_body_to_html`] 做。
pub struct TextSection {
    pub title: String,
    pub body: String,
}

/// 解析 TXT 小说。
///
/// TXT 是唯一需要猜编码的格式：同一本小说的 UTF-8 版和 GBK 版在磁盘上长得一样，
/// 没有任何元数据可查。国内 TXT 小说绝大多数是 GB18030，所以「严格 UTF-8 失败
/// 就按 GB18030」这条兜底是最有效的一刀。
pub fn parse_txt(path: &str) -> Result<EpubBook, String> {
    let raw = fs::read(path).map_err(|e| format!("读取失败: {}", e))?;
    if raw.is_empty() {
        return Err("文件是空的".to_string());
    }

    let text = decode_text(&raw);
    if text.trim().is_empty() {
        return Err("文件里没有文本内容".to_string());
    }

    let title = Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "未命名".to_string());

    let sections = split_txt_sections(&text);
    let chapters = sections
        .into_iter()
        .enumerate()
        .map(|(i, s)| Chapter {
            title: s.title,
            content: txt_body_to_html(&s.body),
            index: i,
        })
        .collect();

    Ok(EpubBook {
        title,
        author: "未知作者".to_string(),
        chapters,
        cover_path: String::new(),
    })
}

/// 嗅探编码并解码。
///
/// 顺序不能反：BOM 优先（它是最强证据），然后严格 UTF-8（能通过基本就不是
/// 单字节编码），最后才按 GB18030。反过来先按 GB18030 的话，UTF-8 的中文会被
/// 解成一堆生僻字 —— 这是「看着像乱码但每个字都认识」的那种，最难判断。
fn decode_text(raw: &[u8]) -> String {
    if raw.starts_with(&[0xef, 0xbb, 0xbf]) {
        return String::from_utf8_lossy(&raw[3..]).to_string();
    }
    if raw.starts_with(&[0xff, 0xfe]) {
        return encoding_rs::UTF_16LE.decode(&raw[2..]).0.into_owned();
    }
    if raw.starts_with(&[0xfe, 0xff]) {
        return encoding_rs::UTF_16BE.decode(&raw[2..]).0.into_owned();
    }
    if let Ok(s) = std::str::from_utf8(raw) {
        return s.to_string();
    }

    // 无 BOM 的 UTF-16：中文文本按奇数位多为 0x00。先看这个特征，
    // 否则 4 万字的 UTF-16 中文会被当成 GB18030 解出一半的乱码。
    if raw.len() % 2 == 0 && raw.len() > 8 {
        let zeros_at_odd = raw.iter().skip(1).step_by(2).filter(|&&b| b == 0).count();
        let pairs = raw.len() / 2;
        if pairs > 0 && zeros_at_odd * 10 > pairs * 6 {
            let (s, _, err) = encoding_rs::UTF_16LE.decode(raw);
            if !err {
                return s.into_owned();
            }
        }
    }

    let (cow, _, _) = encoding_rs::GB18030.decode(raw);
    cow.into_owned()
}

/// 判断一行是不是章节标题。
///
/// 中文小说的章节行几乎都是「第X章/节/回/卷/部/篇/集」打头，再加几个固定词
/// （序章、楔子、番外…）。行长度卡 40 字是关键：正文里「第三次世界大战爆发了」
/// 也以「第三」开头，但它是一整段，不该被切成标题。
pub fn is_chapter_heading(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    let chars: Vec<char> = t.chars().collect();
    if chars.len() > 40 {
        return false;
    }

    // 固定词
    const FIXED: &[&str] = &[
        "序章", "序言", "楔子", "引子", "前言", "后记", "尾声", "终章", "番外", "附录",
    ];
    if FIXED.iter().any(|f| t.starts_with(f)) {
        return true;
    }

    // 第X章 / 第X节 / 第X回 / 第X卷 / 第X部 / 第X篇 / 第X集
    if chars[0] == '第' {
        let mut i = 1;
        let mut digits = 0;
        while i < chars.len() {
            let c = chars[i];
            if is_cjk_digit(c) {
                digits += 1;
                i += 1;
                continue;
            }
            return digits > 0 && matches!(c, '章' | '节' | '回' | '卷' | '部' | '篇' | '集');
        }
        return false;
    }

    // 卷X / 章X
    if matches!(chars[0], '卷' | '章' | '回') && chars.len() > 1 && is_cjk_digit(chars[1]) {
        return true;
    }

    // Chapter 12 / CHAPTER XII
    let lower = t.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("chapter") {
        let rest = rest.trim_start_matches(|c: char| c == ':' || c == ' ' || c == '.');
        return !rest.is_empty()
            && rest
                .chars()
                .next()
                .map(|c| c.is_ascii_digit() || is_roman(c))
                .unwrap_or(false);
    }

    false
}

fn is_cjk_digit(c: char) -> bool {
    matches!(
        c,
        '零' | '〇'
            | '一'
            | '二'
            | '两'
            | '三'
            | '四'
            | '五'
            | '六'
            | '七'
            | '八'
            | '九'
            | '十'
            | '百'
            | '千'
            | '万'
            | '0'..='9'
    )
}

fn is_roman(c: char) -> bool {
    matches!(c.to_ascii_lowercase(), 'i' | 'v' | 'x' | 'l' | 'c' | 'd' | 'm')
}

/// 按章节标题切段。没有标题的纯文本整体算一节。
///
/// 只切不合并：宁可多出几个「无标题」小节，也不要把两章粘在一节里 ——
/// 后者在翻页阅读器里表现为「翻到某页突然换了两章」，读者很难判断进度。
pub fn split_txt_sections(text: &str) -> Vec<TextSection> {
    let mut out: Vec<TextSection> = Vec::new();
    let mut title = "正文".to_string();
    let mut buf: Vec<String> = Vec::new();

    fn flush(title: &str, buf: &mut Vec<String>, out: &mut Vec<TextSection>) {
        let body = buf.join("\n");
        buf.clear();
        if body.trim().is_empty() {
            return;
        }
        out.push(TextSection {
            title: title.to_string(),
            body,
        });
    }

    for line in text.lines() {
        let t = line.trim();
        if is_chapter_heading(t) {
            flush(&title, &mut buf, &mut out);
            title = t.to_string();
        } else {
            buf.push(t.to_string());
        }
    }
    flush(&title, &mut buf, &mut out);

    if out.is_empty() {
        out.push(TextSection {
            title: "正文".to_string(),
            body: text.to_string(),
        });
    }
    out
}

/// 纯文本 → HTML 片段。
///
/// 每个非空行当成一个段落。中文 TXT 小说的排版是「一行一段」，如果改成
/// 「空行才分段」，整本书会粘成几十个巨型段落，翻页器一页显示不出完整段落，
/// 阅读体验比现在这种差得多。
pub fn txt_body_to_html(body: &str) -> String {
    let mut out = String::with_capacity(body.len() + 64);
    for line in body.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        out.push_str("<p>");
        out.push_str(&escape_html(t));
        out.push_str("</p>");
    }
    if out.is_empty() {
        out.push_str("<p></p>");
    }
    out
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

// ───────────────────────── FB2 ─────────────────────────

/// 解析 FictionBook 2（.fb2）。它本来就是 XML，只差把标签换成 HTML 的。
pub fn parse_fb2(path: &str) -> Result<EpubBook, String> {
    let raw = fs::read(path).map_err(|e| format!("读取失败: {}", e))?;
    let text = decode_text(&raw);

    let stem = Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "未命名".to_string());

    let title = extract_first_tag_text(&text, "book-title").unwrap_or(stem);
    let author = extract_fb2_author(&text).unwrap_or_else(|| "未知作者".to_string());

    let html = fb2_body_to_html(&text);
    let chunks = split_html_on_headings(&html);

    let chapters = chunks
        .into_iter()
        .enumerate()
        .map(|(i, html)| {
            let t = first_tag_text(&html, "h3").unwrap_or_else(|| format!("第 {} 节", i + 1));
            Chapter {
                title: t,
                content: html,
                index: i,
            }
        })
        .collect::<Vec<_>>();

    if chapters.is_empty() {
        return Err("FB2 里没有正文".to_string());
    }

    Ok(EpubBook {
        title,
        author,
        chapters,
        cover_path: String::new(),
    })
}

fn extract_fb2_author(text: &str) -> Option<String> {
    // <author><first-name>名</first-name><last-name>姓</last-name></author>
    let start = text.find("<author>")?;
    let end = text[start..].find("</author>")? + start;
    let block = &text[start..end];
    let join = |a: &str, b: &str| -> Option<String> {
        let first = extract_first_tag_text(block, a).unwrap_or_default();
        let last = extract_first_tag_text(block, b).unwrap_or_default();
        let s = format!("{}{}", first, last).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    };
    // 中文 FB2 常写成 <nickname>，西文是 first+last
    join("first-name", "last-name")
        .or_else(|| join("last-name", "first-name"))
        .or_else(|| extract_first_tag_text(block, "nickname"))
}

/// 把 `<body>` 里的内容换成等价 HTML。只做标签名映射，不动层级 ——
/// FB2 的 section 嵌套本身就是章节结构，转成 div 之后由标题切分来定章。
fn fb2_body_to_html(text: &str) -> String {
    let Some(bstart) = text.find("<body") else {
        return String::new();
    };
    let Some(gt) = text[bstart..].find('>') else {
        return String::new();
    };
    let content_start = bstart + gt + 1;
    // 可能有多本 body（<body name="notes">），只取第一个正文
    let content_end = text[content_start..]
        .find("</body>")
        .map(|p| content_start + p)
        .unwrap_or(text.len());
    let body = &text[content_start..content_end];

    let mut s = body.to_string();
    // 命名空间前缀形式 <fb:section>
    s = s.replace("<fb:", "<").replace("</fb:", "</");
    // 先处理成对标签，再做自闭合
    s = s.replace("<section", "<div").replace("</section>", "</div>");
    s = s.replace("<title>", "<h3>").replace("</title>", "</h3>");
    s = s.replace("<subtitle>", "<h4>").replace("</subtitle>", "</h4>");
    s = s.replace("<emphasis", "<em").replace("</emphasis>", "</em>");
    s = s.replace("<epigraph", "<blockquote").replace("</epigraph>", "</blockquote>");
    s = s.replace("<poem", "<div").replace("</poem>", "</div>");
    s = s.replace("<stanza", "<div").replace("</stanza>", "</div>");
    // 图片在独立二进制段里，这里没导出，留着就是裂图
    s = s.replace("<image", "<span data-dropped=\"image\"");
    s = s.replace("<empty-line/>", "<p><br/></p>");
    s = s.replace("<empty-line />", "<p><br/></p>");
    // 去掉 XML 里没意义的 <p/> 与自闭合 p
    s = s.replace("<p/>", "<p><br/></p>");
    s
}

/// 按 h3/h4 切块。FB2 的 `<title>` 已经转成了 h3，正好当章节边界。
fn split_html_on_headings(html: &str) -> Vec<String> {
    let mut marks: Vec<usize> = Vec::new();
    for tag in ["<h3", "<h4"] {
        let mut from = 0;
        while let Some(p) = html[from..].find(tag) {
            marks.push(from + p);
            from += p + tag.len();
        }
    }
    if marks.is_empty() {
        let t = html.trim();
        return if t.is_empty() {
            Vec::new()
        } else {
            vec![t.to_string()]
        };
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

fn extract_first_tag_text(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{}", tag);
    let s = text.find(&open)?;
    let gt = text[s..].find('>')?;
    let start = s + gt + 1;
    let close = format!("</{}>", tag);
    let e = text[start..].find(&close)?;
    let raw = &text[start..start + e];
    let cleaned = raw
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'");
    let t = cleaned.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn first_tag_text(html: &str, tag: &str) -> Option<String> {
    let open = format!("<{}", tag);
    let s = html.find(&open)?;
    let gt = html[s..].find('>')?;
    let start = s + gt + 1;
    let close = format!("</{}>", tag);
    let e = html[start..].find(&close)?;
    let inner = &html[start..start + e];
    let txt = strip_html(inner);
    if txt.is_empty() {
        None
    } else {
        Some(truncate(txt.trim(), 60))
    }
}

fn strip_html(html: &str) -> String {
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

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push('…');
    out
}

// ───────────────────────── 漫画（CBZ） ─────────────────────────

/// 把 CBZ（zip 装的图片）解到 `cache_root` 下，返回按阅读顺序排好的图片路径。
///
/// 走「解到磁盘再用 asset 协议加载」而不是 base64 内联：一部漫画几十页、
/// 单页几百 KB，base64 会让 IPC 消息涨到几十 MB，窗口会直接卡住。
///
/// `cache_root` 由调用方给：Tauri 的 asset 协议有路径白名单，通常只放行
/// `$HOME` / `$APPLOCALDATA`，而 `std::env::temp_dir()` 在 macOS 上落在
/// `/var/folders`，写在那里前端根本加载不出来。原语层不认 Tauri，
/// 所以目录选择权交给命令层。
///
/// 子目录名 = 「路径 + 大小 + 修改时间」的哈希：换一本漫画就换目录，
/// 同一本重开则命中旧目录（已解过就不再解）。
pub fn extract_comic(path: &str, cache_root: &str) -> Result<Vec<String>, String> {
    let meta = fs::metadata(path).map_err(|e| format!("读取失败: {}", e))?;
    let stamp = format!(
        "{}|{}|{}",
        path,
        meta.len(),
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    let dir = Path::new(cache_root).join(hash_name(&stamp));
    fs::create_dir_all(&dir).map_err(|e| format!("建缓存目录失败: {}", e))?;

    let file = fs::File::open(path).map_err(|e| format!("打开失败: {}", e))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("不是有效的 CBZ: {}", e))?;

    // 已解过且页数对得上就直接用
    let mut existing = collect_images(&dir);
    if !existing.is_empty() {
        return Ok(existing.drain(..).map(|p| p.to_string_lossy().to_string()).collect());
    }

    let mut names: Vec<(usize, String)> = Vec::new();
    for i in 0..zip.len() {
        let Ok(zf) = zip.by_index(i) else { continue };
        if zf.is_dir() {
            continue;
        }
        // zip 9 起 name() 返回 ZipResult<Cow<str>>，条目名可能压根不是 UTF-8
        let Ok(raw_name) = zf.name() else { continue };
        let name = raw_name.to_string();
        let lower = name.to_lowercase();
        if !matches!(
            Path::new(&lower).extension().and_then(|e| e.to_str()),
            Some("jpg") | Some("jpeg") | Some("png") | Some("webp") | Some("gif") | Some("bmp")
        ) {
            continue;
        }
        names.push((i, name));
    }
    if names.is_empty() {
        return Err("这个 CBZ 里没有图片".to_string());
    }
    names.sort_by(|a, b| natural_cmp(&a.1, &b.1));

    let mut out = Vec::with_capacity(names.len());
    for (seq, (idx, name)) in names.iter().enumerate() {
        let Ok(mut zf) = zip.by_index(*idx) else { continue };
        let ext = Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("jpg")
            .to_lowercase();
        let target = dir.join(format!("{:05}.{}", seq, ext));
        let mut buf = Vec::new();
        if std::io::Read::read_to_end(&mut zf, &mut buf).is_err() {
            continue;
        }
        if fs::write(&target, &buf).is_err() {
            continue;
        }
        out.push(target.to_string_lossy().to_string());
    }

    if out.is_empty() {
        return Err("解不出任何页面".to_string());
    }
    Ok(out)
}

fn collect_images(dir: &Path) -> Vec<std::path::PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<std::path::PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    files
}

fn hash_name(s: &str) -> String {
    // FNV-1a：这里只用来分目录，不需要抗碰撞
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:016x}", h)
}

/// 自然排序：`2.jpg` 要排在 `10.jpg` 前面。漫画文件名常是 `1.jpg` 而非 `001.jpg`，
/// 纯字典序会把第 10 页排到第 2 页前面。
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(ca), Some(cb)) => {
                if ca.is_ascii_digit() && cb.is_ascii_digit() {
                    let mut na = String::new();
                    while let Some(&c) = ai.peek() {
                        if !c.is_ascii_digit() {
                            break;
                        }
                        na.push(c);
                        ai.next();
                    }
                    let mut nb = String::new();
                    while let Some(&c) = bi.peek() {
                        if !c.is_ascii_digit() {
                            break;
                        }
                        nb.push(c);
                        bi.next();
                    }
                    let va: u64 = na.parse().unwrap_or(0);
                    let vb: u64 = nb.parse().unwrap_or(0);
                    match va.cmp(&vb) {
                        std::cmp::Ordering::Equal => continue,
                        other => return other,
                    }
                }
                match ca.cmp(&cb) {
                    std::cmp::Ordering::Equal => {
                        ai.next();
                        bi.next();
                    }
                    other => return other,
                }
            }
        }
    }
}

/// 提取EPUB封面（base64编码）
pub fn get_epub_cover(path: &str) -> Result<String, String> {
    let file_path = Path::new(path);
    if !file_path.exists() {
        return Err(format!("文件不存在: {}", path));
    }

    let file = fs::File::open(file_path).map_err(|e| format!("打开EPUB失败: {}", e))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("解压EPUB失败: {}", e))?;

    // 查找封面图片
    for i in 0..archive.len() {
        let mut zf = archive
            .by_index(i)
            .map_err(|e| format!("读取ZIP条目失败: {}", e))?;
        let Some(name) = zf.name().ok().map(|n| n.to_string().to_lowercase()) else {
            continue;
        };
        if name.contains("cover")
            && (name.ends_with(".jpg") || name.ends_with(".jpeg") || name.ends_with(".png"))
        {
            let mut buf = Vec::new();
            zf.read_to_end(&mut buf)
                .map_err(|e| format!("读取封面失败: {}", e))?;
            // 使用标准base64编码
            const CHARS: &[u8] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut result = String::with_capacity(buf.len() * 4 / 3 + 4);
            let chunks = buf.chunks(3);
            for chunk in chunks {
                let b0 = chunk[0] as u32;
                let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
                let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
                let triple = (b0 << 16) | (b1 << 8) | b2;
                result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
                result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
                result.push(if chunk.len() > 1 {
                    CHARS[((triple >> 6) & 0x3F) as usize] as char
                } else {
                    '='
                });
                result.push(if chunk.len() > 2 {
                    CHARS[(triple & 0x3F) as usize] as char
                } else {
                    '='
                });
            }
            return Ok(result);
        }
    }

    Err("未找到封面图片".to_string())
}
