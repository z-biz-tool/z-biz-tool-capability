//! MP4 / MOV 容器（ISO BMFF）解析。
//!
//! 只读容器元数据，不解码视频/音频负载：宽高、时长、编码、帧数、码率
//! 全部能从 box 树里取到，而这些正是「拖进窗口先给个信息」和「挑一张封面」
//! 需要的。真正的解码交给 ffmpeg（见 [`crate::ffmpeg`]）。
//!
//! box 头的三种尺寸写法都要认：
//!
//! - `size` 为普通 u32；
//! - `size == 1` 时真实长度是紧随其后的 u64（64 位 box，`mdat` 常见）；
//! - `size == 0` 时该 box 一直延伸到文件末尾。
//!
//! 只认前一种的实现在遇到大文件时会立刻错位，而错位之后**不会崩**，
//! 只会安静地读出垃圾值。

/// 容器层能给出的信息
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContainerInfo {
    /// 时长（秒）
    pub duration_secs: f64,
    /// 视频宽（像素）；无视频轨时为 0
    pub width: u32,
    /// 视频高（像素）
    pub height: u32,
    /// 视频编码 fourcc，如 `avc1` / `hvc1` / `vp09`
    pub video_codec: Option<String>,
    /// 音频编码 fourcc，如 `mp4a`
    pub audio_codec: Option<String>,
    /// 视频轨帧数（来自 stts 的样本计数）
    pub frame_count: Option<u64>,
    /// 轨道总数
    pub track_count: u32,
    /// 是否有音频轨
    pub has_audio: bool,
}

fn be_u32(d: &[u8]) -> u32 {
    u32::from_be_bytes([d[0], d[1], d[2], d[3]])
}

fn be_u64(d: &[u8]) -> u64 {
    u64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]])
}

