//! Matroska / WebM（EBML）容器解析。
//!
//! 与 MP4 的 box 树不同，EBML 用变长整数（vint）编码**元素 ID 和长度**，
//! 两者的 vint 规则还不一样：ID 保留标记位，长度要去掉标记位。
//! 这是 EBML 最容易写错的地方，写错之后**不会崩**，只会读出长度完全错位的
//! 后续元素，于是时长变成一个看起来合理但错误的数。

/// 容器层能给出的信息
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContainerInfo {
    pub duration_secs: f64,
    pub width: u32,
    pub height: u32,
    /// CodecID，如 `V_VP8` / `V_MPEG4/ISO/AVC` / `A_OPUS`
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub frame_count: Option<u64>,
    pub track_count: u32,
    pub has_audio: bool,
}

// EBML 元素 ID（已含标记位）
const ID_SEGMENT: u32 = 0x1853_8067;
const ID_INFO: u32 = 0x1549_A966;
const ID_TIMECODE_SCALE: u32 = 0x002A_D7B1;
const ID_DURATION: u32 = 0x4489;
const ID_TRACKS: u32 = 0x1654_AE6B;
const ID_TRACK_ENTRY: u32 = 0xAE;
const ID_TRACK_TYPE: u32 = 0x83;
const ID_CODEC_ID: u32 = 0x86;
const ID_PIXEL_WIDTH: u32 = 0xB0;
const ID_PIXEL_HEIGHT: u32 = 0xBA;
const ID_DEFAULT_DURATION: u32 = 0x0023_E383;

/// 读一个 vint 的 ID（保留标记位）
fn read_id(d: &[u8], pos: &mut usize) -> Option<u32> {
    let first = *d.get(*pos)?;
    if first == 0 {
        return None; // 0 不是合法的 vint 首字节
    }
    let len = first.leading_zeros() as usize + 1;
    if *pos + len > d.len() {
        return None;
    }
    let mut v: u32 = first as u32;
    for b in &d[*pos + 1..*pos + len] {
        v = (v << 8) | *b as u32;
    }
    *pos += len;
    Some(v)
}

/// 读一个 vint 的长度（**去掉**标记位）
fn read_size(d: &[u8], pos: &mut usize) -> Option<u64> {
    let first = *d.get(*pos)?;
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    if *pos + len > d.len() {
        return None;
    }
    let mask = (1u32 << (8 - len)) - 1;
    let mut v = (first as u32) & mask;
    for b in &d[*pos + 1..*pos + len] {
        v = (v << 8) | *b as u32;
    }
    *pos += len;
    // 未知长度：整个字节都是 0xFF（如 0x01 FF FF FF…）表示「延伸到父元素末尾」，
    // 返回 None 让调用方按父元素边界处理。
    if d[*pos - len..*pos].iter().all(|&b| b == 0xFF) {
        return None;
    }
    Some(v as u64)
}

/// 在 `limit` 范围内迭代子元素
fn children(d: &[u8], limit: usize) -> Vec<(u32, &[u8])> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < limit {
        let before = pos;
        let id = match read_id(d, &mut pos) {
            Some(v) => v,
            None => break,
        };
        let size = match read_size(d, &mut pos) {
            Some(v) => v as usize,
            // 未知长度：该元素延伸到父元素末尾
            None => limit.saturating_sub(pos),
        };
        // **零长度元素是合法的**（EBML 的空元素），不能因此退出循环。
        // 判据必须是「这一轮有没有前进」，而不是「end 有没有超过 pos」——
        // 初版用后者，于是遇到任何零长度元素就 break，
        // 后面真正的 Segment 永远读不到，报「缺少 Segment」。
        if pos <= before {
            break;
        }
        let end = pos.saturating_add(size).min(limit);
        out.push((id, &d[pos..end]));
        if end <= pos {
            // 零长度：pos 已前进，继续下一个元素
            continue;
        }
        pos = end;
    }
    out
}

fn uint(d: &[u8]) -> u64 {
    let mut v = 0u64;
    for b in d.iter().take(8) {
        v = (v << 8) | *b as u64;
    }
    v
}

