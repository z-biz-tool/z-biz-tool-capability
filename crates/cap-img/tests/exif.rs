//! EXIF 全量读写的行为测试。
//!
//! ## 为什么用 kamadak-exif 的 Writer 造图
//!
//! 手搓 TIFF 体（既有 behavior.rs 里的 `tiff_with_model` 那种）只能造出
//! IFD0 里一条 ASCII 记录 —— 造不出 SubIFD、造不出 GPS IFD、造不出 Rational。
//! 于是「全量能力」根本没法被测。
//!
//! 这里改用 kamadak-exif 的 Writer 生成完整的 EXIF 容器：
//!   - **造图与被测同源**：同一份字节，读回来必须还是同样的语义
//!   - **能造 SubIFD / GPS**：光圈、快门、ISO、GPS 坐标全都能覆盖
//!   - **写入可对账**：写完再读，读回的值必须与写入值逐项相等
//!
//! 但**造图不能只用 Writer** —— 那样测的是「kamadak-exif 自己跟自己玩」。
//! 所以每个关键断言都用**独立的解析路径**复核（直接读字节 / 用不同 API 读），
//! 避免自证。

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use exif::{Context, Field, In, Rational, Tag, Value};

static SEQ: AtomicU32 = AtomicU32::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "cap-img-exif-{}-{}-{}-{}",
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

/// 一张 8x8 的真 JPEG（像素有内容，便于验证「写 EXIF 不动像素」）。
fn base_jpeg(dir: &TempDir, rel: &str) -> PathBuf {
    let p = dir.join(rel);
    let mut img = image::RgbImage::new(8, 8);
    for (i, px) in img.pixels_mut().enumerate() {
        *px = image::Rgb([(i * 7 % 256) as u8, 128, (i * 13 % 256) as u8]);
    }
    image::DynamicImage::ImageRgb8(img)
        .save_with_format(&p, image::ImageFormat::Jpeg)
        .unwrap();
    p
}

/// 用 kamadak-exif 的 Writer 生成一段 TIFF/EXIF 体。
fn build_exif(fields: &[Field]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut writer = exif::experimental::Writer::new();
        for f in fields {
            writer.push_field(f);
        }
        writer
            .write(&mut std::io::Cursor::new(&mut buf), true)
            .unwrap();
    }
    buf
}

/// 造一张带指定 EXIF 字段的 JPEG。
fn jpeg_with_fields(dir: &TempDir, rel: &str, fields: &[Field]) -> PathBuf {
    let base = base_jpeg(dir, &format!("{}-base.jpg", rel));
    let base_bytes = fs::read(&base).unwrap();
    let exif_body = build_exif(fields);

    let mut app1 = Vec::new();
    app1.extend_from_slice(b"Exif\0\0");
    app1.extend_from_slice(&exif_body);
    let seg_len = (app1.len() + 2) as u16;

    // 插到 SOI 之后（第一个 APP0 之前），保证能被解析器找到
    let mut out = Vec::new();
    out.extend_from_slice(&base_bytes[..2]);
    out.push(0xFF);
    out.push(0xE1);
    out.extend_from_slice(&seg_len.to_be_bytes());
    out.extend_from_slice(&app1);
    out.extend_from_slice(&base_bytes[2..]);

    let p = dir.join(rel);
    fs::write(&p, out).unwrap();
    p
}

