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

/// 在一张真 JPEG 的段链里插一段 APP1/EXIF。
/// 必须基于真图：`info` 会先用 image crate 解码，手搓的假 SOI 直接
/// 在解码那步就报错，永远走不到 EXIF 分支。
fn jpeg_with_exif(dir: &TempDir, rel: &str) -> PathBuf {
    let base_path = dir.join("base.jpg");
    image::DynamicImage::new_rgb8(4, 4).save(&base_path).unwrap();
    let base = fs::read(&base_path).unwrap();

    // IFD0 一条记录：Model(0x0110) -> 偏移 26 处的 "TEST"
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
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(b"Exif\0\0");
    body.extend_from_slice(&tiff);
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
}

#[test]
fn info_actually_reads_exif_from_a_jpeg() {
    let dir = TempDir::new("exif");
    let src = jpeg_with_exif(&dir, "a.jpg");
    let info = cap_img::info(src.to_string_lossy().as_ref()).unwrap();
    let exif = info.exif.expect("JPEG 里的 EXIF 必须被读出来");
    assert_eq!(exif.get("Model").map(|s| s.as_str()), Some("TEST"));
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
}

#[test]
fn resize_still_writes_and_creates_parent() {
    let dir = TempDir::new("resize-ok");
    let src = png(&dir, "a.png");
    let dest = dir.join("out").join("small.png");
    cap_img::resize(src.to_string_lossy().as_ref(), dest.to_string_lossy().as_ref(), 2, 2)
        .unwrap();
    assert!(dest.exists());
    assert!(fs::metadata(&dest).unwrap().len() > 0);
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
}

#[test]
fn rotate_rejects_unknown_angle() {
    let dir = TempDir::new("rot-bad");
    let src = png(&dir, "a.png");
    let err = cap_img::rotate(src.to_string_lossy().as_ref(), dir.join("x.png").to_string_lossy().as_ref(), 45)
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
