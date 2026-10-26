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

/// 解析MOBI文件（简单实现）
pub fn parse_mobi(path: &str) -> Result<EpubBook, String> {
    let file_path = Path::new(path);
    if !file_path.exists() {
        return Err(format!("文件不存在: {}", path));
    }

    let bytes = fs::read(file_path).map_err(|e| format!("读取MOBI文件失败: {}", e))?;
    if bytes.len() < 64 {
        return Err("MOBI文件过小，无法解析".to_string());
    }

    // 检查MOBI header magic
    if &bytes[60..64] != b"MOBI" {
        return Err("不是有效的MOBI文件格式".to_string());
    }

    let file_stem = file_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "未知书名".to_string());

    // 提取书名
    let title = if bytes.len() > 92 {
        let name_offset =
            u32::from_be_bytes(bytes[84..88].try_into().unwrap_or([0, 0, 0, 0])) as usize;
        let name_len =
            u32::from_be_bytes(bytes[88..92].try_into().unwrap_or([0, 0, 0, 0])) as usize;
        if name_offset > 0 && name_len > 0 && name_offset + name_len <= bytes.len() {
            String::from_utf8_lossy(&bytes[name_offset..name_offset + name_len])
                .trim_end_matches('\0')
                .to_string()
        } else {
            file_stem
        }
    } else {
        file_stem
    };

    let content = extract_mobi_text_content(&bytes);
    let chapters = if content.is_empty() {
        vec![Chapter {
            title: "全文".to_string(),
            content: "<p>无法从MOBI文件中提取文本内容。建议转换为EPUB格式后阅读。</p>".to_string(),
            index: 0,
        }]
    } else {
        // 按HTML heading分割
        let mut chs = Vec::new();
        let mut idx = 0;
        let parts: Vec<&str> = content.split('\x00').collect();
        for part in parts {
            let trimmed = part.trim();
            if !trimmed.is_empty() {
                let ch_title =
                    extract_html_title(trimmed).unwrap_or_else(|| format!("章节 {}", idx + 1));
                chs.push(Chapter {
                    title: ch_title,
                    content: trimmed.to_string(),
                    index: idx,
                });
                idx += 1;
            }
        }
        if chs.is_empty() {
            vec![Chapter {
                title: "全文".to_string(),
                content,
                index: 0,
            }]
        } else {
            chs
        }
    };

    Ok(EpubBook {
        title,
        author: "未知作者".to_string(),
        chapters,
        cover_path: String::new(),
    })
}

/// 从MOBI提取文本内容（简单实现）
fn extract_mobi_text_content(bytes: &[u8]) -> String {
    if bytes.len() < 78 {
        return String::new();
    }

    let num_records =
        u16::from_be_bytes(<[u8; 2]>::try_from(&bytes[76..78]).unwrap_or([0, 0])) as usize;

    if num_records < 2 || bytes.len() < 78 + num_records * 8 {
        return String::new();
    }

    let mut parts = Vec::new();
    for i in 1..num_records {
        let off_start = 78 + i * 8;
        if off_start + 8 > bytes.len() {
            break;
        }

        let start = u32::from_be_bytes(
            bytes[off_start..off_start + 4]
                .try_into()
                .unwrap_or([0, 0, 0, 0]),
        ) as usize;
        let next_off = 78 + (i + 1) * 8;
        let end = if i + 1 < num_records && next_off + 4 <= bytes.len() {
            u32::from_be_bytes(
                bytes[next_off..next_off + 4]
                    .try_into()
                    .unwrap_or([0, 0, 0, 0]),
            ) as usize
        } else {
            bytes.len()
        };

        if start >= bytes.len() || end <= start {
            continue;
        }
        let end = end.min(bytes.len());

        let text = String::from_utf8_lossy(&bytes[start..end]);
        if text.contains('<') || !text.trim().is_empty() {
            parts.push(text.to_string());
        }
    }
    parts.join("\n")
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
