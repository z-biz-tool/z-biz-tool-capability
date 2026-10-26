//! 压 PDF 体积：把页面里的位图统一重画成 JPEG，顺手给能安全无损的流补一层 Flate。
//!
//! 只换图片流本身，正文、字体、页树都不碰。解码直接复用 extract_pdf_images 那条
//! 已经过真实语料验证的管线（/Decode、调色板、PNG 预测器、ICCBased 的坑不必再踩一遍）。
//!
//! 三条硬约束：
//! - 带透明通道的图一律不动 —— JPEG 没有 alpha，换上去等于把透明底变成白底；
//! - 整份文件重排后如果没变小，就直接输出原文件的副本 —— "压缩"绝不会交付一个更大的文件；
//! - 不用 lopdf 的 `Document::compress()`：它会对文档里**每一个**没有 /Filter 的流动手，
//!   而载入时所有流的 `allows_compression` 都是 true，于是内嵌字体的 FontFile2/FontFile3
//!   也会被套一层 Flate —— lopdf 自己的注释写着 "otherwise the font will be corrupt"。
//!   省体积不能以毁掉字形为代价，所以这里只碰"页面正文流 + 无滤镜无预测参数的图片流"。

use flate2::write::ZlibEncoder;
use image::{ColorType, DynamicImage};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;

/// 长边超过这个像素就缩：屏幕阅读 1600 足够，扫描页动辄 3000+
const DEFAULT_MAX_DIMENSION: u32 = 1600;
const DEFAULT_QUALITY: u8 = 70;

#[derive(Debug, Clone, Serialize)]
pub struct CompressReport {
    pub original_size: u64,
    pub new_size: u64,
    pub rewritten: usize,
    /// 无损补了 Flate 的流条数（正文流 + 未压缩的位图采样）
    pub flated: usize,
    /// 一条 = 一张保持原样的图；整份退回原样时额外追加 `DOC_REVERTED_NOTE` 那条说明。
    /// 前端 `src/utils/pdfCompress.ts` 按这条说明把"整份"和"单张"分开计数，改措辞要同步。
    pub skipped: Vec<String>,
}

/// 整份退回原样时写进 `skipped` 的说明，前端靠它区分"一张图没动"和"整份没赚"
pub(crate) const DOC_REVERTED_NOTE: &str = "已按原样输出副本";
pub fn compress_pdf_blocking(
    input_path: String,
    output_path: String,
    quality: Option<u8>,
    max_dimension: Option<u32>,
) -> Result<CompressReport, String> {
    let quality = quality.unwrap_or(DEFAULT_QUALITY).clamp(20, 95);
    let max_dimension = max_dimension
        .unwrap_or(DEFAULT_MAX_DIMENSION)
        .clamp(64, 8000);

    let file = crate::paths::input(&input_path)?;
    // 先过写保护再干活：敏感目录不该被创建，几百 MB 的重编码也不该白跑
    let dest = crate::paths::output(&output_path)?;
    if dest == file {
        // 原地覆盖会让"压不动就退回原样"没有退路（原文件已被写坏）
        return Err("输出不能覆盖输入，请换一个文件名".to_string());
    }
    let original_size = fs::metadata(&file).map(|meta| meta.len()).unwrap_or(0);
    let mut doc = crate::pdf::ops::load_doc(&file)?;

    // 先只读地收集（含 Form 里嵌套的图），再逐个改写：
    // 拿着 get_object_mut 的借用去解码别的对象会直接编译不过
    let mut targets: Vec<(String, ObjectId)> = Vec::new();
    for (index, page) in doc.get_pages().values().enumerate() {
        for (name, id) in crate::pdf::image::image_xobjects(&doc, *page) {
            targets.push((format!("第 {} 页 {}", index + 1, name), id));
        }
    }

    let mut report = CompressReport {
        original_size,
        new_size: 0,
        rewritten: 0,
        flated: 0,
        skipped: Vec::new(),
    };
    let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
    for (label, id) in targets {
        // 同一张图被多页共用时只处理一次
        if !seen.insert(id) {
            continue;
        }
        if let Err(reason) = rewrite_image(&mut doc, id, quality, max_dimension) {
            report.skipped.push(format!("{}：{}", label, reason));
        } else {
            report.rewritten += 1;
        }
    }

    // 换完图之后，把还能无损再挤一点的流挤一下
    report.flated = flate_safe_streams(&mut doc);
    // lopdf 载入时把对象流(ObjStm)展开成一条条独立对象，却把原对象流也留在表里；
    // 不剪掉输出里就同时存着两份
    doc.prune_objects();
    crate::pdf::ops::save_doc(doc, &output_path)?;
    report.new_size = fs::metadata(&dest).map(|meta| meta.len()).unwrap_or(0);
    if report.new_size >= original_size {
        // 重写一遍必然重排对象表（PDF 1.5 的交叉引用流/对象流会被摊平），
        // 图片省下的字节可能被全吃回去。与其交付一个"压缩后更大"的文件，
        // 不如原样给一份副本，并说清楚为什么。
        report.skipped.push(format!(
            "整体重排后反而变大（{} → {}），{}",
            original_size, report.new_size, DOC_REVERTED_NOTE
        ));
        fs::copy(&file, &dest).map_err(|e| format!("写回原样副本失败: {}", e))?;
        report.new_size = original_size;
        report.rewritten = 0;
        report.flated = 0;
    }
    Ok(report)
}

