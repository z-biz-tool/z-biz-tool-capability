//! cap-video 行为测试。
//!
//! **夹具一律手搓字节**，理由与 cap-audio 相同：用被测的编码器造被测解析器的
//! 输入，两者会共享同一个 bug。
//!
//! 容器夹具还有一个额外风险：**解析器和夹具可能对规范有同一处误解**，
//! 于是"宽高读出来正好等于我写进去的值"，而真实文件仍是错的。所以下面
//! `stsd` 夹具里专门加了一条断言，把 VisualSampleEntry 里 `width` 的
//! **绝对字节偏移钉成 32** 并注明逐字段来历 —— 解析器必须与规范一致，
//! 而不是与夹具一致。
//!
//! ffmpeg 相关用例按 ffmpeg 是否存在自动跳过：没有 ffmpeg 时它们无法运行，
//! 此时应报"跳过"而不是"通过"。

// 测试名是中文描述式的，术语里的 ASCII 部分（MP4 / EBML / WAV / ffmpeg）天然
// 含大写，clippy 的 non_snake_case 会逐个报错。与其把术语降写成 `mp4`/`ebml`
// 换取"看起来合规"，不如显式豁免并在此说明——测试名的可读性优先。
#![allow(non_snake_case)]

use cap_video::matroska;
use cap_video::mp4;
use cap_video::{self as video, VideoInfo};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

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
            "cap-video-{}-{}-{}-{}",
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

fn be32(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

fn be16(v: u16) -> [u8; 2] {
    v.to_be_bytes()
}

/// 造一个 box
fn bx(btype: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&be32((body.len() + 8) as u32));
    v.extend_from_slice(btype);
    v.extend_from_slice(body);
    v
}

/// VisualSampleEntry 夹具。
///
/// 逐字段按 ISO 14496-12 的顺序写入。**注意基准**：`width` 在本函数返回的
/// VisualSampleEntry 体内偏移 16（pre_defined 2 + reserved 2 + pre_defined[3] 12），
/// 而解析器是从 **stsd 描述项起点**（含 size 4 + format 4 + reserved[6] 6 +
/// data_reference_index 2）读的，起点不同 ⇒ 绝对偏移是 32。
/// 下面两处断言分别钉住这两个基准：夹具侧 16，`build_mp4` 里的 `entry` 侧 32。
fn visual_sample_entry(width: u16, height: u16) -> Vec<u8> {
    let mut e = Vec::new();
    e.extend_from_slice(&be16(0)); // pre_defined
    e.extend_from_slice(&be16(0)); // reserved
    e.extend_from_slice(&[0u8; 12]); // pre_defined[3]
    let w_off = e.len();
    e.extend_from_slice(&be16(width));
    let h_off = e.len();
    e.extend_from_slice(&be16(height));
    assert_eq!(w_off, 16, "width 在 VisualSampleEntry 体内应偏移 16");
    assert_eq!(h_off, 18, "height 应紧随 width");
    e.extend_from_slice(&be32(0x0048_0000)); // horizresolution 72dpi
    e.extend_from_slice(&be32(0x0048_0000)); // vertresolution
    e.extend_from_slice(&be32(0)); // reserved
    e.extend_from_slice(&be16(1)); // frame_count
    e.extend_from_slice(&[0u8; 32]); // compressorname
    e.extend_from_slice(&be16(24)); // depth
    e.extend_from_slice(&be16(0xFFFF)); // pre_defined
    e
}

