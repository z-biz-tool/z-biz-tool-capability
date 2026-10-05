//! cap-video — 视频处理原语。
//!
//! 矩阵规划 03 §7 原先只落了一个 4 行空壳，理由是「唯一的既有视频处理是 file 里
//! `Command::new` 调外部 ffmpeg 做缩略图，本机 ffmpeg 已知损坏，接口等真实需求来了再定」。
//! 那个理由成立的前提是「只会有缩略图一个需求」，而它同时留下了两个更实际的问题：
//!
//! 1. **「本机 ffmpeg 已知损坏」没有变成可见信号。** 调用方 `Command::new` 之后
//!    拿到的是一个非零退出码和一段 stderr，或者更糟——`Command::new` 本身失败
//!    （PATH 里没有）而调用方没检查。本 crate 把「找得到吗 / 哪一版 / 抽帧成不成功」
//!    做成三个有名字、错误串可读的函数。
//! 2. **容器元数据完全没人读。** 时长、宽高、编码、帧数就在 box 树 / EBML 树里，
//!    不需要解码就能取到——而「拖进来先显示时长和分辨率」是这类工具最常见的诉求。
//!
//! ## 能力边界（说清楚比含糊更有用）
//!
//! - **能自己做的**：容器元数据探测。MP4/MOV（ISO BMFF）与 Matroska/WebM（EBML）
//!   零依赖、零解码，字段全部取自容器头。
//! - **必须借外力的**：解码与抽帧。纯 Rust 没有可用的全格式视频解码器，
//!   与其绑一个残缺的支持矩阵，不如把 ffmpeg 的**可用性检测**与**失败原因**做扎实。
//!   没有 ffmpeg 时会得到一句可执行的报错，而不是一个空白的属性面板。
//!
//! 与 [`cap-img`] / [`cap-audio`] 一致的设计语言：**路径进 → 路径出的纯函数，
//! 不持状态、不建资产库、不调模型**，出错返回 `Result<_, String>`，
//! 错误串里带上实际读到的值。

pub mod ffmpeg;
pub mod matroska;
pub mod mp4;

use std::fs;
use std::path::Path;

pub use ffmpeg::{
    available as ffmpeg_available, extract_frame, probe as probe_ffmpeg, thumbnail, FfmpegInfo,
};
pub use matroska::ContainerInfo as MatroskaInfo;
pub use mp4::ContainerInfo as Mp4Info;

/// 视频信息
#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    /// 时长（秒）
    pub duration_secs: f64,
    /// 宽（像素）
    pub width: u32,
    /// 高（像素）
    pub height: u32,
    /// 平均码率（bps），由文件大小与时长推出
    pub bitrate: u64,
    /// 文件字节数
    pub size: u64,
    /// 容器类型：`"mp4"` / `"mov"` / `"webm"` / `"mkv"`
    pub container: String,
    /// 视频编码标识（MP4 是 fourcc，WebM 是 `V_*`）
    pub video_codec: Option<String>,
    /// 音频编码标识
    pub audio_codec: Option<String>,
    /// 视频轨帧数；容器未记录时为 `None`
    pub frame_count: Option<u64>,
    /// 轨道总数
    pub track_count: u32,
    /// 是否有音频轨
    pub has_audio: bool,
}

impl VideoInfo {
    /// 帧率估计（帧数 ÷ 时长）。缺帧数或时长为 0 时返回 `None`。
    pub fn fps(&self) -> Option<f64> {
        match (self.frame_count, self.duration_secs) {
            (Some(f), d) if f > 0 && d > 0.0 => Some(f as f64 / d),
            _ => None,
        }
    }
}

/// 按扩展名判断容器类型，返回小写名
pub fn detect_container(path: impl AsRef<Path>) -> String {
    let ext = path
        .as_ref()
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "mp4" | "m4v" => "mp4".to_string(),
        "mov" | "qt" => "mov".to_string(),
        "webm" => "webm".to_string(),
        "mkv" => "mkv".to_string(),
        _ => ext,
    }
}