/// 解析 MP4/MOV 容器
pub fn probe(bytes: &[u8]) -> Result<ContainerInfo, String> {
    if bytes.len() < 8 {
        return Err(format!("MP4 头不完整：{} 字节，至少需要 8", bytes.len()));
    }
    let mut info = ContainerInfo::default();
    let mut saw_ftyp = false;
    let mut saw_moov = false;
    let mut i = 0usize;

    while i + 8 <= bytes.len() {
        let size32 = be_u32(&bytes[i..i + 4]);
        let btype = [bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]];
        let (body_start, body_end) = match size32 {
            0 => (i + 8, bytes.len()), // 延伸到文件末尾
            1 => {
                if i + 16 > bytes.len() {
                    break;
                }
                let big = be_u64(&bytes[i + 8..i + 16]);
                let end = (i as u64).saturating_add(big);
                (i + 16, end.min(bytes.len() as u64) as usize)
            }
            s => (
                i + 8,
                (i as u64 + s as u64).min(bytes.len() as u64) as usize,
            ),
        };
        if body_start > body_end {
            break;
        }

        match &btype {
            b"ftyp" => saw_ftyp = true,
            b"moov" => {
                saw_moov = true;
                let body = &bytes[body_start..body_end];
                let (mvhd, tracks) = split_moov(body);
                if let Some(scale_dur) = mvhd.and_then(parse_mvhd) {
                    if scale_dur.1 > 0.0 {
                        info.duration_secs = scale_dur.1 / scale_dur.0 as f64;
                    }
                }
                info.track_count = tracks.len() as u32;
                for raw in tracks {
                    let t = parse_trak(raw);
                    match t.handler.as_slice() {
                        b"vide" => {
                            info.width = t.width;
                            info.height = t.height;
                            info.video_codec = t.codec.clone();
                            if t.frames > 0 {
                                info.frame_count = Some(t.frames);
                            }
                        }
                        b"soun" => {
                            info.has_audio = true;
                            info.audio_codec = t.codec.clone();
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }

        // size32 == 0 时本 box 已到文件末尾
        if size32 == 0 {
            break;
        }
        if body_end <= i {
            break; // 防御：零长度 box 会造成死循环
        }
        i = body_end;
    }

    if !saw_ftyp {
        return Err("不是 MP4/MOV：未见 ftyp box".to_string());
    }
    if !saw_moov {
        return Err("MP4 缺少 moov box（文件可能被截断，或元数据在文件末尾而未读完）".to_string());
    }
    Ok(info)
}

/// moov 里逐个 box 取出 mvhd 与所有 trak
fn split_moov(body: &[u8]) -> (Option<&[u8]>, Vec<&[u8]>) {
    let mut mvhd = None;
    let mut tracks = Vec::new();
    let mut i = 0usize;
    while i + 8 <= body.len() {
        let size32 = be_u32(&body[i..i + 4]);
        let btype = [body[i + 4], body[i + 5], body[i + 6], body[i + 7]];
        let (bs, be) = match size32 {
            0 => (i + 8, body.len()),
            1 => {
                if i + 16 > body.len() {
                    break;
                }
                let big = be_u64(&body[i + 8..i + 16]);
                (i + 16, (i as u64 + big).min(body.len() as u64) as usize)
            }
            s => (i + 8, (i as u64 + s as u64).min(body.len() as u64) as usize),
        };
        if bs > be {
            break;
        }
        match &btype {
            b"mvhd" => mvhd = Some(&body[bs..be]),
            b"trak" => tracks.push(&body[bs..be]),
            _ => {}
        }
        if be <= i {
            break;
        }
        i = be;
    }
    (mvhd, tracks)
}

/// mvhd → (timescale, duration)
fn parse_mvhd(d: &[u8]) -> Option<(u32, f64)> {
    if d.len() < 4 {
        return None;
    }
    let version = d[0];
    if version == 1 {
        if d.len() < 32 {
            return None;
        }
        Some((be_u32(&d[20..24]), be_u64(&d[24..32]) as f64))
    } else {
        if d.len() < 20 {
            return None;
        }
        Some((be_u32(&d[12..16]), be_u32(&d[16..20]) as f64))
    }
}

#[derive(Debug, Default)]
struct Track {
    handler: [u8; 4],
    width: u32,
    height: u32,
    codec: Option<String>,
    frames: u64,
}

fn parse_trak(body: &[u8]) -> Track {
    let mut t = Track::default();
    let mdia = find_box(body, b"mdia").unwrap_or(&[]);
    if let Some(hdlr) = find_box(mdia, b"hdlr") {
        // version+flags(4) pre_defined(4) handler_type(4)
        if hdlr.len() >= 12 {
            t.handler.copy_from_slice(&hdlr[8..12]);
        }
    }
    if let Some(minf) = find_box(mdia, b"minf") {
        if let Some(stbl) = find_box(minf, b"stbl") {
            if let Some(stsd) = find_box(stbl, b"stsd") {
                parse_stsd(stsd, &mut t);
            }
            if let Some(stts) = find_box(stbl, b"stts") {
                t.frames = parse_stts(stts);
            }
        }
    }
    t
}

/// stsd → 编码 fourcc + 宽高
fn parse_stsd(d: &[u8], t: &mut Track) {
    // version+flags(4) entry_count(4)，随后是采样描述项
    if d.len() < 8 {
        return;
    }
    let entry = &d[8..];
    if entry.len() < 8 {
        return;
    }
    let entry_size = be_u32(&entry[0..4]).min(entry.len() as u32) as usize;
    if entry_size < 8 {
        return;
    }
    let codec = String::from_utf8_lossy(&entry[4..8]).trim_end().to_string();
    if !codec.is_empty() {
        t.codec = Some(codec);
    }

    if t.handler == *b"vide" {
        // VisualSampleEntry 相对本描述项起点的绝对偏移，逐字段数（ISO 14496-12）：
        //   0..4   size
        //   4..8   format（fourcc）
        //   8..14  SampleEntry.reserved[6]
        //   14..16 SampleEntry.data_reference_index
        //   16..18 VisualSampleEntry.pre_defined
        //   18..20 VisualSampleEntry.reserved
        //   20..32 VisualSampleEntry.pre_defined[3]
        //   32..34 width
        //   34..36 height
        //
        // 初版这里写的是 24/26 —— 漏算了 reserved[6] + data_reference_index 这 8 字节。
        // 后果很隐蔽：解析出的宽高来自描述项中间那 12 字节 pre_defined（恒为 0），
        // 于是**永远返回 0×0 而不报错**，属性面板上的分辨率就是空的。
        if entry.len() >= 36 {
            t.width = u16::from_be_bytes([entry[32], entry[33]]) as u32;
            t.height = u16::from_be_bytes([entry[34], entry[35]]) as u32;
        }
    }
}

/// stts → 样本总数（帧数）
fn parse_stts(d: &[u8]) -> u64 {
    if d.len() < 8 {
        return 0;
    }
    let count = be_u32(&d[4..8]) as usize;
    let mut total = 0u64;
    let mut i = 8usize;
    for _ in 0..count {
        if i + 8 > d.len() {
            break;
        }
        total = total.saturating_add(be_u32(&d[i..i + 4]) as u64);
        i += 8;
    }
    total
}

/// 在 box 序列里找第一个**直接子级**中指定类型的 box 体。
///
/// 只看直接子级，不递归：递归找会撞上嵌套同名 box（如 moov 里的 trak 与
/// 其它位置的 trak），而 ISO 14496-12 的 box 树本来就是按层级语义定位的。
/// 需要跨层时由调用方逐层下钻（trak → mdia → minf → stbl → stsd）。
fn find_box<'a>(body: &'a [u8], want: &[u8; 4]) -> Option<&'a [u8]> {
    let mut i = 0usize;
    while i + 8 <= body.len() {
        let size32 = be_u32(&body[i..i + 4]);
        let btype = [body[i + 4], body[i + 5], body[i + 6], body[i + 7]];
        let (bs, be) = match size32 {
            0 => (i + 8, body.len()),
            1 => {
                if i + 16 > body.len() {
                    return None;
                }
                let big = be_u64(&body[i + 8..i + 16]);
                (i + 16, (i as u64 + big).min(body.len() as u64) as usize)
            }
            s => (i + 8, (i as u64 + s as u64).min(body.len() as u64) as usize),
        };
        if bs > be {
            return None;
        }
        if &btype == want {
            return Some(&body[bs..be]);
        }
        if be <= i {
            return None;
        }
        i = be;
    }
    None
}