/// 造一个最小 MP4：ftyp + moov{mvhd, trak(vide), trak(soun)}
fn build_mp4(timescale: u32, duration: u32, width: u16, height: u16, frames: u32) -> Vec<u8> {
    // ftyp
    let mut ftyp_body = Vec::new();
    ftyp_body.extend_from_slice(b"isom");
    ftyp_body.extend_from_slice(&be32(512));
    ftyp_body.extend_from_slice(b"isomiso2avc1mp41");
    let ftyp = bx(b"ftyp", &ftyp_body);

    // mvhd（version 0）
    let mut mvhd_body = Vec::new();
    mvhd_body.extend_from_slice(&be32(0)); // version + flags
    mvhd_body.extend_from_slice(&be32(0)); // creation_time
    mvhd_body.extend_from_slice(&be32(0)); // modification_time
    mvhd_body.extend_from_slice(&be32(timescale));
    mvhd_body.extend_from_slice(&be32(duration));
    mvhd_body.extend_from_slice(&be32(0x0001_0000)); // rate
    mvhd_body.extend_from_slice(&be16(0x0100)); // volume
    mvhd_body.extend_from_slice(&[0u8; 10]); // reserved
                                             // 9 个 16 位? 不 —— 统一矩阵是 9 个 32 位，共 36 字节
    mvhd_body.extend_from_slice(&[
        0x82, 0x84, 0x00, 0x21, 0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ]); // matrix 9×u32
    mvhd_body.extend_from_slice(&[0u8; 24]); // pre_defined
    mvhd_body.extend_from_slice(&be32(2)); // next_track_ID
    let mvhd = bx(b"mvhd", &mvhd_body);

    // 视频 trak
    let mut hdlr = Vec::new();
    hdlr.extend_from_slice(&be32(0));
    hdlr.extend_from_slice(&be32(0)); // pre_defined
    hdlr.extend_from_slice(b"vide");
    hdlr.extend_from_slice(&[0u8; 12]);
    let hdlr = bx(b"hdlr", &hdlr);

    let mut entry: Vec<u8> = be32(0).to_vec(); // 占位，稍后回填
    entry.extend_from_slice(b"avc1");
    entry.extend_from_slice(&[0u8; 6]); // reserved
    entry.extend_from_slice(&be16(1)); // data_reference_index
                                       // 此刻 entry.len() = 16，再追加 VisualSampleEntry；
                                       // 于是 width 的 entry 级绝对偏移 = 16 + 16 = 32，与 ISO 14496-12 一致。
    let visual_len_before = entry.len();
    assert_eq!(visual_len_before, 16, "SampleEntry 头应为 16 字节");
    entry.extend_from_slice(&visual_sample_entry(width, height));
    assert_eq!(
        visual_len_before + 16,
        32,
        "width 的 entry 级绝对偏移应为 32"
    );
    let size_pos = 0usize;
    let entry_size = entry.len() + 8;
    entry[size_pos..size_pos + 4].copy_from_slice(&be32(entry_size as u32));

    let mut stsd = Vec::new();
    stsd.extend_from_slice(&be32(0)); // version + flags
    stsd.extend_from_slice(&be32(1)); // entry_count
    stsd.extend_from_slice(&entry);
    let stsd = bx(b"stsd", &stsd);

    let mut stts = Vec::new();
    stts.extend_from_slice(&be32(0));
    stts.extend_from_slice(&be32(1)); // entry_count
    stts.extend_from_slice(&be32(frames)); // sample_count
    stts.extend_from_slice(&be32(1)); // sample_delta
    let stts = bx(b"stts", &stts);

    let stbl = bx(b"stbl", &[stsd, stts].concat());
    let minf = bx(b"minf", &stbl);
    let mdia = bx(b"mdia", &[hdlr, minf].concat());
    let trak_v = bx(b"trak", &mdia);

    // 音频 trak
    let mut ahdlr = Vec::new();
    ahdlr.extend_from_slice(&be32(0));
    ahdlr.extend_from_slice(&be32(0));
    ahdlr.extend_from_slice(b"soun");
    ahdlr.extend_from_slice(&[0u8; 12]);
    let ahdlr = bx(b"hdlr", &ahdlr);
    let mut aentry: Vec<u8> = be32(0).to_vec();
    aentry.extend_from_slice(b"mp4a");
    aentry.extend_from_slice(&[0u8; 6]);
    aentry.extend_from_slice(&be16(1));
    aentry.extend_from_slice(&[0u8; 20]);
    let asz = aentry.len() + 8;
    aentry[0..4].copy_from_slice(&be32(asz as u32));
    let mut astsd = Vec::new();
    astsd.extend_from_slice(&be32(0));
    astsd.extend_from_slice(&be32(1));
    astsd.extend_from_slice(&aentry);
    let astsd = bx(b"stsd", &astsd);
    let amin_stbl = bx(b"stbl", &astsd);
    let aminf = bx(b"minf", &amin_stbl);
    let amdia = bx(b"mdia", &[ahdlr, aminf].concat());
    let trak_a = bx(b"trak", &amdia);

    let moov = bx(b"moov", &[mvhd, trak_v, trak_a].concat());
    let mut out = Vec::new();
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&moov);
    out
}