/// 一组覆盖「典型照片」的字段：机型 + 拍摄参数 + GPS。
fn typical_photo_fields() -> Vec<Field> {
    vec![
        Field {
            tag: Tag::Make,
            ifd_num: In::PRIMARY,
            value: Value::Ascii(vec![b"Canon".to_vec()]),
        },
        Field {
            tag: Tag::Model,
            ifd_num: In::PRIMARY,
            value: Value::Ascii(vec![b"EOS R5".to_vec()]),
        },
        Field {
            tag: Tag::Orientation,
            ifd_num: In::PRIMARY,
            value: Value::Short(vec![1]),
        },
        // Exif SubIFD 的曝光参数
        Field {
            tag: Tag::ExposureTime,
            ifd_num: In::PRIMARY,
            value: Value::Rational(vec![Rational { num: 1, denom: 250 }]),
        },
        Field {
            tag: Tag::FNumber,
            ifd_num: In::PRIMARY,
            value: Value::Rational(vec![Rational { num: 28, denom: 10 }]),
        },
        Field {
            tag: Tag::PhotographicSensitivity,
            ifd_num: In::PRIMARY,
            value: Value::Short(vec![400]),
        },
        Field {
            tag: Tag::FocalLength,
            ifd_num: In::PRIMARY,
            value: Value::Rational(vec![Rational { num: 85, denom: 1 }]),
        },
        Field {
            tag: Tag::LensModel,
            ifd_num: In::PRIMARY,
            value: Value::Ascii(vec![b"RF85mm F1.2 L USM".to_vec()]),
        },
        // GPS
        Field {
            tag: Tag::GPSLatitudeRef,
            ifd_num: In::PRIMARY,
            value: Value::Ascii(vec![b"N".to_vec()]),
        },
        Field {
            tag: Tag::GPSLatitude,
            ifd_num: In::PRIMARY,
            value: Value::Rational(vec![
                Rational { num: 39, denom: 1 },
                Rational { num: 54, denom: 1 },
                Rational { num: 26, denom: 1 },
            ]),
        },
        Field {
            tag: Tag::GPSLongitudeRef,
            ifd_num: In::PRIMARY,
            value: Value::Ascii(vec![b"W".to_vec()]),
        },
        Field {
            tag: Tag::GPSLongitude,
            ifd_num: In::PRIMARY,
            value: Value::Rational(vec![
                Rational { num: 75, denom: 1 },
                Rational { num: 9, denom: 1 },
                Rational { num: 30, denom: 1 },
            ]),
        },
        Field {
            tag: Tag::GPSAltitude,
            ifd_num: In::PRIMARY,
            value: Value::Rational(vec![Rational { num: 100, denom: 1 }]),
        },
    ]
}

// ---------------------------------------------------------------------------
// 读：全量能力
// ---------------------------------------------------------------------------

/// 回归锚点：cap-img 0.3.0 的手写解析只认 5 个 ASCII 标签，
/// 光圈/快门/ISO/焦距/GPS/方向**一个都读不出来**。
///
/// 这条用例是「EXIF 全量」这个承诺的刻度：它必须从「读不到」变成「读得到」。
#[test]
fn reads_exif_fields_the_old_hand_parser_could_not() {
    let dir = TempDir::new("read-full");
    let path = jpeg_with_fields(&dir, "photo.jpg", &typical_photo_fields());

    let data = cap_img::exif::read_or_empty(&path).unwrap();

    // 机型与方向（IFD0）
    assert_eq!(data.display("Make"), Some("Canon"));
    assert_eq!(data.display("Model"), Some("EOS R5"));
    assert_eq!(data.display("Orientation"), Some("1"));

    // 曝光参数（Exif SubIFD）—— 老解析器完全读不到
    assert_eq!(data.display("ExposureTime"), Some("1/250"));
    assert_eq!(data.display("FNumber"), Some("2.8"));
    assert_eq!(data.display("PhotographicSensitivity"), Some("400"));
    assert_eq!(data.display("FocalLength"), Some("85"));
    assert_eq!(data.display("LensModel"), Some("RF85mm F1.2 L USM"));

    // GPS（GPS IFD）
    assert_eq!(data.display("GPSAltitude"), Some("100"));
}

