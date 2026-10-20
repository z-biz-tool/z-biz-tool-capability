//! 从 file 侧搬运时一起过来的行为测试。
//! path_guard 相关用例（黑名单源/黑名单目标/相对路径拒绝）留在 file 侧——
//! 那是调用方的安全策略，不在本 crate 职责内（lib.rs 顶部注释）。

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

static SEQ: AtomicU32 = AtomicU32::new(0);

/// 每个用例独享一个目录，作用域结束自动删除
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "cap-img-{}-{}-{}-{}",
            name,
            std::process::id(),
            nanos,
            seq
        ));
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn join(&self, rel: &str) -> PathBuf {
        let p = self.0.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        p
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 4x4 纯色 PNG，够小且 image crate 能真的解码
fn png(dir: &TempDir, rel: &str) -> PathBuf {
    let p = dir.join(rel);
    image::DynamicImage::new_rgb8(4, 4).save(&p).unwrap();
    p
}

/// 2x3 六色图（每格不同色，行优先）：几何断言靠它区分「转/翻对了」与「转/翻错了」。
///
/// 2x3 是刻意选的非方形尺寸 —— 正方形图上 rotate 90 与 rotate 270 的结果一样，
/// 断言就区分不出来了。
fn six_color(dir: &TempDir, rel: &str) -> PathBuf {
    let p = dir.join(rel);
    let mut img = image::RgbImage::new(2, 3);
    let colors = [
        image::Rgb([255, 0, 0]),   // (0,0) 红
        image::Rgb([0, 255, 0]),   // (1,0) 绿
        image::Rgb([0, 0, 255]),   // (0,1) 蓝
        image::Rgb([255, 255, 0]), // (1,1) 黄
        image::Rgb([0, 255, 255]), // (0,2) 青
        image::Rgb([255, 0, 255]), // (1,2) 品红
    ];
    for (i, px) in img.pixels_mut().enumerate() {
        *px = colors[i];
    }
    image::DynamicImage::ImageRgb8(img).save(&p).unwrap();
    p
}

/// 读回一张图并取某点像素（RGB 三元组）
fn pixel_at(path: &std::path::Path, x: u32, y: u32) -> [u8; 3] {
    let img = image::open(path).unwrap().to_rgb8();
    let p = img.get_pixel(x, y);
    [p[0], p[1], p[2]]
}

/// 读回一张图的尺寸
fn dims_of(path: &std::path::Path) -> (u32, u32) {
    let img = image::open(path).unwrap();
    (img.width(), img.height())
}

/// 读文件头几个字节（判魔数用）
fn magic_of(path: &std::path::Path) -> Vec<u8> {
    fs::read(path).unwrap().into_iter().take(4).collect()
}

const PNG_MAGIC: [u8; 4] = [0x89, b'P', b'N', b'G'];
const JPEG_MAGIC: [u8; 2] = [0xFF, 0xD8];

/// 在一张真 JPEG 的段链里插一段 APP1/EXIF。
/// 必须基于真图：`info` 会先用 image crate 解码，手搓的假 SOI 直接
/// 在解码那步就报错，永远走不到 EXIF 分支。
fn jpeg_with_exif(dir: &TempDir, rel: &str) -> PathBuf {
    jpeg_with_tiff(dir, rel, &tiff_with_model())
}

/// 造一段 TIFF 体：IFD0 一条记录 Model(0x0110) -> 偏移 26 处的 "TEST"
fn tiff_with_model() -> Vec<u8> {
    let mut tiff: Vec<u8> = Vec::new();
    tiff.extend_from_slice(b"MM"); // 大端
    tiff.extend_from_slice(&0x002Au16.to_be_bytes());
    tiff.extend_from_slice(&8u32.to_be_bytes()); // IFD0 从偏移 8 开始
    tiff.extend_from_slice(&1u16.to_be_bytes()); // 1 条记录
    tiff.extend_from_slice(&0x0110u16.to_be_bytes()); // tag = Model
    tiff.extend_from_slice(&2u16.to_be_bytes()); // type = ASCII
    tiff.extend_from_slice(&5u32.to_be_bytes()); // count
    tiff.extend_from_slice(&26u32.to_be_bytes()); // 值所在偏移
    tiff.extend_from_slice(&0u32.to_be_bytes()); // 下一个 IFD = 无
    tiff.extend_from_slice(b"TEST\0");
    while tiff.len() < 40 {
        tiff.push(0);
    }
    tiff
}

/// 在一张真 JPEG 里插一段给定的 TIFF 体（EXIF 段）
fn jpeg_with_tiff(dir: &TempDir, rel: &str, tiff: &[u8]) -> PathBuf {
    let base_path = dir.join("base.jpg");
    image::DynamicImage::new_rgb8(4, 4)
        .save(&base_path)
        .unwrap();
    let base = fs::read(&base_path).unwrap();

    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(b"Exif\0\0");
    body.extend_from_slice(tiff);
    // 段长把自身两字节算在内
    let seg = (body.len() + 2) as u16;

    // 跳过编码器自带的 APP0/APPn，插到它们之后、SOF 之前
    let mut pos = 2;
    while pos + 4 <= base.len() && base[pos] == 0xFF && (0xE0..=0xEF).contains(&base[pos + 1]) {
        let l = u16::from_be_bytes([base[pos + 2], base[pos + 3]]) as usize;
        pos += 2 + l;
    }
    let mut out = base[..pos].to_vec();
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&seg.to_be_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(&base[pos..]);

    let p = dir.join(rel);
    fs::write(&p, out).unwrap();
    p
}

#[test]
fn info_still_reads_an_ordinary_file() {
    let dir = TempDir::new("ok");
    let src = png(&dir, "a.png");
    let info = cap_img::info(src.to_string_lossy().as_ref()).unwrap();
    assert_eq!((info.width, info.height), (4, 4));
    assert_eq!(info.format, "png");
    // size 与 has_alpha 也是对外契约的一部分（file 侧属性面板用），
    // 只验宽高与格式的话，把 size 恒填 0、把 has_alpha 恒填 false 都能过。
    assert_eq!(
        info.size,
        fs::metadata(&src).unwrap().len(),
        "size 必须是文件真实字节数"
    );
    assert!(!info.has_alpha, "纯 RGB 的 PNG 没有 alpha 通道");
    assert!(info.exif.is_none(), "PNG 不该有 EXIF");
}

/// 带 alpha 通道的 PNG 必须被认出来（has_alpha 恒 false 会让前端误判透明度）。
#[test]
fn info_detects_alpha_channel() {
    let dir = TempDir::new("alpha");
    let src = dir.join("a.png");
    let mut img = image::RgbaImage::new(4, 4);
    for p in img.pixels_mut() {
        *p = image::Rgba([1, 2, 3, 4]);
    }
    image::DynamicImage::ImageRgba8(img).save(&src).unwrap();
    let info = cap_img::info(src.to_string_lossy().as_ref()).unwrap();
    assert!(info.has_alpha, "RGBA 的 PNG 必须报 has_alpha=true");
}

#[test]
fn info_actually_reads_exif_from_a_jpeg() {
    let dir = TempDir::new("exif");
    let src = jpeg_with_exif(&dir, "a.jpg");
    let info = cap_img::info(src.to_string_lossy().as_ref()).unwrap();
    let exif = info.exif.expect("JPEG 里的 EXIF 必须被读出来");
    assert_eq!(exif.get("Model").map(|s| s.as_str()), Some("TEST"));
}

/// EXIF 的 IFD0 偏移越界时必须**返回空 EXIF**，不能 panic。
///
/// 回归锚点：EXIF 段里的偏移量是**图片文件里的字节**，任何人都能手搓一个 JPEG
/// 把它指到段尾。原实现只判 `ifd0_offset >= data.len()`，于是 offset == len-1 时
/// `data[offset..offset+2]` 切片越界 panic —— 一个畸形图片就能让 `info()` 崩掉，
/// 而调用方（file 仓的 get_image_info）只当它是个坏文件。
#[test]
fn info_survives_exif_with_out_of_range_ifd_offset() {
    let dir = TempDir::new("exif-oob");
    // TIFF 体：MM + 0x002A + IFD0偏移(=8) + 1 字节 ⇒ 长度 9
    // offset(8) < len(9) 通过了原来的检查，但 offset+2 = 10 > 9 ⇒ 越界
    let mut tiff: Vec<u8> = Vec::new();
    tiff.extend_from_slice(b"MM");
    tiff.extend_from_slice(&0x002Au16.to_be_bytes());
    tiff.extend_from_slice(&8u32.to_be_bytes());
    tiff.resize(9, 0);

    let src = jpeg_with_tiff(&dir, "a.jpg", &tiff);
    // 不 panic 本身就是断言：越界切片会直接把测试线程带崩。
    let info = cap_img::info(src.to_string_lossy().as_ref()).unwrap();
    let exif = info.exif.expect("应返回 Some（只是内容为空）");
    assert!(exif.is_empty(), "越界 EXIF 应读成空表，实得 {:?}", exif);
}

#[test]
fn thumbnail_returns_base64_png_within_bounds() {
    let dir = TempDir::new("thumb");
    let src = png(&dir, "a.png");
    let t = cap_img::thumbnail(src.to_string_lossy().as_ref(), 2).unwrap();
    assert!(t.width <= 2 && t.height <= 2);
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let raw = STANDARD.decode(&t.data).unwrap();
    // PNG 魔数
    assert_eq!(&raw[..4], &[0x89, b'P', b'N', b'G']);

    // 光"不超过 2"钉不住"真的缩到了 2"：实现成返回 1x1 或 0x0 也照样过。
    // 而且 width/height 字段必须与**实际解码出来的图**一致 ——
    // 字段与内容对不上时，前端按字段排版就会排出错位的图。
    let img = image::load_from_memory(&raw).unwrap();
    assert_eq!((img.width(), img.height()), (t.width, t.height));
    assert_eq!((t.width, t.height), (2, 2), "4x4 缩到 max_size=2 应得 2x2");
}

/// thumbnail 的 max_size=0 语义：退到默认上限，而不是塌成 0x0。
#[test]
fn thumbnail_zero_max_size_falls_back_to_default() {
    let dir = TempDir::new("thumb0");
    let src = dir.join("a.png");
    image::DynamicImage::new_rgb8(400, 400).save(&src).unwrap();
    let t = cap_img::thumbnail(src.to_string_lossy().as_ref(), 0).unwrap();
    assert_eq!((t.width, t.height), (200, 200));
}

#[test]
fn resize_still_writes_and_creates_parent() {
    let dir = TempDir::new("resize-ok");
    let src = png(&dir, "a.png");
    let dest = dir.join("out").join("small.png");
    cap_img::resize(
        src.to_string_lossy().as_ref(),
        dest.to_string_lossy().as_ref(),
        2,
        2,
    )
    .unwrap();
    assert!(dest.exists());
    assert!(fs::metadata(&dest).unwrap().len() > 0);
    // 「文件存在且非空」钉不住 resize 真的缩放了：把实现改成原样拷贝也能过。
    // 必须断言**结果图的尺寸**。
    assert_eq!(dims_of(&dest), (2, 2));
}

/// resize 的尺寸语义：保持宽高比，**缩放系数取两边的较小者**（不裁切、不填充）。
///
/// 真实语义来自 image crate 的 `resize_dimensions(w, h, nw, nh, fill=false)`：
/// `ratio = min(nw/w, nh/h)`，再按 ratio 等比缩放。读源码确认过，不是猜的：
/// fill=true 时取 max（那才是"裁切填满"），fill=false 取 min。
///
/// 源 2x3 塞进 4x4：ratio = min(4/2, 4/3) = 1.333 ⇒ 3x4（**不是** 4x4，
/// 也不是 4x6 —— 后两个是按"填满"或"塞进"猜出来的，都不对）。
/// 这条断言的作用就是钉住"取 min"这件事：改成取 max 就得红。
#[test]
fn resize_keeps_aspect_ratio_scaling_by_the_smaller_ratio() {
    let dir = TempDir::new("resize-aspect");
    let src = six_color(&dir, "a.png");
    let s = src.to_string_lossy().to_string();

    // 2x3 → 框 4x4 ⇒ ratio=min(2, 4/3)=4/3 ⇒ 3x4
    let d1 = dir.join("box4x4.png");
    cap_img::resize(&s, d1.to_string_lossy().as_ref(), 4, 4).unwrap();
    assert_eq!(dims_of(&d1), (3, 4), "2x3 按 min 比例缩到 4x4 框应得 3x4");

    // 2x3 → 框 100x2 ⇒ ratio=min(50, 2/3)=2/3 ⇒ 1x2
    let d2 = dir.join("box100x2.png");
    cap_img::resize(&s, d2.to_string_lossy().as_ref(), 100, 2).unwrap();
    assert_eq!(dims_of(&d2), (1, 2), "2x3 按 min 比例缩到 100x2 框应得 1x2");

    // 2x3 → 框 2x2 ⇒ ratio=min(1, 2/3)=2/3 ⇒ 1x2
    let d3 = dir.join("box2x2.png");
    cap_img::resize(&s, d3.to_string_lossy().as_ref(), 2, 2).unwrap();
    assert_eq!(dims_of(&d3), (1, 2));
}

/// rotate 的方向必须被钉住：90/180/270 三者互不相同。
///
/// 用 2x3 非方形 + 六色图：正方形图上 90 与 270 结果相同，断言区分不出来。
/// 期望值由 image crate 的 rotate90/180/270 定义给出，并逐点核对，
/// 而不是只断言"宽高变了"。
#[test]
fn rotate_turns_the_right_way() {
    let dir = TempDir::new("rotate-dir");
    let src = six_color(&dir, "a.png");
    let s = src.to_string_lossy().to_string();

    // 源布局（行优先，2 宽 3 高）：
    //   (0,0)红 (1,0)绿
    //   (0,1)蓝 (1,1)黄
    //   (0,2)青 (1,2)品红
    //
    // rotate90 = 顺时针 90° ⇒ 结果 3 宽 2 高，左上应是原左下(青)
    let d90 = dir.join("r90.png");
    cap_img::rotate(&s, d90.to_string_lossy().as_ref(), 90).unwrap();
    assert_eq!(dims_of(&d90), (3, 2));
    assert_eq!(
        pixel_at(&d90, 0, 0),
        [0, 255, 255],
        "rotate90 左上应是原左下(青)"
    );
    assert_eq!(
        pixel_at(&d90, 2, 0),
        [255, 0, 0],
        "rotate90 右上应是原左上(红)"
    );

    // rotate180 ⇒ 结果仍 2x3，左上应是原右下(品红)
    let d180 = dir.join("r180.png");
    cap_img::rotate(&s, d180.to_string_lossy().as_ref(), 180).unwrap();
    assert_eq!(dims_of(&d180), (2, 3));
    assert_eq!(
        pixel_at(&d180, 0, 0),
        [255, 0, 255],
        "rotate180 左上应是原右下(品红)"
    );

    // rotate270 = 逆时针 90° ⇒ 结果 3x2，左上应是原右上(绿)
    let d270 = dir.join("r270.png");
    cap_img::rotate(&s, d270.to_string_lossy().as_ref(), 270).unwrap();
    assert_eq!(dims_of(&d270), (3, 2));
    assert_eq!(
        pixel_at(&d270, 0, 0),
        [0, 255, 0],
        "rotate270 左上应是原右上(绿)"
    );
}

/// flip 的横/竖必须分清：把两个参数写反了，"成功"照样成立。
#[test]
fn flip_distinguishes_horizontal_and_vertical() {
    let dir = TempDir::new("flip-dir");
    let src = six_color(&dir, "a.png");
    let s = src.to_string_lossy().to_string();

    // 水平翻转 ⇒ 左上变成原右上(绿)
    let dh = dir.join("fh.png");
    cap_img::flip(&s, dh.to_string_lossy().as_ref(), true).unwrap();
    assert_eq!(dims_of(&dh), (2, 3));
    assert_eq!(
        pixel_at(&dh, 0, 0),
        [0, 255, 0],
        "水平翻转左上应是原右上(绿)"
    );

    // 垂直翻转 ⇒ 左上变成原左下(青)
    let dv = dir.join("fv.png");
    cap_img::flip(&s, dv.to_string_lossy().as_ref(), false).unwrap();
    assert_eq!(dims_of(&dv), (2, 3));
    assert_eq!(
        pixel_at(&dv, 0, 0),
        [0, 255, 255],
        "垂直翻转左上应是原左下(青)"
    );
}

/// crop 必须真的裁到指定区域，而不是裁个别的什么。
#[test]
fn crop_takes_the_requested_region() {
    let dir = TempDir::new("crop-region");
    let src = six_color(&dir, "a.png");
    let s = src.to_string_lossy().to_string();

    // 裁 (1,1) 起 1x1 ⇒ 唯一那格是黄
    let d = dir.join("c.png");
    cap_img::crop(&s, d.to_string_lossy().as_ref(), 1, 1, 1, 1).unwrap();
    assert_eq!(dims_of(&d), (1, 1));
    assert_eq!(pixel_at(&d, 0, 0), [255, 255, 0], "裁 (1,1) 应得到黄色");

    // 裁 (0,0) 起 2x2 ⇒ 左上红、右上绿、左下蓝
    let d2 = dir.join("c2.png");
    cap_img::crop(&s, d2.to_string_lossy().as_ref(), 0, 0, 2, 2).unwrap();
    assert_eq!(dims_of(&d2), (2, 2));
    assert_eq!(pixel_at(&d2, 0, 0), [255, 0, 0]);
    assert_eq!(pixel_at(&d2, 1, 0), [0, 255, 0]);
    assert_eq!(pixel_at(&d2, 0, 1), [0, 0, 255]);
}

/// crop 的边界：正好贴边必须允许（> 而不是 >=）。
///
/// 4x4 的图裁 4x4 是合法的，原实现用 `x + w > width` 判越界，
/// 贴边通过；但把 x/y 推到 u32::MAX 时 `x + w` 溢出 ⇒ debug panic、
/// release 回绕成小值反而绕过检查。两条都要钉住。
#[test]
fn crop_accepts_exact_edge_but_rejects_overflow() {
    let dir = TempDir::new("crop-edge");
    let src = png(&dir, "a.png");
    let s = src.to_string_lossy().to_string();

    // 贴边：正好等于图尺寸 ⇒ 允许
    let d = dir.join("edge.png");
    cap_img::crop(&s, d.to_string_lossy().as_ref(), 0, 0, 4, 4).expect("正好贴边必须允许");
    assert_eq!(dims_of(&d), (4, 4));

    // 超出一格 ⇒ 拒绝（既有行为）
    let err = cap_img::crop(
        &s,
        dir.join("over.png").to_string_lossy().as_ref(),
        0,
        0,
        5,
        4,
    )
    .expect_err("超出 1 像素必须被拒");
    assert!(err.contains("超出图片范围"), "实得 {}", err);

    // 溢出：x + w 溢出 u32 ⇒ 必须返回 Err，不能 panic，也不能静默通过
    let err2 = cap_img::crop(
        &s,
        dir.join("ovf.png").to_string_lossy().as_ref(),
        u32::MAX,
        0,
        4,
        4,
    )
    .expect_err("坐标溢出必须被拒");
    assert!(
        err2.contains("溢出") || err2.contains("超出图片范围"),
        "实得 {}",
        err2
    );
    assert!(
        !dir.join("ovf.png").exists(),
        "被拒的裁剪不得留下半成品文件"
    );
}

/// export 的 quality 必须真的影响产物 —— 否则"质量"这个参数就是个摆设。
///
/// 判据用**产物字节数**：编码器对 quality 极其敏感（实测 64x64 噪声图：
/// q=10 → 1655 字节，q=100 → 15934 字节），字节数是最可靠的可观察量。
///
/// 顺带记一条实测事实：image crate 的 JPEG 编码器自己就把 quality 夹到 1..=100
/// （q=0 与 q=1 同为 1462 字节，q=101/255 与 q=100 同为 15934 字节），
/// 所以 export 里的 `quality.clamp(1, 100)` 实际是**冗余**的 ——
/// 去掉它行为不变。这是等价变异，不是缺陷。
#[test]
fn export_quality_changes_the_output_size() {
    let dir = TempDir::new("export-quality");
    // 带噪声的图：纯色图在任何 quality 下都是同样大小，区分不出来
    let src = dir.join("a.png");
    let mut img = image::RgbImage::new(64, 64);
    for (i, p) in img.pixels_mut().enumerate() {
        *p = image::Rgb([
            (i * 37 % 256) as u8,
            (i * 91 % 256) as u8,
            (i * 13 % 256) as u8,
        ]);
    }
    image::DynamicImage::ImageRgb8(img).save(&src).unwrap();
    let s = src.to_string_lossy().to_string();

    let low = dir.join("low.jpg");
    let high = dir.join("high.jpg");
    cap_img::export(&s, low.to_string_lossy().as_ref(), "jpeg", 10).unwrap();
    cap_img::export(&s, high.to_string_lossy().as_ref(), "jpeg", 100).unwrap();

    let low_size = fs::metadata(&low).unwrap().len();
    let high_size = fs::metadata(&high).unwrap().len();
    assert!(
        high_size > low_size * 2,
        "quality 100 的产物应显著大于 quality 10：实得 {} vs {}",
        high_size,
        low_size
    );
}

/// 变换族与 export 都必须让**文件内容与扩展名相符**。
///
/// 原实现按"源图扩展名"决定输出格式，于是 resize(a.png → out.jpg) 往 out.jpg 里
/// 写的是 PNG 字节 —— 名字说谎。同一个 crate 里 save_bytes 早就拒绝这种不一致，
/// 变换族与 export 却没跟上。
#[test]
fn output_format_follows_the_destination_extension() {
    let dir = TempDir::new("ext-truth");
    let src = six_color(&dir, "a.png");
    let s = src.to_string_lossy().to_string();

    // 源 .png → 目标 .jpg ⇒ 目标里必须是 JPEG 字节
    let dj = dir.join("out.jpg");
    cap_img::resize(&s, dj.to_string_lossy().as_ref(), 2, 2).unwrap();
    assert_eq!(
        &magic_of(&dj)[..2],
        &JPEG_MAGIC,
        "目标叫 .jpg 就必须装 JPEG 字节"
    );

    // 源 .jpg → 目标 .png ⇒ 目标里必须是 PNG 字节
    let jpg_src = dir.join("src.jpg");
    cap_img::export(&s, jpg_src.to_string_lossy().as_ref(), "jpeg", 90).unwrap();
    let dp = dir.join("out2.png");
    cap_img::resize(
        jpg_src.to_string_lossy().as_ref(),
        dp.to_string_lossy().as_ref(),
        2,
        2,
    )
    .unwrap();
    assert_eq!(
        &magic_of(&dp)[..4],
        &PNG_MAGIC,
        "目标叫 .png 就必须装 PNG 字节"
    );
}

/// export 的 format 与目标扩展名不符时必须拒绝（与 save_bytes 同一规则）。
#[test]
fn export_refuses_format_extension_mismatch() {
    let dir = TempDir::new("export-lie");
    let src = png(&dir, "a.png");
    let s = src.to_string_lossy().to_string();

    let dest = dir.join("lie.png");
    let err = cap_img::export(&s, dest.to_string_lossy().as_ref(), "jpeg", 90)
        .expect_err("format=jpeg 而扩展名是 .png 必须被拒");
    assert!(err.contains("格式不符"), "实得 {}", err);
    assert!(!dest.exists(), "被拒的导出不得留下半成品文件");

    // 大小写与 jpeg 别名不能把这条闸拦掉
    let ok = dir.join("ok.jpeg");
    cap_img::export(&s, ok.to_string_lossy().as_ref(), "JPEG", 90).unwrap();
    assert_eq!(&magic_of(&ok)[..2], &JPEG_MAGIC);
}

/// 目标扩展名认不出来时必须报错，而不是悄悄写成 PNG。
#[test]
fn transform_rejects_unknown_destination_extension() {
    let dir = TempDir::new("unknown-ext");
    let src = png(&dir, "a.png");
    let s = src.to_string_lossy().to_string();

    let dest = dir.join("out.xyz");
    let err = cap_img::resize(&s, dest.to_string_lossy().as_ref(), 2, 2)
        .expect_err("认不出的扩展名必须报错而不是悄悄写 PNG");
    assert!(err.contains("不受支持"), "实得 {}", err);
    assert!(!dest.exists(), "被拒的变换不得留下半成品文件");
}

#[test]
fn transform_family_round_trips() {
    let dir = TempDir::new("xform");
    let src = png(&dir, "a.png");
    let s = src.to_string_lossy().to_string();
    for (label, res) in [
        (
            "rotate",
            cap_img::rotate(&s, dir.join("r.png").to_string_lossy().as_ref(), 90),
        ),
        (
            "flip",
            cap_img::flip(&s, dir.join("f.png").to_string_lossy().as_ref(), true),
        ),
        (
            "crop",
            cap_img::crop(&s, dir.join("c.png").to_string_lossy().as_ref(), 0, 0, 2, 2),
        ),
        (
            "filter",
            cap_img::apply_filter(
                &s,
                dir.join("g.png").to_string_lossy().as_ref(),
                "grayscale",
            ),
        ),
        (
            "export",
            cap_img::export(&s, dir.join("e.jpg").to_string_lossy().as_ref(), "jpeg", 90),
        ),
    ] {
        res.unwrap_or_else(|e| panic!("{} 应当成功: {}", label, e));
    }
    // 光"成功"钉不住任何东西：把 rotate 实现成原样拷贝、把 flip 实现成恒等，
    // 这里照样全绿。每条都必须留下**内容可核对的产物**。
    assert_eq!(
        dims_of(&dir.join("r.png")),
        (4, 4),
        "rotate 产物必须存在且可解码"
    );
    assert_eq!(dims_of(&dir.join("f.png")), (4, 4));
    assert_eq!(dims_of(&dir.join("c.png")), (2, 2), "crop 产物必须是 2x2");
    assert_eq!(dims_of(&dir.join("g.png")), (4, 4));
    assert_eq!(
        &magic_of(&dir.join("e.jpg"))[..2],
        &JPEG_MAGIC,
        "export 产物必须是 JPEG 字节"
    );
}

#[test]
fn rotate_rejects_unknown_angle() {
    let dir = TempDir::new("rot-bad");
    let src = png(&dir, "a.png");
    let err = cap_img::rotate(
        src.to_string_lossy().as_ref(),
        dir.join("x.png").to_string_lossy().as_ref(),
        45,
    )
    .expect_err("45° 不该被接受");
    assert!(err.contains("不支持的角度"), "实得 {}", err);
}

#[test]
fn crop_rejects_out_of_range() {
    let dir = TempDir::new("crop-bad");
    let src = png(&dir, "a.png");
    let err = cap_img::crop(
        src.to_string_lossy().as_ref(),
        dir.join("x.png").to_string_lossy().as_ref(),
        0,
        0,
        8,
        8,
    )
    .expect_err("4x4 的图裁 8x8 必须被拒绝");
    assert!(err.contains("超出图片范围"), "实得 {}", err);
}

#[test]
fn filter_rejects_unknown_name() {
    let dir = TempDir::new("filter-bad");
    let src = png(&dir, "a.png");
    let err = cap_img::apply_filter(
        src.to_string_lossy().as_ref(),
        dir.join("x.png").to_string_lossy().as_ref(),
        "vaporwave",
    )
    .expect_err("未知滤镜必须被拒绝");
    assert!(err.contains("不支持的滤镜"), "实得 {}", err);
}

/// 每个滤镜的**输出数值**必须被钉住。
///
/// 原来的 transform_family_round_trips 只验"调用成功"，于是把 invert 实现成
/// 原样返回、把 grayscale 实现成恒等，全都能过 —— 而滤镜的语义恰恰就是
/// "把像素改成什么样子"。
///
/// 期望值取自**实跑**（纯红 255,0,0 的 4x4 PNG 经各滤镜后的左上角像素），
/// 不是按公式手推 —— 手推过一次 grayscale 的 ITU-R 601 权重就与实现有 1 LSB 差。
/// 这里用容差 2 吸收编码器往返的舍入，但仍然要求"方向与量级"对得上。
#[test]
fn filters_produce_the_expected_pixel_values() {
    let dir = TempDir::new("filter-math");
    let src = dir.join("a.png");
    let mut img = image::RgbImage::new(4, 4);
    for p in img.pixels_mut() {
        *p = image::Rgb([255, 0, 0]);
    }
    image::DynamicImage::ImageRgb8(img).save(&src).unwrap();
    let s = src.to_string_lossy().to_string();

    // (滤镜, 期望 RGB) —— 实测值
    let cases: [(&str, [u8; 3]); 7] = [
        ("grayscale", [54, 54, 54]), // 亮度 0.2126R+0.7152G+0.0722B
        ("sepia", [100, 88, 69]),    // sepia 矩阵
        ("invert", [0, 255, 255]),   // 255 - 各通道
        ("brightness", [255, 0, 0]), // ×1.3 后夹到 255
        ("contrast", [255, 0, 0]),   // ×1.5 后夹到 255
        ("blur", [255, 0, 0]),       // 纯色图模糊后不变
        ("sharpen", [255, 0, 0]),    // 纯色图锐化后不变
    ];

    for (name, want) in cases {
        let dest = dir.join(&format!("{}.png", name));
        cap_img::apply_filter(&s, dest.to_string_lossy().as_ref(), name)
            .unwrap_or_else(|e| panic!("{} 应当成功: {}", name, e));
        let got = pixel_at(&dest, 0, 0);
        for ch in 0..3 {
            assert!(
                (got[ch] as i32 - want[ch] as i32).abs() <= 2,
                "滤镜 {} 的通道 {} 期望 ≈{} 实得 {}（全像素 {:?}）",
                name,
                ch,
                want[ch],
                got[ch],
                got
            );
        }
    }
}

/// invert 必须真的取反（不是"接近原色"就行）—— 纯红进、纯青出。
#[test]
fn invert_actually_inverts() {
    let dir = TempDir::new("filter-invert");
    let src = dir.join("a.png");
    let mut img = image::RgbImage::new(4, 4);
    for p in img.pixels_mut() {
        *p = image::Rgb([10, 20, 30]);
    }
    image::DynamicImage::ImageRgb8(img).save(&src).unwrap();
    let dest = dir.join("out.png");
    cap_img::apply_filter(
        src.to_string_lossy().as_ref(),
        dest.to_string_lossy().as_ref(),
        "invert",
    )
    .unwrap();
    let got = pixel_at(&dest, 0, 0);
    assert_eq!(got, [245, 235, 225], "10/20/30 取反应是 245/235/225");
}

/// brightness 的系数必须被钉住 —— 用**中间值**像素，纯红区分不出来。
///
/// 变异验证实测：把系数 1.3 改成 1.0，纯红图 (255,0,0) 两种实现都得 255
/// （1.3×255 被夹到上限），判据不敏感。
/// 能区分的形状是 100：1.3×100 = 130，1.0×100 = 100。
#[test]
fn brightness_scales_by_a_factor_above_one() {
    let dir = TempDir::new("filter-bright");
    let src = dir.join("a.png");
    let mut img = image::RgbImage::new(4, 4);
    for p in img.pixels_mut() {
        *p = image::Rgb([100, 0, 0]);
    }
    image::DynamicImage::ImageRgb8(img).save(&src).unwrap();

    let dest = dir.join("out.png");
    cap_img::apply_filter(
        src.to_string_lossy().as_ref(),
        dest.to_string_lossy().as_ref(),
        "brightness",
    )
    .unwrap();
    let got = pixel_at(&dest, 0, 0);
    // 100 × 1.3 = 130（PNG 是无损的，这里应精确相等）
    assert_eq!(got[0], 130, "100 × 1.3 应得 130，实得 {}", got[0]);
    // 绿色通道是 0，×1.3 仍是 0
    assert_eq!(got[1], 0);
}

#[test]
fn save_bytes_still_writes_bytes() {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let dir = TempDir::new("save-ok");
    let dest = dir.join("sub").join("blob.bin");
    let n = cap_img::save_bytes(
        STANDARD.encode(b"payload").as_str(),
        dest.to_string_lossy().as_ref(),
        "bin",
    )
    .unwrap();
    assert_eq!(n, 7);
    assert_eq!(fs::read(&dest).unwrap(), b"payload");
}

#[test]
fn save_bytes_refuses_extension_format_mismatch() {
    // format 这个参数以前是被丢掉的：调用方说自己是 png、扩展名写 .jpg 也照写，
    // 于是磁盘上出现一个"名字说谎"的文件，双击打不开还不知道为什么
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let dir = TempDir::new("save-format");
    let dest = dir.join("lie.jpg");
    let err = cap_img::save_bytes(
        STANDARD.encode(b"pngbytes").as_str(),
        dest.to_string_lossy().as_ref(),
        "png",
    )
    .expect_err("扩展名与格式不符时必须拒绝");
    assert!(err.contains("格式不符"), "实得 {}", err);
    assert!(!dest.exists());
    // 大小写与 jpeg 别名不能把这条闸拦掉
    let ok = dir.join("real.jpeg");
    cap_img::save_bytes(
        STANDARD.encode(b"pngbytes").as_str(),
        ok.to_string_lossy().as_ref(),
        "JPEG",
    )
    .unwrap();
    assert!(ok.exists());
}

#[test]
fn detect_format_normalizes_known_extensions() {
    assert_eq!(cap_img::detect_format("a.JPG"), "jpg");
    assert_eq!(cap_img::detect_format("a.jpeg"), "jpg");
    assert_eq!(cap_img::detect_format("a.tif"), "tiff");
    assert_eq!(cap_img::detect_format("a.heic"), "png"); // 未知扩展名默认 png
}

// ---------------------------------------------------------------------------
// 0.2.0 字节级原语：魔数嗅探 / data URL / 帧编码
// 每个用例都对着「消费方的原始形状」断言，确保搬过来语义没漂。
// ---------------------------------------------------------------------------

#[test]
fn sniff_by_magic_not_extension() {
    // aigen reference.rs 的原始断言形状
    let png: &[u8] = b"\x89PNG\r\n\x1a\n..";
    let jpeg: &[u8] = b"\xff\xd8\xff\xe0..";
    assert_eq!(cap_img::sniff_format(png), Some("png"));
    assert_eq!(cap_img::sniff_format(jpeg), Some("jpg"));
    assert_eq!(cap_img::sniff_format(b"GIF89a.."), Some("gif"));
    let webp: &[u8] = b"RIFF\x00\x00\x00\x00WEBP\x00\x00\x00";
    assert_eq!(cap_img::sniff_format(webp), Some("webp"));
    assert_eq!(cap_img::sniff_format(b"\x00\x01binary"), None);
    // 扩展名说谎也不认：只有 .png 后缀但内容是垃圾 → None
    assert_eq!(cap_img::sniff_format(b"not an image at all"), None);
}

#[test]
fn sniff_rejects_short_webp_prefix() {
    // RIFF 开头但不足 12 字节：不能越界切片，也不能误判
    assert_eq!(cap_img::sniff_format(b"RIFF"), None);
    assert_eq!(cap_img::sniff_format(b"RIFFxxxxWEB"), None);
}

/// 魔数必须**完整**匹配，不能只要前缀。
///
/// 变异验证实测：把 PNG 魔数从 8 字节缩成 4 字节（`\x89PNG\r\n`），
/// 原有断言全绿 —— 因为 `starts_with` 只要求**前缀**，
/// 短魔数只会**更宽松**，真 PNG 照样匹配。
/// 能区分的形状是「只有 PNG 魔数前缀、但不是完整 PNG 头」的输入：
/// 完整魔数下它该被拒，短魔数下会被误认成 PNG。
#[test]
fn sniff_requires_the_complete_magic_number() {
    // 完整 PNG 魔数只写了一半（缺 \x1a\n）
    assert_eq!(
        cap_img::sniff_format(b"\x89PNG\r\n"),
        None,
        "只有 PNG 魔数前缀必须被拒"
    );
    // 完整魔数 + 一点后续字节 ⇒ 认得出
    assert_eq!(cap_img::sniff_format(b"\x89PNG\r\n\x1a\n"), Some("png"));
    // JPEG 魔数同理：只有 SOI 头两字节不够（实现要求三字节 \xff\xd8\xff）
    assert_eq!(cap_img::sniff_format(b"\xff\xd8"), None);
}

/// `parse_format` 必须直接接受 tif —— 它的归一化只发生在 `detect_format` 里，
/// 而 detect_format 会把 tif 先变成 tiff，于是 parse_format 的 tif 分支
/// 若没被直接测过，就永远没被执行过（变异验证实测该变异存活）。
#[test]
fn parse_format_accepts_every_documented_alias() {
    use image::ImageFormat;
    assert_eq!(cap_img::parse_format("jpg").unwrap(), ImageFormat::Jpeg);
    assert_eq!(cap_img::parse_format("jpeg").unwrap(), ImageFormat::Jpeg);
    assert_eq!(cap_img::parse_format("JPEG").unwrap(), ImageFormat::Jpeg);
    assert_eq!(cap_img::parse_format("png").unwrap(), ImageFormat::Png);
    assert_eq!(cap_img::parse_format("gif").unwrap(), ImageFormat::Gif);
    assert_eq!(cap_img::parse_format("bmp").unwrap(), ImageFormat::Bmp);
    assert_eq!(cap_img::parse_format("webp").unwrap(), ImageFormat::WebP);
    assert_eq!(cap_img::parse_format("ico").unwrap(), ImageFormat::Ico);
    assert_eq!(cap_img::parse_format("tiff").unwrap(), ImageFormat::Tiff);
    assert_eq!(cap_img::parse_format("tif").unwrap(), ImageFormat::Tiff);
    // 不支持的必须报错，而不是悄悄给个默认格式
    assert!(cap_img::parse_format("heic").is_err());
    assert!(cap_img::parse_format("").is_err());
}

#[test]
fn data_url_roundtrip() {
    let bytes = b"hello image bytes";
    let url = cap_img::encode_data_url(bytes, "image/png");
    assert!(url.starts_with("data:image/png;base64,"));
    let parsed = cap_img::decode_data_url(&url).expect("刚编的必须能解回来");
    assert_eq!(parsed.mime, "image/png");
    assert_eq!(parsed.bytes, bytes);
}

#[test]
fn decode_data_url_matches_aigen_semantics() {
    // aigen history.rs 的原始用例（解不出 → None）
    let png = cap_img::encode_data_url(b"\x89PNG\r\n\x1a\n", "image/png");
    let (b, ext) = {
        let d = cap_img::decode_data_url(&png).unwrap();
        (d.bytes, cap_img::ext_for_mime(&d.mime))
    };
    assert_eq!(b, b"\x89PNG\r\n\x1a\n");
    assert_eq!(ext, "png");
    assert_eq!(
        cap_img::decode_data_url("data:image/jpeg;base64,AA==")
            .map(|d| cap_img::ext_for_mime(&d.mime)),
        Some("jpg".to_string())
    );
    assert_eq!(
        cap_img::decode_data_url("data:image/svg+xml;base64,AA==")
            .map(|d| cap_img::ext_for_mime(&d.mime)),
        Some("svg".to_string())
    );
    // meta 没有 base64 标记 / payload 不是合法 base64 → None
    assert!(cap_img::decode_data_url("data:image/png,notbase64").is_none());
    assert!(cap_img::decode_data_url("data:image/png;base64,!!!not-b64!!!").is_none());
    // 不是 data URL / 缺逗号 → None
    assert!(cap_img::decode_data_url("https://x/y.png").is_none());
    assert!(cap_img::decode_data_url("data:image/png;base64").is_none());
    assert!(cap_img::decode_data_url("").is_none());

    // 上面那条 "data:image/png,notbase64" 钉不住「meta 必须含 base64 标记」：
    // 载荷 "notbase64" 本身就不是合法 base64，去掉标记检查后**仍然**解不出来，
    // 两条路径撞巧一致 ⇒ 判据不敏感。
    // 能区分的形状是「载荷合法但缺标记」：AA== 是合法 base64，
    // 去掉标记检查后就会被解出来。
    assert!(
        cap_img::decode_data_url("data:image/png,AA==").is_none(),
        "meta 里没有 base64 标记时必须拒绝（哪怕载荷本身是合法 base64）"
    );
}

#[test]
fn decode_data_url_accepts_unpadded_payload() {
    // note 侧历史行为：payload 无 padding 也能解
    let d = cap_img::decode_data_url("data:image/png;base64,AA").expect("无 padding 应可解");
    assert_eq!(d.bytes, vec![0u8]);
    let d2 = cap_img::decode_data_url("data:image/png;base64,AA==").expect("有 padding 应可解");
    assert_eq!(d.bytes, d2.bytes);
}

#[test]
fn decode_data_url_mime_is_normalized() {
    // 带参数段 + 大写：取到的 mime 应是干净小写的主类型
    let d = cap_img::decode_data_url("data:image/PNG; charset=utf-8;BASE64,AA==")
        .expect("带参数也应可解");
    assert_eq!(d.mime, "image/png");
}

#[test]
fn mime_ext_mapping_bidirectional() {
    // note read_image 的 mime 选择形状
    assert_eq!(cap_img::mime_for_ext("jpg"), "image/jpeg");
    assert_eq!(cap_img::mime_for_ext("jpeg"), "image/jpeg");
    assert_eq!(cap_img::mime_for_ext("png"), "image/png");
    assert_eq!(cap_img::mime_for_ext("gif"), "image/gif");
    assert_eq!(cap_img::mime_for_ext("webp"), "image/webp");
    assert_eq!(cap_img::mime_for_ext("svg"), "application/octet-stream");
    // note save_image 的 ext 选择形状（按 mime 判）
    assert_eq!(cap_img::ext_for_mime("image/png"), "png");
    assert_eq!(cap_img::ext_for_mime("image/jpeg"), "jpg");
    assert_eq!(cap_img::ext_for_mime("image/svg+xml"), "svg");
    assert_eq!(cap_img::ext_for_mime("video/mp4"), "mp4");
    assert_eq!(cap_img::ext_for_mime("garbage"), "png");
}

#[test]
fn frame_jpeg_encodes_rgba_without_panic() {
    // remote capture.rs 的真实形状：4x4 全 128 的 RGBA 帧
    let (w, h) = (4u32, 4u32);
    let rgba = vec![128u8; (w * h * 4) as usize];
    let frame = cap_img::encode_frame_jpeg(&rgba, w, h, 80, 0).expect("合法帧必须编码成功");
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(&frame.base64)
        .expect("base64 必须合法");
    // JPEG 魔数 + 确实是解得开的图
    assert_eq!(&raw[..2], &[0xFF, 0xD8]);
    let img = image::load_from_memory(&raw).expect("编码产物必须能被 image 解回来");
    assert_eq!((img.width(), img.height()), (w, h));
    assert_eq!((frame.width, frame.height), (w, h));
}

#[test]
fn frame_jpeg_wrong_channel_length_is_error_not_panic() {
    // 历史 bug 的回归锚点：4 通道数据 + 声明 3 通道 = image crate 直接 abort。
    // 现在必须在长度校验处返回 Err，进程活着拿到可读信息。
    let (w, h) = (4u32, 4u32);
    let wrong = vec![128u8; (w * h * 3) as usize]; // 3 通道冒充 4 通道
    let err =
        cap_img::encode_frame_jpeg(&wrong, w, h, 80, 0).expect_err("长度不符必须报错而不是 panic");
    assert!(err.contains("长度不符"), "实得 {}", err);
    assert!(err.contains("48"), "错误信息应带期望长度，实得 {}", err);
}

#[test]
fn frame_jpeg_downscales_to_max_width() {
    // remote 的 downscale 语义：等比缩小、高度向上取整、已窄不放大
    let (w, h) = (100u32, 33u32);
    let rgba = vec![200u8; (w * h * 4) as usize];
    let f = cap_img::encode_frame_jpeg(&rgba, w, h, 70, 50).unwrap();
    assert_eq!(f.width, 50);
    assert_eq!(f.height, 17); // 33 * 0.5 = 16.5 → ceil 17

    // max_width == 0 → 原样
    let f0 = cap_img::encode_frame_jpeg(&rgba, w, h, 70, 0).unwrap();
    assert_eq!((f0.width, f0.height), (100, 33));

    // 原图已比 max_width 窄 → 不放大
    let f1 = cap_img::encode_frame_jpeg(&rgba, w, h, 70, 500).unwrap();
    assert_eq!((f1.width, f1.height), (100, 33));
}

#[test]
fn frame_jpeg_quality_is_clamped() {
    let rgba = vec![10u8; (8 * 8 * 4) as usize];
    // 0 与 255 都不该 panic/报错，夹到 10..=100
    assert!(cap_img::encode_frame_jpeg(&rgba, 8, 8, 0, 0).is_ok());
    assert!(cap_img::encode_frame_jpeg(&rgba, 8, 8, 255, 0).is_ok());
}

#[test]
fn frame_jpeg_rejects_overflow_dimensions() {
    let rgba = vec![0u8; 16];
    let err =
        cap_img::encode_frame_jpeg(&rgba, u32::MAX, u32::MAX, 80, 0).expect_err("尺寸溢出应报错");
    assert!(
        err.contains("溢出") || err.contains("长度不符"),
        "实得 {}",
        err
    );
}