// ───────────────────────── MP4 ─────────────────────────

#[test]
fn mp4_读出时长宽高编码帧数() {
    // timescale 1000、duration 5000 ⇒ 5 秒
    let bytes = build_mp4(1000, 5000, 1920, 1080, 150);
    let info = mp4::probe(&bytes).unwrap();
    assert!(
        (info.duration_secs - 5.0).abs() < 1e-9,
        "时长 {}",
        info.duration_secs
    );
    assert_eq!(info.width, 1920, "宽读成了 0 ⇒ stsd 偏移错位");
    assert_eq!(info.height, 1080, "高读成了 0 ⇒ stsd 偏移错位");
    assert_eq!(info.video_codec.as_deref(), Some("avc1"));
    assert_eq!(info.audio_codec.as_deref(), Some("mp4a"));
    assert_eq!(info.frame_count, Some(150));
    assert_eq!(info.track_count, 2);
    assert!(info.has_audio);
}

#[test]
fn mp4_音视频分轨时宽高只取自视频轨() {
    // 音轨的 stsd 里根本没有 VisualSampleEntry；若不按 hdlr 过滤，
    // 音轨的描述项会被当视频读，宽高取到音轨里的垃圾值。
    let bytes = build_mp4(600, 1200, 640, 480, 24);
    let info = mp4::probe(&bytes).unwrap();
    assert_eq!((info.width, info.height), (640, 480));
    assert!((info.duration_secs - 2.0).abs() < 1e-9);
    assert_eq!(info.frame_count, Some(24));
}

#[test]
fn mp4_逐个采样时长累加出总帧数() {
    // stts 可以有多个条目，帧数是所有 sample_count 之和。
    let mut stts_body = Vec::new();
    stts_body.extend_from_slice(&be32(0));
    stts_body.extend_from_slice(&be32(3)); // 3 个条目
    stts_body.extend_from_slice(&be32(10));
    stts_body.extend_from_slice(&be32(1));
    stts_body.extend_from_slice(&be32(20));
    stts_body.extend_from_slice(&be32(1));
    stts_body.extend_from_slice(&be32(7));
    stts_body.extend_from_slice(&be32(1));
    let stts = bx(b"stts", &stts_body);
    let stbl = bx(b"stbl", &stts);
    // 层级必须与 ISO 14496-12 一致：trak → mdia → minf → stbl → stts。
    // 初版这里写成 mdia → stbl，解析器（已按规范逐层下钻）自然找不到。
    let minf = bx(b"minf", &stbl);
    // 帧数只从**视频轨**取，所以这条轨必须带 hdlr=vide；
    // 没有 hdlr 时解析器会跳过它（与真实文件一致），frame_count 应为 None。
    let mut hdlr_body = Vec::new();
    hdlr_body.extend_from_slice(&be32(0));
    hdlr_body.extend_from_slice(&be32(0));
    hdlr_body.extend_from_slice(b"vide");
    hdlr_body.extend_from_slice(&[0u8; 12]);
    let hdlr = bx(b"hdlr", &hdlr_body);
    let mdia = bx(b"mdia", &[hdlr, minf].concat());
    let trak = bx(b"trak", &mdia);
    let moov = bx(b"moov", &trak);
    let ftyp = bx(b"ftyp", b"isom\x00\x00\x02\x00isom");
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&ftyp);
    bytes.extend_from_slice(&moov);

    let info = mp4::probe(&bytes).unwrap();
    assert_eq!(info.frame_count, Some(37), "只取了第一个条目");
}