/// 只给"确定能无损还原"的流补 Flate：页面正文流，以及既没有 /Filter 也没有
/// /DecodeParms（预测器）的图片流。其余一律不碰，尤其是字体程序流。
fn flate_safe_streams(doc: &mut Document) -> usize {
    let mut ids: BTreeSet<ObjectId> = BTreeSet::new();
    for page in doc.get_pages().values().copied().collect::<Vec<_>>() {
        let contents = doc
            .get_object(page)
            .ok()
            .and_then(|object| object.as_dict().ok())
            .and_then(|dict| dict.get(b"Contents").ok())
            .cloned();
        match contents {
            Some(Object::Reference(id)) => {
                ids.insert(id);
            }
            Some(Object::Array(items)) => {
                for item in items {
                    if let Ok(id) = item.as_reference() {
                        ids.insert(id);
                    }
                }
            }
            _ => {}
        }
    }
    for (id, object) in doc.objects.iter() {
        if let Object::Stream(stream) = object {
            if stream
                .dict
                .get(b"Subtype")
                .ok()
                .and_then(|value| value.as_name().ok())
                == Some(b"Image".as_ref())
            {
                ids.insert(*id);
            }
        }
    }

    let mut done = 0;
    for id in ids {
        let Some(Object::Stream(stream)) = doc.objects.get_mut(&id) else {
            continue;
        };
        if stream.dict.get(b"Filter").is_ok()
            || stream.dict.get(b"DecodeParms").is_ok()
            || stream.dict.get(b"DP").is_ok()
        {
            continue;
        }
        let Ok(deflated) = deflate(&stream.content) else {
            continue;
        };
        // 和 lopdf 同一条经验规则：省不到 20 字节就别动，免得"压缩"反手把文件弄大
        if deflated.len() + 19 >= stream.content.len() {
            continue;
        }
        stream
            .dict
            .set("Filter", Object::Name(b"FlateDecode".to_vec()));
        stream.set_content(deflated);
        done += 1;
    }
    done
}

fn deflate(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(bytes).map_err(|e| e.to_string())?;
    encoder.finish().map_err(|e| e.to_string())
}