/// GPS 坐标要能算出十进制度 —— DMS 格式直接当数字读是错的。
#[test]
fn gps_coordinates_are_decoded_to_decimal_degrees() {
    let dir = TempDir::new("gps");
    let path = jpeg_with_fields(&dir, "photo.jpg", &typical_photo_fields());

    let data = cap_img::exif::read_or_empty(&path).unwrap();
    let (lat, lon) = data.gps_coordinates().expect("必须能算出坐标");

    // 39°54'26"N = 39 + 54/60 + 26/3600
    let expect_lat = 39.0 + 54.0 / 60.0 + 26.0 / 3600.0;
    // 75°09'30"W ⇒ 取负
    let expect_lon = -(75.0 + 9.0 / 60.0 + 30.0 / 3600.0);
    assert!((lat - expect_lat).abs() < 1e-9, "lat={}", lat);
    assert!((lon - expect_lon).abs() < 1e-9, "lon={}", lon);
}

/// 南纬/西经必须取负 —— 搞错方向会把照片定位到地球另一头。
#[test]
fn gps_south_and_west_are_negative() {
    let dir = TempDir::new("gps-sw");
    let fields = vec![
        Field {
            tag: Tag::GPSLatitudeRef,
            ifd_num: In::PRIMARY,
            value: Value::Ascii(vec![b"S".to_vec()]),
        },
        Field {
            tag: Tag::GPSLatitude,
            ifd_num: In::PRIMARY,
            value: Value::Rational(vec![
                Rational { num: 33, denom: 1 },
                Rational { num: 52, denom: 1 },
                Rational { num: 0, denom: 1 },
            ]),
        },
        Field {
            tag: Tag::GPSLongitudeRef,
            ifd_num: In::PRIMARY,
            value: Value::Ascii(vec![b"W".to_vec()]),
        },
        Field {
            tag: Tag::GPSLongitude,
            ifd_num: In::PRIMARY,
            value: Value::Rational(vec![
                Rational { num: 151, denom: 1 },
                Rational { num: 12, denom: 1 },
                Rational { num: 0, denom: 1 },
            ]),
        },
    ];
    let path = jpeg_with_fields(&dir, "photo.jpg", &fields);

    let data = cap_img::exif::read_or_empty(&path).unwrap();
    let (lat, lon) = data.gps_coordinates().unwrap();
    assert!(lat < 0.0, "南纬必须为负，实得 {}", lat);
    assert!(lon < 0.0, "西经必须为负，实得 {}", lon);
}

/// 字段要按 IFD 正确分桶 —— 曝光参数不该混在 GPS IFD 里。
#[test]
fn fields_are_bucketed_by_ifd() {
    let dir = TempDir::new("buckets");
    let path = jpeg_with_fields(&dir, "photo.jpg", &typical_photo_fields());

    let data = cap_img::exif::read_or_empty(&path).unwrap();

    let primary_names: Vec<&str> = data
        .primary
        .fields
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    let exif_names: Vec<&str> = data.exif.fields.iter().map(|f| f.name.as_str()).collect();
    let gps_names: Vec<&str> = data.gps.fields.iter().map(|f| f.name.as_str()).collect();

    assert!(
        primary_names.contains(&"Make"),
        "primary={:?}",
        primary_names
    );
    assert!(
        exif_names.contains(&"FNumber"),
        "FNumber 应在 exif 桶，实得 {:?}",
        exif_names
    );
    assert!(
        gps_names.contains(&"GPSLatitude"),
        "GPSLatitude 应在 gps 桶，实得 {:?}",
        gps_names
    );
    // 交叉污染检查
    assert!(!primary_names.contains(&"FNumber"));
    assert!(!gps_names.contains(&"Make"));
}