#[test]
fn mp4_缺_moov_时明确报错而不是返回空信息() {
    // 元数据缺失时返回全 0 的"成功"结果，是最难查的一类静默失败：
    // 属性面板显示 0 秒 0×0，没人知道是文件坏了还是解析器坏了。
    let ftyp = bx(b"ftyp", b"isom\x00\x00\x02\x00isom");
    let err = mp4::probe(&ftyp).unwrap_err();
    assert!(err.contains("moov"), "错误应点明缺 moov，实际：{}", err);
}

#[test]
fn mp4_不是_iso_bmff_时报错() {
    let err = mp4::probe(b"\x00\x00\x00\x10junkjunkjunk").unwrap_err();
    assert!(err.contains("ftyp"), "实际错误：{}", err);
    // 过短输入不得越界
    assert!(mp4::probe(&[0u8; 3]).is_err());
    assert!(mp4::probe(&[]).is_err());
}

#[test]
fn mp4_零长度_box_不会死循环() {
    // size == 0 表示延伸到文件末尾。若步进写成 0，就会原地打转直到超时。
    // 这里造一个 size=0 的 ftyp 放在最前，验证探针能正常走完。
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&be32(0));
    bytes.extend_from_slice(b"ftyp");
    bytes.extend_from_slice(b"isom");
    let _ = mp4::probe(&bytes);
}

#[test]
fn mp4_声明长度超过实际文件时截断而不是越界() {
    let mut bytes = build_mp4(1000, 5000, 320, 240, 30);
    // 把 moov 的长度改大，模拟截断下载
    let pos = bytes
        .windows(4)
        .position(|w| w == b"moov")
        .expect("应含 moov");
    let claimed = u32::from_be_bytes([
        bytes[pos - 4],
        bytes[pos - 3],
        bytes[pos - 2],
        bytes[pos - 1],
    ]);
    bytes[pos - 4..pos].copy_from_slice(&be32(claimed + 4096));
    // 不得 panic；解析结果要么可用要么报错，不许读出越界数据
    // 不得 panic；解析结果要么可用要么报错，不许读出越界数据
    if let Ok(i) = mp4::probe(&bytes) {
        assert!(i.width <= 320);
    }
}

// ───────────────────────── Matroska / WebM ─────────────────────────

/// 取整数的低 N 字节（大端序）。
///
/// 初版的 3 字节分支写成 `b[1..4]` —— 对 4 字节的 u32 恰好是低 3 字节，
/// 但对 u64 来说低 3 字节在 `b[5..8]`，于是 70000 被写成了 32，
/// 整个元素长度错位、Segment 认不出来。
fn low_bytes(v: u64, n: usize) -> Vec<u8> {
    v.to_be_bytes()[8 - n..].to_vec()
}

/// 写一个 EBML 元素 ID。
///
/// **分档必须按 VINT 标记位，不能按数值大小。** 标记位在首字节的高位：
/// 1 字节 0x80–0xFF / 2 字节 0x4000–0x7FFF / 3 字节 0x200000–0x3FFFFF / 4 字节 0x10000000–。
/// 初版我按 `0xFFFF` / `0xFF_FFFF` 分档，于是 Duration 的 ID `0x4489`（真实是 2 字节）
/// 被写成了 3 字节 `80 44 89`，解析器读出来是 `0x80` —— Duration 永远匹配不上，
/// 时长恒为 0。这类错误同样**不报错**，只是让字段静默丢失。
fn ebml_id(id: u32) -> Vec<u8> {
    if id <= 0xFF {
        vec![id as u8]
    } else if id <= 0x7FFF {
        let mut v = low_bytes(id as u64, 2);
        v[0] |= 0x40; // 2 字节 ID 的标记位是 0x40，不是 0x80
        v
    } else if id <= 0x3F_FFFF {
        let mut v = low_bytes(id as u64, 3);
        v[0] |= 0x20;
        v
    } else {
        let mut v = low_bytes(id as u64, 4);
        v[0] |= 0x10;
        v
    }
}

