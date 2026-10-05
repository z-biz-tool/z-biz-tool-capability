//! cap-audio — 音频处理原语。
//!
//! 矩阵规划 03 §7 原先只落了一个 4 行空壳占位，理由是「等 creator 侧音频能力
//! 集中后再定接口」。空壳的代价是：名字敞在 crates.io 上随时可能被别人占掉
//! （`PUBLISH.md` 自己写了「crates.io 一旦发布不可覆盖、不可删除」），
//! 而 matrix 里声明的「解码/重采样/混音/响度」四项能力一个都不存在。
//!
//! 本版本把接口定下来，遵循与 [`cap-img`] 一致的设计语言：
//! **路径进 → 路径出的纯函数，不持状态、不建资产库、不调模型**，
//! 出错一律返回 `Result<_, String>`，错误串里带上实际读到的值。
//!
//! ## 零外部依赖是刻意的
//!
//! 本 crate 不依赖任何音频库：RIFF/WAVE 容器解析与 PCM 样本换算是纯计算，
//! 换句话说**这个 crate 的每一条能力都能在单测里被验证**，不需要真实音频设备、
//! 不需要 ffmpeg、不需要网络。代价是不支持 MP3/AAC/FLAC 等压缩格式 ——
//! 那需要真正的解码器，不属于「原语」层。若要接压缩格式，由调用方先解码成 WAV。
//!
//! ## 层次
//!
//! - [`pcm`]：PCM 数据结构与 WAV 编解码（内核）
//! - [`dsp`]：重采样/增益/淡入淡出/裁剪/拼接/混音/倒放/声道拆分/响度
//! - 本模块：`AudioInfo` 探测 + 文件级一键操作
//!
//! 安全策略（路径黑名单等）属于调用方的 path_guard，不在本 crate 职责内。

pub mod dsp;
pub mod pcm;

use std::fs;
use std::io::Read;
use std::path::Path;

pub use dsp::{
    concat as concat_pcm, fade_in, fade_out, gain as apply_gain, is_silent, mix as mix_pcm,
    normalize_peak, peak, resample as resample_pcm, reverse as reverse_pcm, rms, rms_dbfs,
    split_channels, to_mono as to_mono_pcm, trim as trim_pcm,
};
pub use pcm::{decode_wav, encode_wav, probe_wav, Pcm, SampleFormat, WavProbe, WaveFmt};

/// 探测时读取的头部字节上限。WAV 的 `data` 块紧随 `fmt `，
/// 中间只可能夹 `LIST`/`JUNK`/`bext` 这类元数据块，1 MiB 足够覆盖。
/// 超过这个窗口还没见到 `data` 块就报错，而不是把整个大文件读进内存。
const HEADER_PROBE_LIMIT: u64 = 1024 * 1024;

/// 音频信息
#[derive(Debug, Clone, PartialEq)]
pub struct AudioInfo {
    /// 采样率（Hz）
    pub sample_rate: u32,
    /// 声道数
    pub channels: u16,
    /// 每样本位数
    pub bits_per_sample: u16,
    /// 样本是否为 IEEE 浮点
    pub is_float: bool,
    /// 时长（秒）
    pub duration_secs: f64,
    /// 帧数（每声道一个采样点为一帧）
    pub frames: u64,
    /// 文件字节数
    pub size: u64,
    /// 容器格式，如 `"wav"`
    pub format: String,
}

/// 获取音频信息（只读文件头，不解码样本）
pub fn info(path: impl AsRef<Path>) -> Result<AudioInfo, String> {
    let path = path.as_ref();
    let metadata = fs::metadata(path).map_err(|e| format!("读取元数据失败: {}", e))?;
    let size = metadata.len();

    let mut head = Vec::new();
    fs::File::open(path)
        .map_err(|e| format!("打开音频文件失败: {}", e))?
        .take(HEADER_PROBE_LIMIT)
        .read_to_end(&mut head)
        .map_err(|e| format!("读取音频头失败: {}", e))?;

    let probe = probe_wav(&head).map_err(|e| format!("解析 WAV 头失败: {}", e))?;
    let frames = probe.frames(size);

    Ok(AudioInfo {
        sample_rate: probe.fmt.sample_rate,
        channels: probe.fmt.channels,
        bits_per_sample: probe.fmt.bits_per_sample,
        is_float: probe.fmt.format_tag == 0x0003,
        duration_secs: if probe.fmt.sample_rate == 0 {
            0.0
        } else {
            frames as f64 / probe.fmt.sample_rate as f64
        },
        frames,
        size,
        format: detect_format(path),
    })
}

/// 按扩展名判断格式，返回小写名（与 cap-img 的 `detect_format` 同风格）
pub fn detect_format(path: impl AsRef<Path>) -> String {
    let ext = path
        .as_ref()
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "wav" | "wave" => "wav".to_string(),
        _ => ext,
    }
}

/// 按头部字节嗅探格式
pub fn sniff_format(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        return Some("wav");
    }
    // AIFF 是大端的 FORM，且 "AIFF"/"AIFC" 紧随其后
    if bytes.len() >= 12
        && &bytes[0..4] == b"FORM"
        && (&bytes[8..12] == b"AIFF" || &bytes[8..12] == b"AIFC")
    {
        return Some("aiff");
    }
    if bytes.len() >= 4 && &bytes[0..4] == b"OggS" {
        return Some("ogg");
    }
    if bytes.len() >= 4 && &bytes[0..4] == b"fLaC" {
        return Some("flac");
    }
    None
}