/// 成功返回 Ok(())，Err 一律表示"这张保持原样"并带上原因
fn rewrite_image(
    doc: &mut Document,
    id: ObjectId,
    quality: u8,
    max_dimension: u32,
) -> Result<(), String> {
    let (original_bytes, decoded) = {
        let stream = doc
            .get_object(id)
            .ok()
            .and_then(|object| object.as_stream().ok())
            .ok_or_else(|| "对象不是流".to_string())?;
        for key in [
            b"SMask".as_slice(),
            b"Mask".as_slice(),
            b"ImageMask".as_slice(),
        ] {
            if stream.dict.get(key).is_ok() {
                // 透明蒙板是独立对象，换掉底图后蒙板尺寸/内容都对不上了
                let mut name = String::from_utf8_lossy(key).to_string();
                if name == "ImageMask" {
                    name.push_str("（单色掩膜图）");
                }
                return Err(format!("带 /{}，换掉会丢透明度", name));
            }
        }
        (
            stream.content.len() as u64,
            crate::pdf::image::decode_image(doc, stream, 0)?,
        )
    };

    let image = match decoded {
        crate::pdf::image::Decoded::Image(image) => image,
        crate::pdf::image::Decoded::Passthrough { bytes, .. } => {
            // 已经是 JPEG：只有在"还能缩"的时候才值得重编码
            image::load_from_memory(&bytes).map_err(|_| "内嵌图无法二次解码".to_string())?
        }
    };
    if has_alpha(&image) {
        return Err("带透明通道，转 JPEG 会丢透明度".to_string());
    }
    let gray = matches!(image, DynamicImage::ImageLuma8(_));

    let longest = image.width().max(image.height());
    let scale = if longest > max_dimension {
        max_dimension as f32 / longest as f32
    } else {
        1.0
    };
    let target_width = ((image.width() as f32 * scale).round().max(1.0) as u32).min(image.width());
    let target_height =
        ((image.height() as f32 * scale).round().max(1.0) as u32).min(image.height());

    let encoded = if gray {
        let mut plane = image.to_luma8();
        if (plane.width(), plane.height()) != (target_width, target_height) {
            plane = image::imageops::resize(
                &plane,
                target_width,
                target_height,
                image::imageops::FilterType::Lanczos3,
            );
        }
        encode_jpeg(
            plane.as_raw(),
            target_width,
            target_height,
            ColorType::L8,
            quality,
        )?
    } else {
        let mut plane = image.to_rgb8();
        if (plane.width(), plane.height()) != (target_width, target_height) {
            plane = image::imageops::resize(
                &plane,
                target_width,
                target_height,
                image::imageops::FilterType::Lanczos3,
            );
        }
        encode_jpeg(
            plane.as_raw(),
            target_width,
            target_height,
            ColorType::Rgb8,
            quality,
        )?
    };

    if encoded.len() as u64 >= original_bytes {
        return Err("重编码后没有更小，已保留原图".to_string());
    }

    let mut dict = Dictionary::new();
    dict.set("Type", Object::Name(b"XObject".to_vec()));
    dict.set("Subtype", Object::Name(b"Image".to_vec()));
    dict.set("Width", Object::Integer(target_width as i64));
    dict.set("Height", Object::Integer(target_height as i64));
    dict.set("BitsPerComponent", Object::Integer(8));
    dict.set(
        "ColorSpace",
        Object::Name(if gray {
            b"DeviceGray".to_vec()
        } else {
            b"DeviceRGB".to_vec()
        }),
    );
    dict.set("Filter", Object::Name(b"DCTDecode".to_vec()));
    // /Decode 用缺省值即可：JPEG 的样本本身就是 0~255 的显示值
    let stream = Stream::new(dict, encoded);
    *doc.get_object_mut(id)
        .map_err(|_| "对象无法写入".to_string())? = Object::Stream(stream);
    Ok(())
}

fn encode_jpeg(
    samples: &[u8],
    width: u32,
    height: u32,
    color: ColorType,
    quality: u8,
) -> Result<Vec<u8>, String> {
    let mut buffer = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buffer, quality)
        .encode(samples, width, height, color.into())
        .map_err(|e| format!("JPEG 编码失败: {}", e))?;
    Ok(buffer)
}