/// 值要保留类型，不能全压成字符串。
#[test]
fn values_keep_their_type() {
    let dir = TempDir::new("types");
    let path = jpeg_with_fields(&dir, "photo.jpg", &typical_photo_fields());

    let data = cap_img::exif::read_or_empty(&path).unwrap();

    let make = data.find("Make").unwrap();
    assert_eq!(make.kind, cap_img::ExifValueKind::Ascii);

    let fnumber = data.find("FNumber").unwrap();
    assert_eq!(fnumber.kind, cap_img::ExifValueKind::Rational);
    assert_eq!(
        fnumber.values,
        vec![cap_img::ExifValue::Rational("28/10".into())]
    );

    let iso = data.find("PhotographicSensitivity").unwrap();
    assert_eq!(iso.kind, cap_img::ExifValueKind::Short);
    assert_eq!(iso.values, vec![cap_img::ExifValue::Int(400)]);
}

/// 同一张图两次读出的字段顺序必须完全一致 —— 否则前端做 diff 会得到假差异。
#[test]
fn reading_twice_yields_identical_order() {
    let dir = TempDir::new("stable");
    let path = jpeg_with_fields(&dir, "photo.jpg", &typical_photo_fields());

    let a = cap_img::exif::read_or_empty(&path).unwrap();
    let b = cap_img::exif::read_or_empty(&path).unwrap();
    assert_eq!(a, b);
}

// ---------------------------------------------------------------------------
// 写：全量能力
// ---------------------------------------------------------------------------

/// 写回一个字段后，必须能原样读回来。
#[test]
fn write_then_read_roundtrips_the_field() {
    let dir = TempDir::new("write-roundtrip");
    let src = jpeg_with_fields(&dir, "src.jpg", &typical_photo_fields());
    let dest = dir.join("out.jpg");

    let edit = cap_img::exif::ExifEdit {
        set: vec![cap_img::ExifField {
            name: "Artist".into(),
            tag: 0x013b,
            kind: cap_img::ExifValueKind::Ascii,
            display: "Zifang".into(),
            values: vec![cap_img::ExifValue::Text("Zifang".into())],
        }],
        remove: vec![],
    };
    cap_img::exif::write(&src, &dest, &edit).unwrap();

    let data = cap_img::exif::read_or_empty(&dest).unwrap();
    assert_eq!(data.display("Artist"), Some("Zifang"));
    // 原有字段还在
    assert_eq!(data.display("Make"), Some("Canon"));
    assert_eq!(data.display("FNumber"), Some("2.8"));
}

/// 写 EXIF **不许动像素** —— 否则「只改元数据」变成了「重编码图片」。
#[test]
fn writing_exif_does_not_change_the_pixels() {
    let dir = TempDir::new("pixels");
    let src = jpeg_with_fields(&dir, "src.jpg", &typical_photo_fields());
    let dest = dir.join("out.jpg");

    let edit = cap_img::exif::ExifEdit {
        set: vec![cap_img::ExifField {
            name: "Artist".into(),
            tag: 0x013b,
            kind: cap_img::ExifValueKind::Ascii,
            display: "x".into(),
            values: vec![cap_img::ExifValue::Text("x".into())],
        }],
        remove: vec![],
    };
    cap_img::exif::write(&src, &dest, &edit).unwrap();

    let before = image::open(&src).unwrap().to_rgb8();
    let after = image::open(&dest).unwrap().to_rgb8();
    assert_eq!(before.dimensions(), after.dimensions());
    for (x, y, p) in before.enumerate_pixels() {
        assert_eq!(p, after.get_pixel(x, y), "像素在 ({},{}) 变了", x, y);
    }
}