/// 读入整段音频并解码为 PCM
pub fn read_pcm(path: impl AsRef<Path>) -> Result<Pcm, String> {
    let bytes = fs::read(path.as_ref()).map_err(|e| format!("读取音频文件失败: {}", e))?;
    decode_wav(&bytes)
}

/// 写出 PCM
pub fn write_pcm(
    pcm: &Pcm,
    dest_path: impl AsRef<Path>,
    format: SampleFormat,
) -> Result<u64, String> {
    let bytes = encode_wav(pcm, format)?;
    if let Some(parent) = dest_path.as_ref().parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录失败 {}: {}", parent.display(), e))?;
        }
    }
    fs::write(dest_path, &bytes).map_err(|e| format!("写入失败: {}", e))?;
    Ok(bytes.len() as u64)
}

/// 一步到位：读入 → （可选）重采样 / 降单声道 → 写出。
/// 这是最常用的一条通路：转码 = 换采样率 + 换位深。
pub fn convert(
    src: impl AsRef<Path>,
    dest: impl AsRef<Path>,
    target_rate: Option<u32>,
    mono: bool,
    format: SampleFormat,
) -> Result<u64, String> {
    let pcm = read_pcm(src)?;
    let mut out = match target_rate {
        Some(r) if r != pcm.sample_rate => resample_pcm(&pcm, r)?,
        _ => pcm,
    };
    if mono && out.channels > 1 {
        out = to_mono_pcm(&out)?;
    }
    write_pcm(&out, dest, format)
}

/// 重采样并写出
pub fn resample(
    src: impl AsRef<Path>,
    dest: impl AsRef<Path>,
    new_rate: u32,
    format: SampleFormat,
) -> Result<u64, String> {
    let pcm = read_pcm(src)?;
    let out = resample_pcm(&pcm, new_rate)?;
    write_pcm(&out, dest, format)
}

/// 按秒裁剪并写出
pub fn trim(
    src: impl AsRef<Path>,
    dest: impl AsRef<Path>,
    start_secs: f64,
    end_secs: f64,
    format: SampleFormat,
) -> Result<u64, String> {
    let pcm = read_pcm(src)?;
    let out = trim_pcm(&pcm, start_secs, end_secs)?;
    write_pcm(&out, dest, format)
}

/// 调整线性增益并写出
pub fn apply_gain_file(
    src: impl AsRef<Path>,
    dest: impl AsRef<Path>,
    linear: f32,
    format: SampleFormat,
) -> Result<u64, String> {
    let pcm = read_pcm(src)?;
    let out = apply_gain(&pcm, linear);
    write_pcm(&out, dest, format)
}

/// 淡入淡出并写出
pub fn fade(
    src: impl AsRef<Path>,
    dest: impl AsRef<Path>,
    fade_in_secs: f32,
    fade_out_secs: f32,
    format: SampleFormat,
) -> Result<u64, String> {
    let pcm = read_pcm(src)?;
    let out = fade_out(&fade_in(&pcm, fade_in_secs), fade_out_secs);
    write_pcm(&out, dest, format)
}

/// 峰值归一化并写出
pub fn normalize(
    src: impl AsRef<Path>,
    dest: impl AsRef<Path>,
    target_peak: f32,
    format: SampleFormat,
) -> Result<u64, String> {
    let pcm = read_pcm(src)?;
    let out = normalize_peak(&pcm, target_peak)?;
    write_pcm(&out, dest, format)
}

/// 多声道降为单声道并写出
pub fn to_mono(
    src: impl AsRef<Path>,
    dest: impl AsRef<Path>,
    format: SampleFormat,
) -> Result<u64, String> {
    let pcm = read_pcm(src)?;
    let out = to_mono_pcm(&pcm)?;
    write_pcm(&out, dest, format)
}

/// 倒放并写出
pub fn reverse(
    src: impl AsRef<Path>,
    dest: impl AsRef<Path>,
    format: SampleFormat,
) -> Result<u64, String> {
    let pcm = read_pcm(src)?;
    let out = reverse_pcm(&pcm);
    write_pcm(&out, dest, format)
}

/// 多文件首尾相接成一段
pub fn concat(
    paths: &[impl AsRef<Path>],
    dest: impl AsRef<Path>,
    format: SampleFormat,
) -> Result<u64, String> {
    if paths.is_empty() {
        return Err("拼接失败：没有输入文件".to_string());
    }
    let mut parts = Vec::with_capacity(paths.len());
    for p in paths {
        parts.push(read_pcm(p)?);
    }
    let merged = concat_pcm(&parts)?;
    write_pcm(&merged, dest, format)
}

/// 两段音频叠加混音
pub fn mix(
    a: impl AsRef<Path>,
    b: impl AsRef<Path>,
    dest: impl AsRef<Path>,
    format: SampleFormat,
) -> Result<u64, String> {
    let pa = read_pcm(a)?;
    let pb = read_pcm(b)?;
    let out = mix_pcm(&pa, &pb)?;
    write_pcm(&out, dest, format)
}