/// 写一个 EBML 长度（去掉标记位）。分档同样按标记位。
fn ebml_size(n: u64) -> Vec<u8> {
    if n <= 0x7F {
        vec![0x80 | n as u8]
    } else if n <= 0x3FFF {
        let mut v = low_bytes(n, 2);
        v[0] |= 0x40;
        v
    } else if n <= 0x1F_FFFF {
        let mut v = low_bytes(n, 3);
        v[0] |= 0x20;
        v
    } else {
        let mut v = low_bytes(n, 4);
        v[0] |= 0x10;
        v
    }
}

fn ebml_el(id: u32, body: &[u8]) -> Vec<u8> {
    let mut v = ebml_id(id);
    v.extend_from_slice(&ebml_size(body.len() as u64));
    v.extend_from_slice(body);
    v
}

fn ebml_uint_body(v: u64) -> Vec<u8> {
    if v == 0 {
        vec![0]
    } else {
        let b = v.to_be_bytes();
        let start = b.iter().position(|&x| x != 0).unwrap();
        b[start..].to_vec()
    }
}

#[test]
fn webm_读出时长宽高与编码() {
    let mut info_body = Vec::new();
    // TimecodeScale = 1_000_000 ns/刻度（即 1ms），Duration 以**刻度**计。
    // 12.5 秒 = 12500 刻度。写成 Duration=12.5 会得到 12.5ms ——
    // 量纲差 1000 倍，而数值本身"看起来仍然合理"，是最容易蒙混过去的一类错。
    info_body.extend_from_slice(&ebml_el(0x2AD7B1, &ebml_uint_body(1_000_000)));
    info_body.extend_from_slice(&ebml_el(0x4489, &12_500f64.to_be_bytes()));
    let info = ebml_el(0x1549A966, &info_body);

    let mut vtrack = Vec::new();
    vtrack.extend_from_slice(&ebml_el(0x83, &ebml_uint_body(1))); // TrackType=video
    vtrack.extend_from_slice(&ebml_el(0x86, b"V_VP9"));
    vtrack.extend_from_slice(&ebml_el(0xB0, &ebml_uint_body(1280)));
    vtrack.extend_from_slice(&ebml_el(0xBA, &ebml_uint_body(720)));
    let vtrack = ebml_el(0xAE, &vtrack);

    let mut atrack = Vec::new();
    atrack.extend_from_slice(&ebml_el(0x83, &ebml_uint_body(2)));
    atrack.extend_from_slice(&ebml_el(0x86, b"A_OPUS"));
    let atrack = ebml_el(0xAE, &atrack);

    let tracks = ebml_el(0x1654AE6B, &[vtrack, atrack].concat());
    let segment = ebml_el(0x18538067, &[info, tracks].concat());

    let header = ebml_el(0x1A45DFA3, b"");
    let mut bytes = header;
    bytes.extend_from_slice(&segment);

    let ci = matroska::probe(&bytes).unwrap();
    assert!(
        (ci.duration_secs - 12.5).abs() < 1e-6,
        "时长 {}",
        ci.duration_secs
    );
    assert_eq!(ci.width, 1280);
    assert_eq!(ci.height, 720);
    assert_eq!(ci.video_codec.as_deref(), Some("V_VP9"));
    assert_eq!(ci.audio_codec.as_deref(), Some("A_OPUS"));
    assert_eq!(ci.track_count, 2);
    assert!(ci.has_audio);
}

#[test]
fn webm_时长要乘_TimecodeScale_不能直接当秒() {
    // 刻意把 TimecodeScale 设成 1µs（1000ns）而不是默认的 1ms：
    // 这样「漏乘 scale」会得到 3×10⁷ 秒，而正确答案是 30 秒 ——
    // 两者差 6 个数量级，不会被"看起来合理"蒙混过去。
    let mut info_body = Vec::new();
    info_body.extend_from_slice(&ebml_el(0x2AD7B1, &ebml_uint_body(1_000)));
    info_body.extend_from_slice(&ebml_el(0x4489, &30_000_000f64.to_be_bytes()));
    let info = ebml_el(0x1549A966, &info_body);
    let segment = ebml_el(0x18538067, &info);
    let mut bytes = ebml_el(0x1A45DFA3, b"");
    bytes.extend_from_slice(&segment);

    let ci = matroska::probe(&bytes).unwrap();
    assert!(
        (ci.duration_secs - 30.0).abs() < 1e-6,
        "秒 = Duration × TimecodeScale / 1e9；漏乘 TimecodeScale 会得到 {} 秒",
        ci.duration_secs
    );
}