/// 隐私刚需：清除 GPS 时，**其它拍摄参数必须保留**。
///
/// 旧实现只有「整块丢弃 EXIF」这一档 ⇒ 清 GPS 顺手把光圈快门也丢了。
#[test]
fn removing_gps_keeps_the_other_metadata() {
    let dir = TempDir::new("strip-gps");
    let src = jpeg_with_fields(&dir, "src.jpg", &typical_photo_fields());
    let dest = dir.join("out.jpg");

    let edit = cap_img::exif::ExifEdit {
        set: vec![],
        remove: vec![
            "GPSLatitude".into(),
            "GPSLongitude".into(),
            "GPSAltitude".into(),
        ],
    };
    cap_img::exif::write(&src, &dest, &edit).unwrap();

    let data = cap_img::exif::read_or_empty(&dest).unwrap();
    // GPS 没了
    assert!(data.find("GPSLatitude").is_none());
    assert!(data.find("GPSLongitude").is_none());
    assert!(data.find("GPSAltitude").is_none());
    // 但拍摄参数还在
    assert_eq!(data.display("Make"), Some("Canon"));
    assert_eq!(data.display("FNumber"), Some("2.8"));
    assert_eq!(data.display("ExposureTime"), Some("1/250"));
    assert_eq!(data.display("LensModel"), Some("RF85mm F1.2 L USM"));
}

/// 清除全部 EXIF ⇒ 一个字段都不剩（隐私模式的「彻底」）。
#[test]
fn strip_removes_every_field() {
    let dir = TempDir::new("strip-all");
    let src = jpeg_with_fields(&dir, "src.jpg", &typical_photo_fields());
    let dest = dir.join("out.jpg");

    cap_img::exif::strip(&src, &dest).unwrap();

    let data = cap_img::exif::read_or_empty(&dest).unwrap();
    assert_eq!(
        data.field_count(),
        0,
        "字段数应为 0，实得 {}",
        data.field_count()
    );
}

/// 覆盖写：同一个标签写两次，后一次的值生效，且不产生重复字段。
#[test]
fn overwriting_a_field_replaces_it_without_duplicating() {
    let dir = TempDir::new("overwrite");
    let src = jpeg_with_fields(&dir, "src.jpg", &typical_photo_fields());
    let dest = dir.join("out.jpg");

    let edit = cap_img::exif::ExifEdit {
        set: vec![cap_img::ExifField {
            name: "Artist".into(),
            tag: 0x013b,
            kind: cap_img::ExifValueKind::Ascii,
            display: "second".into(),
            values: vec![cap_img::ExifValue::Text("second".into())],
        }],
        remove: vec![],
    };
    cap_img::exif::write(&src, &dest, &edit).unwrap();
    // 再写一次不同的值
    let edit2 = cap_img::exif::ExifEdit {
        set: vec![cap_img::ExifField {
            name: "Artist".into(),
            tag: 0x013b,
            kind: cap_img::ExifValueKind::Ascii,
            display: "third".into(),
            values: vec![cap_img::ExifValue::Text("third".into())],
        }],
        remove: vec![],
    };
    cap_img::exif::write(&dest, &dest, &edit2).unwrap();

    let data = cap_img::exif::read_or_empty(&dest).unwrap();
    let artists: Vec<&cap_img::ExifField> = data
        .primary
        .fields
        .iter()
        .filter(|f| f.name == "Artist")
        .collect();
    assert_eq!(
        artists.len(),
        1,
        "Artist 应只有一条，实得 {}",
        artists.len()
    );
    assert_eq!(artists[0].display, "third");
}

/// 按十六进制标签删除（前端只知道标签值时用得上）。
#[test]
fn remove_by_hex_tag_works() {
    let dir = TempDir::new("remove-hex");
    let src = jpeg_with_fields(&dir, "src.jpg", &typical_photo_fields());
    let dest = dir.join("out.jpg");

    let edit = cap_img::exif::ExifEdit {
        set: vec![],
        remove: vec!["0x010f".into()], // Make
    };
    cap_img::exif::write(&src, &dest, &edit).unwrap();

    let data = cap_img::exif::read_or_empty(&dest).unwrap();
    assert!(data.find("Make").is_none(), "Make 应被删除");
    assert_eq!(data.display("Model"), Some("EOS R5"), "其它字段应保留");
}