fn float(d: &[u8]) -> f64 {
    match d.len() {
        4 => f32::from_be_bytes([d[0], d[1], d[2], d[3]]) as f64,
        8 => f64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]),
        _ => 0.0,
    }
}

/// 解析 Matroska / WebM
pub fn probe(bytes: &[u8]) -> Result<ContainerInfo, String> {
    if bytes.len() < 4 {
        return Err(format!("EBML 头不完整：{} 字节", bytes.len()));
    }
    if bytes[0] != 0x1A {
        return Err(format!(
            "不是 EBML 容器：首字节应为 0x1A，实得 0x{:02X}",
            bytes[0]
        ));
    }
    let mut info = ContainerInfo::default();
    let mut saw_segment = false;
    let mut timecode_scale = 1_000_000f64; // 规范默认值 1ms
    let mut raw_duration = 0.0f64;
    let mut default_dur_ns = 0u64;

    // 顶层找 Segment
    let mut pos = 0usize;
    while pos < bytes.len() {
        let before = pos;
        let id = match read_id(bytes, &mut pos) {
            Some(v) => v,
            None => break,
        };
        let size = match read_size(bytes, &mut pos) {
            Some(v) => v as usize,
            None => bytes.len().saturating_sub(pos),
        };
        if pos <= before {
            break; // 没有前进，死循环保护
        }
        let end = pos.saturating_add(size).min(bytes.len());
        if end <= pos {
            continue; // 零长度元素，pos 已前进
        }
        if id == ID_SEGMENT {
            saw_segment = true;
            let seg = &bytes[pos..end];
            for (cid, cbody) in children(seg, seg.len()) {
                match cid {
                    ID_INFO => {
                        for (iid, ibody) in children(cbody, cbody.len()) {
                            match iid {
                                ID_TIMECODE_SCALE => {
                                    timecode_scale = uint(ibody).max(1) as f64;
                                }
                                ID_DURATION => raw_duration = float(ibody),
                                _ => {}
                            }
                        }
                    }
                    ID_TRACKS => {
                        let mut n = 0u32;
                        for (tid, tbody) in children(cbody, cbody.len()) {
                            if tid != ID_TRACK_ENTRY {
                                continue;
                            }
                            n += 1;
                            let (mut ttype, mut codec, mut w, mut h, mut dd) =
                                (0u64, None, 0u32, 0u32, 0u64);
                            for (fid, fbody) in children(tbody, tbody.len()) {
                                match fid {
                                    ID_TRACK_TYPE => ttype = uint(fbody),
                                    ID_CODEC_ID => {
                                        codec = Some(
                                            String::from_utf8_lossy(fbody)
                                                .trim_end_matches('\0')
                                                .to_string(),
                                        )
                                    }
                                    ID_PIXEL_WIDTH => w = uint(fbody) as u32,
                                    ID_PIXEL_HEIGHT => h = uint(fbody) as u32,
                                    ID_DEFAULT_DURATION => dd = uint(fbody),
                                    _ => {}
                                }
                            }
                            if dd > 0 {
                                default_dur_ns = dd;
                            }
                            match ttype {
                                1 => {
                                    info.width = w;
                                    info.height = h;
                                    info.video_codec = codec;
                                }
                                2 => {
                                    info.has_audio = true;
                                    info.audio_codec = codec;
                                }
                                _ => {}
                            }
                        }
                        info.track_count = n;
                    }
                    _ => {}
                }
            }
        }
        pos = end;
    }

    if !saw_segment {
        return Err("EBML 容器缺少 Segment 元素".to_string());
    }

    // Duration 以 TimecodeScale 为单位：秒 = Duration × Scale / 1e9
    if raw_duration > 0.0 {
        info.duration_secs = raw_duration * timecode_scale / 1_000_000_000.0;
    } else if default_dur_ns > 0 && info.frame_count.is_some() {
        // 没有 Duration 时退化：用 DefaultDuration × 帧数
        let frames = info.frame_count.unwrap_or(0);
        info.duration_secs = (default_dur_ns as f64) * frames as f64 / 1_000_000_000.0;
    }
    Ok(info)
}