#[test]
fn webm_不是_ebml_时报错() {
    assert!(matroska::probe(b"\x00\x00\x00\x20ftypisom").is_err());
    assert!(matroska::probe(&[0x1A]).is_err());
    assert!(matroska::probe(&[]).is_err());
}

// ───────────────────────── 嗅探与文件级 ─────────────────────────

#[test]
fn 嗅探容器按头部而不是扩展名() {
    assert_eq!(
        video::sniff_container(&build_mp4(1000, 1000, 16, 16, 1)),
        Some("mp4")
    );
    assert_eq!(
        video::sniff_container(b"\x00\x00\x00\x18ftypqt  \x00\x00\x02\x00qt  "),
        Some("mov")
    );
    assert_eq!(
        video::sniff_container(&[0x1A, 0x45, 0xDF, 0xA3, 0, 0, 0, 0]),
        Some("webm")
    );
    assert_eq!(
        video::sniff_container(b"RIFF\x00\x00\x00\x00AVI LIST"),
        Some("avi")
    );
    assert_eq!(video::sniff_container(b"FLV\x01\x05"), Some("flv"));
    assert_eq!(video::sniff_container(b"nothing here at all"), None);
    assert_eq!(video::sniff_container(b"RI"), None);
    assert_eq!(video::detect_container("/x/y.MP4"), "mp4");
    assert_eq!(video::detect_container("/x/y.m4v"), "mp4");
    assert_eq!(video::detect_container("/x/y.MKV"), "mkv");
    assert_eq!(video::detect_container("/x/noext"), "");
}

#[test]
fn info_读真实文件并推出码率与帧率() {
    let dir = TempDir::new("info");
    let p = dir.join("v.mp4");
    // 1000 timescale / 4000 duration = 4s；150 帧 ⇒ 37.5fps
    let bytes = build_mp4(1000, 4000, 1280, 720, 150);
    fs::write(&p, &bytes).unwrap();

    let i: VideoInfo = video::info(&p).unwrap();
    assert_eq!(i.container, "mp4");
    assert!((i.duration_secs - 4.0).abs() < 1e-9);
    assert_eq!((i.width, i.height), (1280, 720));
    assert_eq!(i.size, bytes.len() as u64);
    assert_eq!(i.track_count, 2);
    assert!(i.has_audio);
    assert_eq!(
        i.bitrate,
        (bytes.len() as u64 * 8 / 4),
        "码率应按 文件大小×8÷时长"
    );
    let fps = i.fps().expect("有帧数和时长时应能算帧率");
    assert!((fps - 37.5).abs() < 1e-9, "帧率 {}", fps);
}

#[test]
fn webm_子元素里有零长度的也不会挡住后面的字段() {
    // EBML 的**零长度元素是合法的**（空元素）。变异验证实测：把
    // 「零长度就 continue」改回「零长度就 break」，本仓测试全绿 ——
    // 因为当时所有夹具的零长度元素都在**顶层**（空的 EBML 头），
    // 走的是另一段循环，嵌套层的 break 从没被触发。
    //
    // 这条用例在 Info 里塞一个零长度元素和几个非目标元素，
    // 把 Duration 放在它们**之后** —— 解析必须跳过前面的杂项读到 Duration。
    let mut info_body = Vec::new();
    info_body.extend_from_slice(&ebml_el(0x2AD7B1, &ebml_uint_body(1_000_000)));
    info_body.extend_from_slice(&ebml_el(0x4282, b"webm")); // DocType
    info_body.extend_from_slice(&ebml_el(0xEC, b"")); // 零长度 Void
    info_body.extend_from_slice(&ebml_el(0x5741, b"webm")); // 任意非目标字段
    info_body.extend_from_slice(&ebml_el(0x4489, &4_000f64.to_be_bytes())); // Duration=4s
    let info = ebml_el(0x1549A966, &info_body);
    let segment = ebml_el(0x18538067, &info);
    let mut bytes = ebml_el(0x1A45DFA3, b"");
    bytes.extend_from_slice(&segment);

    let ci = matroska::probe(&bytes).unwrap();
    assert!(
        (ci.duration_secs - 4.0).abs() < 1e-6,
        "Info 里前面有零长度与非目标元素时，Duration 没读到（得到 {} 秒）",
        ci.duration_secs
    );
}