/// 按头部字节嗅探容器类型
pub fn sniff_container(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && &bytes[4..8] == b"ftyp" {
        // 拿 major_brand 区分 mp4 / mov
        if bytes.len() >= 12 {
            let major = &bytes[8..12];
            if major == b"qt  " {
                return Some("mov");
            }
        }
        return Some("mp4");
    }
    // EBML 头魔数：0x1A 0x45 0xDF 0xA3
    // MKV 与 WebM 同为 EBML，靠 EBML 头里的 DocType 字符串区分；
    // 这里只按魔数认，容器类型留给扩展名区分，DocType 解析不在本 crate 范围。
    if bytes.len() >= 4 && bytes[0] == 0x1A && &bytes[1..4] == b"\x45\xDF\xA3" {
        return Some("webm");
    }
    // AVI 属于 RIFF 家族
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"AVI " {
        return Some("avi");
    }
    if bytes.len() >= 4 && &bytes[0..4] == b"FLV\x01" {
        return Some("flv");
    }
    None
}

/// 容器类型是否由本 crate 原生支持解析
pub fn is_native_container(container: &str) -> bool {
    matches!(container, "mp4" | "mov" | "webm" | "mkv")
}

/// 获取视频信息（只读容器头，不解码）
pub fn info(path: impl AsRef<Path>) -> Result<VideoInfo, String> {
    let path = path.as_ref();
    let metadata = fs::metadata(path).map_err(|e| format!("读取元数据失败: {}", e))?;
    let size = metadata.len();

    // 只读头：MP4 的元数据在 moov，可能在文件末尾，所以窗口要够大。
    // 1 MiB 覆盖绝大多数真实文件；moov 更大的会明确报错而不是读出半截。
    const HEAD_LIMIT: u64 = 1024 * 1024;
    let mut head = Vec::new();
    let want = std::cmp::min(size, HEAD_LIMIT);
    use std::io::Read;
    fs::File::open(path)
        .map_err(|e| format!("打开视频文件失败: {}", e))?
        .take(want)
        .read_to_end(&mut head)
        .map_err(|e| format!("读取视频头失败: {}", e))?;

    let sniffed = sniff_container(&head);
    let ext_container = detect_container(path);

    // 先按头部判断（头部比扩展名可信），认不出来再退回扩展名
    let mut kind = sniffed.map(|s| s.to_string());
    if kind.as_deref().map(|k| !is_native_container(k)) == Some(true) {
        // 头部认出来了但不是原生支持的容器，报清楚
        return Err(format!(
            "不支持的容器：{}（本 crate 原生支持 MP4/MOV/WebM/MKV；\
             其余格式请用 ffmpeg 侧能力）",
            kind.unwrap()
        ));
    }
    if kind.is_none() {
        kind = Some(ext_container.clone());
    }
    let kind = kind.unwrap_or_default();

    let parsed = match kind.as_str() {
        "mp4" | "mov" => {
            let p = mp4::probe(&head)?;
            (
                p.duration_secs,
                p.width,
                p.height,
                p.video_codec,
                p.audio_codec,
                p.frame_count,
                p.track_count,
                p.has_audio,
            )
        }
        "webm" | "mkv" => {
            let p = matroska::probe(&head)?;
            (
                p.duration_secs,
                p.width,
                p.height,
                p.video_codec,
                p.audio_codec,
                p.frame_count,
                p.track_count,
                p.has_audio,
            )
        }
        other => {
            return Err(format!(
                "不支持的容器：{}（本 crate 原生支持 MP4/MOV/WebM/MKV）",
                if other.is_empty() {
                    "无法识别"
                } else {
                    other
                }
            ))
        }
    };

    let (
        duration_secs,
        width,
        height,
        video_codec,
        audio_codec,
        frame_count,
        track_count,
        has_audio,
    ) = parsed;
    let bitrate = if duration_secs > 0.0 {
        (size as f64 / duration_secs * 8.0) as u64
    } else {
        0
    };

    Ok(VideoInfo {
        duration_secs,
        width,
        height,
        bitrate,
        size,
        container: kind,
        video_codec,
        audio_codec,
        frame_count,
        track_count,
        has_audio,
    })
}