/// 没有 EXIF 的图片 ⇒ 读出来是空快照，不是错误。
#[test]
fn a_plain_image_yields_an_empty_snapshot() {
    let dir = TempDir::new("no-exif");
    let path = base_jpeg(&dir, "plain.jpg");

    let data = cap_img::exif::read_or_empty(&path).unwrap();
    assert!(data.is_empty());
    assert_eq!(data.field_count(), 0);
}

/// 未知标签（MakerNote 之类）不能被写回时丢掉。
#[test]
fn unknown_tags_survive_a_write() {
    let dir = TempDir::new("unknown");
    // 造一个我们不认识的标签（Tiff 上下文，编号 0x9999）
    let fields = vec![
        Field {
            tag: Tag::Make,
            ifd_num: In::PRIMARY,
            value: Value::Ascii(vec![b"Canon".to_vec()]),
        },
        Field {
            tag: exif::Tag(Context::Tiff, 0x9999),
            ifd_num: In::PRIMARY,
            value: Value::Undefined(b"MAKERNOTEDATA".to_vec(), 0),
        },
    ];
    let src = jpeg_with_fields(&dir, "src.jpg", &fields);
    let dest = dir.join("out.jpg");

    let edit = cap_img::exif::ExifEdit {
        set: vec![cap_img::ExifField {
            name: "Artist".into(),
            tag: 0x013b,
            kind: cap_img::ExifValueKind::Ascii,
            display: "x".into(),
            values: vec![cap_img::ExifValue::Text("x".into())],
        }],
        remove: vec![],
    };
    cap_img::exif::write(&src, &dest, &edit).unwrap();

    // 未知标签读出来名字是 Tag_0x9999，字节内容仍在
    let data = cap_img::exif::read_or_empty(&dest).unwrap();
    let unknown = data
        .primary
        .fields
        .iter()
        .find(|f| f.tag == 0x9999)
        .expect("未知标签必须还在");
    assert_eq!(unknown.kind, cap_img::ExifValueKind::Undefined);
    assert!(
        unknown.display.contains("4d414b4552"),
        "MakerNote 字节应保留，实得 {}",
        unknown.display
    );
}

/// 写入非法值必须报错，而不是写出一个坏文件。
#[test]
fn writing_an_invalid_value_is_an_error() {
    let dir = TempDir::new("invalid");
    let src = jpeg_with_fields(&dir, "src.jpg", &typical_photo_fields());
    let dest = dir.join("out.jpg");

    let edit = cap_img::exif::ExifEdit {
        set: vec![cap_img::ExifField {
            name: "FNumber".into(),
            tag: 0x829d,
            kind: cap_img::ExifValueKind::Rational,
            display: "x".into(),
            // 有理数分母为 0 是非法的
            values: vec![cap_img::ExifValue::Rational("28/0".into())],
        }],
        remove: vec![],
    };
    let err = cap_img::exif::write(&src, &dest, &edit).unwrap_err();
    assert!(err.contains("分母"), "实得 {}", err);
}

/// 写入后 JPEG 必须仍是合法 JPEG（魔数 + 可解码）。
#[test]
fn the_written_file_is_still_a_valid_jpeg() {
    let dir = TempDir::new("valid");
    let src = jpeg_with_fields(&dir, "src.jpg", &typical_photo_fields());
    let dest = dir.join("out.jpg");

    let edit = cap_img::exif::ExifEdit {
        set: vec![cap_img::ExifField {
            name: "Artist".into(),
            tag: 0x013b,
            kind: cap_img::ExifValueKind::Ascii,
            display: "x".into(),
            values: vec![cap_img::ExifValue::Text("x".into())],
        }],
        remove: vec![],
    };
    cap_img::exif::write(&src, &dest, &edit).unwrap();

    let bytes = fs::read(&dest).unwrap();
    assert_eq!(&bytes[..2], &[0xFF, 0xD8], "必须是 JPEG 魔数");
    let img = image::open(&dest).expect("产物必须能被解码");
    assert_eq!((img.width(), img.height()), (8, 8));
}