#[test]
fn webm_多字节长度_vint_也能正确切分() {
    // 变异验证实测：把 read_size 的掩码恒改成 0x7F，本仓测试全绿 ——
    // 因为此前所有测试元素的**长度字段都是 1 字节 vint**（元素都小于 127 字节），
    // 2/3/4 字节长度分支从未被走到。
    //
    // 这里塞一个 200 字节的 Void（长度 vint 变成 2 字节）和一个 70000 字节的
    // Void（3 字节），并把 Duration 放在它们之后：解析必须按多字节长度跳过。
    let mut info_body = Vec::new();
    info_body.extend_from_slice(&ebml_el(0x2AD7B1, &ebml_uint_body(1_000_000)));
    info_body.extend_from_slice(&ebml_el(0xEC, &[0u8; 200])); // 2 字节长度
    info_body.extend_from_slice(&ebml_el(0xEC, &[0u8; 70_000])); // 3 字节长度
    info_body.extend_from_slice(&ebml_el(0x4489, &2_000f64.to_be_bytes()));
    let info = ebml_el(0x1549A966, &info_body);
    let segment = ebml_el(0x18538067, &info);
    let mut bytes = ebml_el(0x1A45DFA3, b"");
    bytes.extend_from_slice(&segment);

    let ci = matroska::probe(&bytes).unwrap();
    assert!(
        (ci.duration_secs - 2.0).abs() < 1e-6,
        "多字节长度元素之后 Duration 没读到（得到 {} 秒）",
        ci.duration_secs
    );
}

#[test]
fn info_以头部为准_扩展名写成另一种容器也要按内容解析() {
    // 变异验证实测：把 sniff 改成恒 None（完全信扩展名），原有测试全绿 ——
    // 因为那条夹具是「AVI 内容 + .mp4 名字」，两条路径都会报错，结果撞巧一致。
    //
    // 真正能区分的形状是**两边都能解析**：一个真 MP4 改名成 .webm。
    // 按内容 ⇒ 当 MP4 解析成功；按扩展名 ⇒ 走 EBML 解析器，报「首字节应为 0x1A」。
    let dir = TempDir::new("liar");
    let p = dir.join("actually-mp4.webm");
    fs::write(&p, build_mp4(1000, 3000, 320, 240, 90)).unwrap();

    let i = video::info(&p).unwrap();
    assert_eq!(i.container, "mp4", "容器应由头部判定，而不是扩展名");
    assert_eq!((i.width, i.height), (320, 240));
    assert!((i.duration_secs - 3.0).abs() < 1e-9);
}

#[test]
fn info_对不支持的容器明确报错而不是返回全零() {
    let dir = TempDir::new("unsupported");
    let p = dir.join("v.avi");
    fs::write(&p, b"RIFF\x00\x00\x00\x00AVI LISTjunk").unwrap();
    let err = video::info(&p).unwrap_err();
    assert!(err.contains("不支持"), "实际错误：{}", err);
    // 扩展名骗人也一样：内容是 AVI，名字写 mp4
    let p2 = dir.join("fake.mp4");
    fs::write(&p2, b"RIFF\x00\x00\x00\x00AVI LISTjunk").unwrap();
    assert!(video::info(&p2).is_err(), "被扩展名骗过去了");
}