fn has_alpha(image: &DynamicImage) -> bool {
    matches!(
        image,
        DynamicImage::ImageRgba8(_) | DynamicImage::ImageLumaA8(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn name(value: &str) -> Object {
        Object::Name(value.as_bytes().to_vec())
    }

    fn base_dict(width: i64, height: i64) -> Dictionary {
        let mut dict = Dictionary::new();
        dict.set("Type", name("XObject"));
        dict.set("Subtype", name("Image"));
        dict.set("Width", Object::Integer(width));
        dict.set("Height", Object::Integer(height));
        dict.set("BitsPerComponent", Object::Integer(8));
        dict.set("ColorSpace", name("DeviceRGB"));
        dict
    }

    /// 平滑渐变：既压得动（能验证"确实变小"），又和原图逐点可比（能验证"没压坏"）
    fn gradient(width: u32, height: u32) -> Vec<u8> {
        let mut data = Vec::with_capacity((width * height * 3) as usize);
        for y in 0..height {
            for x in 0..width {
                let r = if width > 1 {
                    (x * 255 / (width - 1)) as u8
                } else {
                    0
                };
                let g = if height > 1 {
                    (y * 255 / (height - 1)) as u8
                } else {
                    0
                };
                data.extend_from_slice(&[r, g, 128]);
            }
        }
        data
    }

    fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
        assert_eq!(a.len(), b.len(), "两张图尺寸不一致，没法逐点比");
        a.iter()
            .zip(b)
            .map(|(x, y)| x.abs_diff(*y) as f64)
            .sum::<f64>()
            / a.len() as f64
    }

    struct Fixture {
        doc: Document,
        images: Vec<(String, ObjectId)>,
        fonts: Vec<(String, ObjectId)>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                doc: Document::new(),
                images: Vec::new(),
                fonts: Vec::new(),
            }
        }

        /// 建一条完整的字体链 Font → FontDescriptor → FontFile2，并挂进页面 Resources。
        /// 必须可达，否则 `prune_objects` 会先替我们把字体流删掉，测试就成了空跑。
        fn embed_font(&mut self, program: Vec<u8>) -> ObjectId {
            let mut file = Dictionary::new();
            file.set("Length1", Object::Integer(program.len() as i64));
            let mut descriptor = Dictionary::new();
            descriptor.set("Type", name("FontDescriptor"));
            descriptor.set("FontName", name("Fake-Regular"));
            descriptor.set("FontFile2", Object::Reference(self.stream(file, program)));
            let descriptor = self.doc.add_object(Object::Dictionary(descriptor));
            let mut font = Dictionary::new();
            font.set("Type", name("Font"));
            font.set("Subtype", name("Type1"));
            font.set("BaseFont", name("Fake-Regular"));
            font.set("FontDescriptor", Object::Reference(descriptor));
            let font = self.doc.add_object(Object::Dictionary(font));
            self.fonts.push(("F1".to_string(), font));
            font
        }

        /// 只建对象、不挂到页面 Resources 上：/SMask 这类图本来就是从图片字典引用的
        fn stream(&mut self, dict: Dictionary, content: Vec<u8>) -> ObjectId {
            self.doc
                .add_object(Object::Stream(Stream::new(dict, content)))
        }

        fn image(&mut self, key: &str, dict: Dictionary, content: Vec<u8>) -> ObjectId {
            let id = self.stream(dict, content);
            self.images.push((key.to_string(), id));
            id
        }

        /// pages 张页面共用同一份 Resources，用来覆盖"同一张图被多页引用只处理一次"
        fn save(&mut self, dir: &Path, file_name: &str, pages: usize) -> PathBuf {
            let mut xobjects = Dictionary::new();
            for (key, id) in &self.images {
                xobjects.set(key.as_bytes().to_vec(), Object::Reference(*id));
            }
            let mut fonts = Dictionary::new();
            for (key, id) in &self.fonts {
                fonts.set(key.as_bytes().to_vec(), Object::Reference(*id));
            }
            let mut resources = Dictionary::new();
            resources.set("XObject", Object::Dictionary(xobjects));
            resources.set("Font", Object::Dictionary(fonts));
            let resources = self.doc.add_object(Object::Dictionary(resources));
            let mut kids = Vec::new();
            for index in 0..pages {
                let body = format!("q 100 0 0 100 {} 0 cm /Im0 Do Q", index * 10);
                let content = self.doc.add_object(Object::Stream(Stream::new(
                    Dictionary::new(),
                    body.into_bytes(),
                )));
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
                kids.push(Object::Reference(
                    self.doc.add_object(Object::Dictionary(dict)),
                ));
            }
            let mut pages_dict = Dictionary::new();
            pages_dict.set("Type", name("Pages"));
            pages_dict.set("Kids", Object::Array(kids));
            pages_dict.set("Count", Object::Integer(pages as i64));
            let pages_id = self.doc.add_object(Object::Dictionary(pages_dict));
            let mut catalog = Dictionary::new();
            catalog.set("Type", name("Catalog"));
            catalog.set("Pages", Object::Reference(pages_id));
            let catalog = self.doc.add_object(Object::Dictionary(catalog));
            self.doc.trailer.set("Root", Object::Reference(catalog));
            let path = dir.join(file_name);
            self.doc.save(&path).unwrap();
            path
        }
    }

    fn compress(input: &Path, output: &Path, quality: u8, max_dimension: u32) -> CompressReport {
        compress_pdf_blocking(
            input.to_string_lossy().to_string(),
            output.to_string_lossy().to_string(),
            Some(quality),
            Some(max_dimension),
        )
        .unwrap()
    }

    fn int_of(dict: &Dictionary, key: &[u8]) -> Option<i64> {
        match dict.get(key).ok()? {
            Object::Integer(value) => Some(*value),
            Object::Real(value) => Some(*value as i64),
            _ => None,
        }
    }

    fn name_of(dict: &Dictionary, key: &[u8]) -> String {
        dict.get(key)
            .ok()
            .and_then(|object| object.as_name().ok())
            .map(|value| String::from_utf8_lossy(value).to_string())
            .unwrap_or_default()
    }

    fn filter_of(stream: &Stream) -> String {
        name_of(&stream.dict, b"Filter")
    }

    /// 输出文档里所有图片流（对象号在 save/load 之间不变，所以数量本身也是断言）
    fn image_streams(path: &Path) -> Vec<(Stream, Vec<u8>)> {
        let doc = crate::pdf::ops::load_doc(path).unwrap();
        doc.objects
            .values()
            .filter_map(|object| object.as_stream().ok())
            .filter(|stream| {
                stream
                    .dict
                    .get(b"Subtype")
                    .ok()
                    .and_then(|value| value.as_name().ok())
                    .map(|value| value == b"Image")
                    .unwrap_or(false)
            })
            .map(|stream| {
                let bytes = stream.content.clone();
                (stream.clone(), bytes)
            })
            .collect()
    }

    #[test]
    fn uncompressed_rgb_image_becomes_a_smaller_jpeg_that_still_matches_the_pixels() {
        let dir = crate::test_support::TempDir::new("cmp-rgb");
        let mut fixture = Fixture::new();
        let samples = gradient(48, 32);
        fixture.image("Im0", base_dict(48, 32), samples.clone());
        let input = fixture.save(&dir, "in.pdf", 1);

        let report = compress(&input, &dir.join("out.pdf"), 70, DEFAULT_MAX_DIMENSION);
        assert_eq!(report.rewritten, 1, "{:?}", report.skipped);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(report.original_size, fs::metadata(&input).unwrap().len());
        assert!(
            report.new_size < report.original_size,
            "没有变小: {} >= {}",
            report.new_size,
            report.original_size
        );

        let streams = image_streams(&dir.join("out.pdf"));
        assert_eq!(streams.len(), 1);
        let (stream, bytes) = &streams[0];
        assert_eq!(filter_of(stream), "DCTDecode");
        assert_eq!(int_of(&stream.dict, b"Width"), Some(48));
        assert_eq!(int_of(&stream.dict, b"Height"), Some(32));
        assert_eq!(int_of(&stream.dict, b"BitsPerComponent"), Some(8));
        assert!(
            stream.dict.get(b"Decode").is_err(),
            "/Decode 必须换掉，否则样本被映射两次"
        );
        // 换上去的必须真是一张解得开、且画的就是原图的 JPEG
        let decoded = image::load_from_memory(bytes).expect("换上去的不是合法 JPEG");
        assert_eq!((decoded.width(), decoded.height()), (48, 32));
        let back = decoded.to_rgb8();
        assert!(
            mean_abs_diff(back.as_raw(), &samples) < 6.0,
            "JPEG  round trip 后偏色过大"
        );
    }

    #[test]
    fn images_carrying_transparency_are_never_touched() {
        let dir = crate::test_support::TempDir::new("cmp-alpha");
        let mut fixture = Fixture::new();
        let mut mask_dict = base_dict(4, 4);
        mask_dict.remove(b"ColorSpace");
        mask_dict.set("ColorSpace", name("DeviceGray"));
        let mask = fixture.stream(mask_dict, vec![255u8, 128, 64, 0]);
        let mut rgb = base_dict(4, 4);
        rgb.set("SMask", Object::Reference(mask));
        let content = gradient(4, 4);
        fixture.image("Im0", rgb, content.clone());
        let input = fixture.save(&dir, "in.pdf", 1);

        let report = compress(&input, &dir.join("out.pdf"), 70, DEFAULT_MAX_DIMENSION);
        assert_eq!(report.rewritten, 0);
        assert!(
            report
                .skipped
                .iter()
                .any(|r| r.contains("SMask") && r.contains("Im0")),
            "原因里要带上是哪张图: {:?}",
            report.skipped
        );

        let streams = image_streams(&dir.join("out.pdf"));
        assert_eq!(streams.len(), 2);
        assert!(
            streams
                .iter()
                .all(|(stream, _)| filter_of(stream) != "DCTDecode"),
            "带透明通道的图不该被换成 JPEG"
        );
        assert!(streams.iter().any(|(_, bytes)| *bytes == content));
    }

    #[test]
    fn stencil_masks_are_skipped_by_name() {
        let dir = crate::test_support::TempDir::new("cmp-stencil");
        let mut fixture = Fixture::new();
        let mut dict = Dictionary::new();
        dict.set("Type", name("XObject"));
        dict.set("Subtype", name("Image"));
        dict.set("Width", Object::Integer(8));
        dict.set("Height", Object::Integer(8));
        dict.set("ImageMask", Object::Boolean(true));
        dict.set("BitsPerComponent", Object::Integer(1));
        fixture.image("Im0", dict, vec![0b1010_1010; 8]);
        let input = fixture.save(&dir, "in.pdf", 1);

        let report = compress(&input, &dir.join("out.pdf"), 70, DEFAULT_MAX_DIMENSION);
        assert_eq!(report.rewritten, 0);
        assert!(
            report.skipped[0].contains("ImageMask"),
            "{:?}",
            report.skipped
        );
    }

    #[test]
    fn reencode_that_would_grow_keeps_the_original_bytes() {
        let dir = crate::test_support::TempDir::new("cmp-grow");
        let mut fixture = Fixture::new();
        let content = gradient(2, 2);
        assert!(content.len() < 20, "fixture 得小到 JPEG 一定压不过它");
        fixture.image("Im0", base_dict(2, 2), content.clone());
        let input = fixture.save(&dir, "in.pdf", 1);

        let report = compress(&input, &dir.join("out.pdf"), 70, DEFAULT_MAX_DIMENSION);
        assert_eq!(report.rewritten, 0);
        assert!(
            report.skipped[0].contains("没有更小"),
            "{:?}",
            report.skipped
        );
        let streams = image_streams(&dir.join("out.pdf"));
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].1, content, "不该变小却换了图，等于白损一次画质");
    }

    #[test]
    fn only_the_long_edge_is_capped_and_small_images_are_never_upscaled() {
        let dir = crate::test_support::TempDir::new("cmp-scale");
        let mut fixture = Fixture::new();
        fixture.image("Im0", base_dict(240, 120), gradient(240, 120));
        let big = fixture.save(&dir, "big.pdf", 1);
        let report = compress(&big, &dir.join("big-out.pdf"), 70, 120);
        assert_eq!(report.rewritten, 1, "{:?}", report.skipped);
        let streams = image_streams(&dir.join("big-out.pdf"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            (
                int_of(&streams[0].0.dict, b"Width"),
                int_of(&streams[0].0.dict, b"Height")
            ),
            (Some(120), Some(60)),
            "长边要正好压到上限且保持比例"
        );

        // 上限给得离谱（30）时被 64 的下限兜住，不至于把图缩成一团马赛克
        let report = compress(&big, &dir.join("floor-out.pdf"), 70, 30);
        assert_eq!(report.rewritten, 1, "{:?}", report.skipped);
        let streams = image_streams(&dir.join("floor-out.pdf"));
        assert_eq!(int_of(&streams[0].0.dict, b"Width"), Some(64));

        // 同一个上限放到 4000：本来就不该重采样，更不该放大
        let mut fixture = Fixture::new();
        fixture.image("Im0", base_dict(60, 40), gradient(60, 40));
        let small = fixture.save(&dir, "small.pdf", 1);
        let report = compress(&small, &dir.join("small-out.pdf"), 90, 4000);
        assert_eq!(report.rewritten, 1, "{:?}", report.skipped);
        let streams = image_streams(&dir.join("small-out.pdf"));
        assert_eq!(
            (
                int_of(&streams[0].0.dict, b"Width"),
                int_of(&streams[0].0.dict, b"Height")
            ),
            (Some(60), Some(40)),
            "小图被放大了"
        );
    }

    #[test]
    fn an_image_shared_by_two_pages_is_rewritten_once() {
        let dir = crate::test_support::TempDir::new("cmp-shared");
        let mut fixture = Fixture::new();
        fixture.image("Im0", base_dict(64, 64), gradient(64, 64));
        let input = fixture.save(&dir, "in.pdf", 2);

        let report = compress(&input, &dir.join("out.pdf"), 70, DEFAULT_MAX_DIMENSION);
        assert_eq!(report.rewritten, 1, "共用对象被重复处理了: {:?}", report);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(image_streams(&dir.join("out.pdf")).len(), 1);
    }

    #[test]
    fn has_alpha_only_matches_images_that_actually_carry_transparency() {
        let rgb = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2, 2, image::Rgb([9, 9, 9])));
        let gray = DynamicImage::ImageLuma8(image::GrayImage::from_pixel(2, 2, image::Luma([9])));
        let rgba = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            2,
            2,
            image::Rgba([9, 9, 9, 0]),
        ));
        let luma_a =
            DynamicImage::ImageLumaA8(image::ImageBuffer::from_pixel(2, 2, image::LumaA([9, 0])));
        assert!(!has_alpha(&rgb));
        assert!(!has_alpha(&gray));
        assert!(has_alpha(&rgba));
        assert!(has_alpha(&luma_a));
    }

    /// lopdf 的 `Document::compress()` 会连字体程序流一起套 Flate（载入时
    /// `allows_compression` 全是 true），这里必须只碰正文和图片采样。
    #[test]
    fn lossless_flate_covers_images_and_body_but_never_font_programs() {
        let dir = crate::test_support::TempDir::new("cmp-flate");
        let mut fixture = Fixture::new();
        let mut mask_dict = base_dict(40, 40);
        mask_dict.remove(b"ColorSpace");
        mask_dict.set("ColorSpace", name("DeviceGray"));
        let mask = fixture.stream(mask_dict, vec![200u8; 1_600]);
        let mut rgb = base_dict(40, 40);
        rgb.set("SMask", Object::Reference(mask));
        fixture.image("Im0", rgb, vec![7u8; 4_800]);
        // 字体程序流：形态上就是一段没有 /Filter 的流，最容易被顺手压坏
        fixture.embed_font(vec![b'Q'; 5_000]);
        let input = fixture.save(&dir, "in.pdf", 1);

        let report = compress(&input, &dir.join("out.pdf"), 70, DEFAULT_MAX_DIMENSION);
        // 带透明的图不能转 JPEG，但它的采样仍可无损挤 —— 两条约束互不干扰
        assert_eq!(report.rewritten, 0, "{:?}", report);
        assert!(report.skipped[0].contains("SMask"), "{:?}", report.skipped);
        assert_eq!(report.flated, 2, "应正好是图片 + 掩码两条流: {:?}", report);

        // 字体流是可达对象了，剪枝不会替我们删掉它 —— 剩下的就得靠被测代码自己躲开
        let out = crate::pdf::ops::load_doc(&dir.join("out.pdf")).unwrap();
        let font_id = out
            .objects
            .iter()
            .find_map(|(id, object)| match object {
                Object::Stream(stream) if stream.content.len() == 5_000 => Some(*id),
                _ => None,
            })
            .expect("字体程序流不该被剪枝删掉");
        let stream = out.get_object(font_id).unwrap().as_stream().unwrap();
        assert!(
            stream.dict.get(b"Filter").is_err() && stream.content == vec![b'Q'; 5_000],
            "字体程序流被套了一层 Flate，字形就废了"
        );
        // 按 ColorSpace 挑底图（掩码是 DeviceGray），别用 /SMask 自身去挑，那才是被测的东西
        let picture = image_streams(&dir.join("out.pdf"))
            .into_iter()
            .find(|(stream, _)| name_of(&stream.dict, b"ColorSpace") == "DeviceRGB")
            .expect("底图流应在输出里");
        assert_eq!(filter_of(&picture.0), "FlateDecode");
        assert!(picture.0.dict.get(b"SMask").is_ok(), "/SMask 引用不能丢");
        use std::io::Read;
        let mut back = Vec::new();
        flate2::read::ZlibDecoder::new(&picture.1[..])
            .read_to_end(&mut back)
            .unwrap();
        assert_eq!(back, vec![7u8; 4_800], "无损压缩必须真的无损");
    }

    /// 小图 + 小正文：单图压不动、整体重排还反涨几字节 —— 正好走"原样副本"这条退路。
    /// 真实语料里 9 份有 3 份就是这样（PDF 1.5 的交叉引用流被摊平），
    /// 少了这道闸，"压缩"会交出比原文件更大的产物。
    #[test]
    fn output_is_never_fatter_than_the_input() {
        let dir = crate::test_support::TempDir::new("cmp-fallback");
        let mut fixture = Fixture::new();
        // 2x2 随机样：既压不出 JPEG，也压不动 Flate
        fixture.image(
            "Im0",
            base_dict(2, 2),
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
        );
        let input = fixture.save(&dir, "in.pdf", 1);
        let output = dir.join("out.pdf");

        let report = compress(&input, &output, 70, DEFAULT_MAX_DIMENSION);
        assert!(
            report.new_size <= report.original_size,
            "{} -> {}",
            report.original_size,
            report.new_size
        );
        assert_eq!(report.rewritten, 0, "退回原样之后不该再声称换过图");
        assert_eq!(report.flated, 0);
        // 前端 src/utils/pdfCompress.ts 用这个子串把"整份退回"从"单张保留"里挑出来，
        // 措辞一改就会静默算错张数，所以这里钉的是常量本身而不是随手写的片段
        assert!(
            report.skipped.iter().any(|r| r.contains(DOC_REVERTED_NOTE)),
            "{:?}",
            report.skipped
        );
        assert!(
            report
                .skipped
                .iter()
                .all(|r| r.contains(DOC_REVERTED_NOTE) || r.starts_with("第 ")),
            "除整份说明外的每条都应是一张图的保留原因：{:?}",
            report.skipped
        );
        assert_eq!(
            fs::read(&output).unwrap(),
            fs::read(&input).unwrap(),
            "退路必须是逐字节相同的副本"
        );
    }

    #[test]
    fn output_may_not_overwrite_the_input() {
        let dir = crate::test_support::TempDir::new("cmp-self");
        let mut fixture = Fixture::new();
        fixture.image("Im0", base_dict(64, 64), gradient(64, 64));
        let input = fixture.save(&dir, "in.pdf", 1);
        let before = fs::read(&input).unwrap();
        let err = compress_pdf_blocking(
            input.to_string_lossy().to_string(),
            input.to_string_lossy().to_string(),
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("不能覆盖"), "{}", err);
        assert_eq!(
            fs::read(&input).unwrap(),
            before,
            "被拒绝的调用不该动原文件"
        );
    }
}