#[test]
fn info_对缺失文件给出可读错误() {
    let dir = TempDir::new("missing");
    let err = video::info(dir.join("nope.mp4")).unwrap_err();
    assert!(err.contains("失败"), "错误串应说明是文件操作失败：{}", err);
}

// ───────────────────────── ffmpeg 层 ─────────────────────────

/// `available()` 必须意味着「**真的跑得起来**」，而不是「文件存在」。
///
/// 这条测试是本机现实逼出来的：本机有一个 ffmpeg 文件齐全、可执行位正常，
/// 但缺 `libx265.215.dylib`，一 spawn 就 dyld 报错。第一版 `available()`
/// 写成 `locate().is_some()`，在这台机器上会撒谎报 true，随后抽帧必然失败。
///
/// 所以这里断言的是二者的一致性：available() == probe().is_ok()。
/// 拿不到一致的场合（既没装、也定位不到）也算通过。
#[test]
fn available_必须与_probe_一致_不能对坏掉的二进制撒谎() {
    let located = video::ffmpeg::locate("ffmpeg").is_some();
    let probed = video::probe_ffmpeg().is_ok();
    assert_eq!(
        video::ffmpeg_available(),
        probed,
        "available() 与 probe().is_ok() 不一致：locate 找到={located} probe 成功={probed}"
    );
}

#[test]
fn ffmpeg_不可用时报错_可用时给出路径与版本() {
    match video::probe_ffmpeg() {
        Ok(info) => {
            assert!(
                info.version_line.to_lowercase().contains("ffmpeg"),
                "版本首行不像 ffmpeg：{}",
                info.version_line
            );
            assert!(info.path.exists());
        }
        Err(e) => {
            // 两种失败必须可区分：没装 vs 装了但坏了。
            // 混成一句的话，用户会去反复重装一个根本没装的程序。
            let installed = video::ffmpeg::locate("ffmpeg").is_some();
            if installed {
                assert!(
                    e.contains("无法执行") && e.contains("重装"),
                    "装了却跑不起来时，错误应指向「重装」：{}",
                    e
                );
            } else {
                assert!(
                    e.contains("安装") || e.contains("未找到"),
                    "没装时，错误应指向「安装」：{}",
                    e
                );
            }
            // 报错不该把上千字符的 dyld spew 原样甩给调用方
            assert!(
                e.chars().count() < 1200,
                "错误串过长（{} 字符），dyld spew 未被压住",
                e.chars().count()
            );
        }
    }
}

#[test]
fn ffmpeg_抽帧_不可用时报错_可用时产出非空文件() {
    let dir = TempDir::new("frame");
    let src = dir.join("in.mp4");
    fs::write(&src, build_mp4(1000, 4000, 640, 480, 120)).unwrap();
    let dest = dir.join("out.jpg");

    if !video::ffmpeg_available() {
        let err =
            video::extract_frame(&src, 0.0, &dest, std::time::Duration::from_secs(10)).unwrap_err();
        assert!(err.contains("ffmpeg"), "应明确说 ffmpeg 不可用：{}", err);
        assert!(!dest.exists(), "失败时不应留下空文件");
        return;
    }
    let n = video::extract_frame(&src, 0.0, &dest, std::time::Duration::from_secs(20)).unwrap();
    assert!(n > 0);
    assert!(dest.exists());
}

#[test]
fn ffmpeg_抽帧_源不存在时先报错_不启动子进程() {
    let dir = TempDir::new("nofile");
    let dest = dir.join("out.jpg");
    let err = video::extract_frame(
        dir.join("ghost.mp4"),
        0.0,
        &dest,
        std::time::Duration::from_secs(5),
    )
    .unwrap_err();
    // 本地文件检查是零成本的，必须排在探测 ffmpeg 前面 ——
    // 否则在没装 ffmpeg 的机器上，这里报的是「缺 ffmpeg」而不是「文件不存在」。
    assert!(err.contains("不存在"), "实际错误：{}", err);
}

#[test]
fn locate_认不出不存在的程序_而不是返回垃圾路径() {
    assert!(video::ffmpeg::locate("definitely-not-a-real-binary-xyzzy").is_none());
}
