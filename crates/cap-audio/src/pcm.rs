//! PCM 数据与 WAV 编解码。
//!
//! 零外部依赖：RIFF/WAVE 容器与 PCM 样本换算是纯计算，本 crate 因此可以
//! 在没有音频库的环境里全量单测（见 `tests/behavior.rs`）。
//!
//! 样本一律以 `f32` 承载，取值区间约定为 `[-1.0, 1.0]`：
//!
//! - 8bit WAVE 是**无符号**的，解码时做 `(x - 128) / 128` 才落到同一条区间；
//! - 其余位深按各自满量程归一（16bit 除 2^15，24bit 除 2^23，32bit 除 2^31）。
//!
//! 这条归一必须双向一致，否则「解码→处理→重编码」往返一次就整体偏移。

/// 一段交错存放（interleaved）的 PCM 音频。
///
/// 交错是 WAVE 容器本来的布局：第 `i` 帧的各声道样本连着排
/// （`[L0, R0, L1, R1, ...]`），不另存声道平面 —— 保持与容器一致，
/// 免得每次读写都多一次转置，而转置正是这类代码最容易写反的地方。
#[derive(Debug, Clone, PartialEq)]
pub struct Pcm {
    /// 采样率（Hz）
    pub sample_rate: u32,
    /// 声道数，恒 `>= 1`
    pub channels: u16,
    /// 交错样本，取值 `[-1.0, 1.0]`
    pub samples: Vec<f32>,
}

impl Pcm {
    /// 新建一段 PCM。
    pub fn new(sample_rate: u32, channels: u16, samples: Vec<f32>) -> Result<Pcm, String> {
        if channels == 0 {
            return Err("声道数不能为 0".to_string());
        }
        if sample_rate == 0 {
            return Err("采样率不能为 0".to_string());
        }
        if !samples.len().is_multiple_of(channels as usize) {
            return Err(format!(
                "样本数 {} 不是声道数 {} 的整数倍，交错数据被截断了",
                samples.len(),
                channels
            ));
        }
        Ok(Pcm {
            sample_rate,
            channels,
            samples,
        })
    }

    /// 帧数（每声道一个采样点为一帧）
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels as usize
        }
    }

    /// 时长（秒）
    pub fn duration_secs(&self) -> f64 {
        if self.sample_rate == 0 {
            return 0.0;
        }
        self.frames() as f64 / self.sample_rate as f64
    }

    /// 空音频（时长 0）
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// 取某一帧的全部声道样本
    pub fn frame(&self, index: usize) -> &[f32] {
        let ch = self.channels as usize;
        let start = index * ch;
        &self.samples[start..(start + ch).min(self.samples.len())]
    }

    /// 校验不变式。`Pcm::new` 已保证过一次，但 `samples` 是 pub 字段，
    /// 外部可以直接改，改完不自检的话下游换算会静默错位。
    pub fn validate(&self) -> Result<(), String> {
        if self.channels == 0 {
            return Err("声道数为 0".to_string());
        }
        if self.sample_rate == 0 {
            return Err("采样率为 0".to_string());
        }
        if !self.samples.len().is_multiple_of(self.channels as usize) {
            return Err(format!(
                "样本数 {} 不是声道数 {} 的整数倍",
                self.samples.len(),
                self.channels
            ));
        }
        Ok(())
    }
}

/// 样本的存储格式。decode 方向由容器头决定，encode 方向由调用方指定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    /// 8bit 无符号整数（WAVE 里唯一的无符号 PCM）
    U8,
    /// 16bit 有符号整数
    I16,
    /// 24bit 有符号整数（小端 3 字节）
    I24,
    /// 32bit 有符号整数
    I32,
    /// 32bit IEEE 浮点
    F32,
}

impl SampleFormat {
    /// 每样本位数
    pub fn bits(self) -> u16 {
        match self {
            SampleFormat::U8 => 8,
            SampleFormat::I16 => 16,
            SampleFormat::I24 => 24,
            SampleFormat::I32 => 32,
            SampleFormat::F32 => 32,
        }
    }

    /// 每样本字节数
    pub fn bytes(self) -> usize {
        (self.bits() / 8) as usize
    }

    /// 是否浮点
    pub fn is_float(self) -> bool {
        matches!(self, SampleFormat::F32)
    }

    /// 由位深反查格式；整数位深取无符号的那个归一，
    /// 8bit WAVE 只能是 U8。
    pub fn from_bits(bits: u16) -> Option<SampleFormat> {
        match bits {
            8 => Some(SampleFormat::U8),
            16 => Some(SampleFormat::I16),
            24 => Some(SampleFormat::I24),
            32 => Some(SampleFormat::I32),
            _ => None,
        }
    }
}

/// WAVE 的 `fmt ` 块解析结果（只取本 crate 关心的字段）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaveFmt {
    /// 1 = PCM，3 = IEEE 浮点（EXTENSIBLE 已在解析时展开成真实标签）
    pub format_tag: u16,
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
}

/// WAVE 头探测结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavProbe {
    pub fmt: WaveFmt,
    /// `data` 块在文件中的起始偏移
    pub data_offset: u64,
    /// `data` 块声明的长度；`None` 表示块长不可信（0 或 0xFFFFFFFF 流式）
    pub data_bytes: Option<u64>,
}

impl WavProbe {
    /// 帧数。`data_bytes` 不可信时退化为「按块对齐向下取整」。
    pub fn frames(&self, file_len: u64) -> u64 {
        let frame_bytes = self.frame_bytes();
        if frame_bytes == 0 {
            return 0;
        }
        match self.data_bytes {
            Some(n) => n / frame_bytes as u64,
            None => file_len.saturating_sub(self.data_offset) / frame_bytes as u64,
        }
    }

    /// 容器声明的块对齐（未声明时按 声道数 × 每样本字节数 推）
    pub fn frame_bytes(&self) -> usize {
        self.fmt.channels as usize * (self.fmt.bits_per_sample / 8) as usize
    }
}

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// 解析 WAVE 头。
///
/// 逐块走而不是按固定偏移取：`fmt ` 并不保证在 `data` 之前，
/// 真实文件里 `LIST`/`JUNK`/`bext` 夹在中间很常见，按固定偏移读会取到垃圾。
/// 块长一律对剩余字节取小 —— 声明的长度可能是 0xFFFFFFFF（流式）或是垃圾值，
/// 信任它就会越界。
pub fn probe_wav(bytes: &[u8]) -> Result<WavProbe, String> {
    if bytes.len() < 12 {
        return Err(format!("WAV 头不完整：{} 字节，至少需要 12", bytes.len()));
    }
    if &bytes[0..4] != b"RIFF" {
        return Err("不是 RIFF 容器：缺少 \"RIFF\" 标识".to_string());
    }
    if &bytes[8..12] != b"WAVE" {
        return Err(format!(
            "RIFF 容器类型不是 WAVE：前 4 字节为 {:?}",
            String::from_utf8_lossy(&bytes[8..12])
        ));
    }

    let riff_size = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as u64;
    // RIFF 长度字段不含开头 8 字节；声明值可能大于实际文件长度（截断下载），
    // 取小的那个，否则会在残缺文件上越界。
    let container_end = std::cmp::min(bytes.len() as u64, riff_size.saturating_add(8)) as usize;
    let container_end = std::cmp::max(container_end, 12);

    let mut fmt: Option<WaveFmt> = None;
    // (数据体偏移, 实际可读长度, 块头声明的长度)
    let mut data: Option<(usize, usize, u32)> = None;
    let mut pos = 12usize;

    while pos + 8 <= container_end {
        let id = [bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]];
        let declared = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]);
        let body = pos + 8;
        let available = container_end - body;
        let len = std::cmp::min(declared as usize, available);

        match &id {
            b"fmt " => fmt = Some(parse_fmt(&bytes[body..(body + len).min(container_end)])?),
            // 多个 data 块时取第一个：WAV 的规范行为是忽略后续的
            b"data" if data.is_none() => data = Some((body, len, declared)),
            _ => {}
        }

        // 块数据按偶数字节对齐，奇数长度块后有 1 字节填充。
        // 漏掉这一步：奇数长度的块（如某些 LIST）会让后续块整体错位 1 字节。
        pos = body + len + (len & 1);
    }

    let fmt = fmt.ok_or_else(|| "WAV 缺少 fmt 块".to_string())?;
    validate_fmt(&fmt)?;

    let (data_offset, data_len, declared) = data.ok_or_else(|| "WAV 缺少 data 块".to_string())?;

    // 声明长度 0 或 0xFFFFFFFF 视为不可信（流式写法）
    let data_bytes = if declared == 0 || declared == 0xFFFF_FFFF {
        None
    } else {
        Some(std::cmp::min(declared as u64, data_len as u64))
    };

    Ok(WavProbe {
        fmt,
        data_offset: data_offset as u64,
        data_bytes,
    })
}

fn parse_fmt(d: &[u8]) -> Result<WaveFmt, String> {
    if d.len() < 16 {
        return Err(format!("fmt 块过短：{} 字节，至少需要 16", d.len()));
    }
    let mut tag = u16::from_le_bytes([d[0], d[1]]);
    let channels = u16::from_le_bytes([d[2], d[3]]);
    let sample_rate = u32::from_le_bytes([d[4], d[5], d[6], d[7]]);
    let block_align = u16::from_le_bytes([d[12], d[13]]);
    let bits_per_sample = u16::from_le_bytes([d[14], d[15]]);

    // WAVE_FORMAT_EXTENSIBLE 把真实标签藏在 SubFormat GUID 的头两个字节。
    // 布局：18=wValidBitsPerSample, 22=dwChannelMask, 26=SubFormat GUID(16)
    // ⇒ GUID 起始于 24，标签即 [24..26]，故至少需要 26 字节。
    if tag == WAVE_FORMAT_EXTENSIBLE {
        if d.len() < 26 {
            return Err(format!(
                "EXTENSIBLE fmt 块被截断：{} 字节，读不到 SubFormat GUID（至少 26）",
                d.len()
            ));
        }
        tag = u16::from_le_bytes([d[24], d[25]]);
    }

    // 块对齐与「声道数 × 每样本字节数」不一致时不能只信一个：
    // 信声明值会让帧错位，信推算值会让非标准文件读错。这里显式报错。
    let derived = channels as usize * (bits_per_sample / 8) as usize;
    if block_align != 0 && block_align as usize != derived {
        return Err(format!(
            "fmt 块块对齐自相矛盾：声明 nBlockAlign={}，但 声道{}×{}bit 推出 {}",
            block_align, channels, bits_per_sample, derived
        ));
    }

    Ok(WaveFmt {
        format_tag: tag,
        channels,
        sample_rate,
        bits_per_sample,
    })
}

fn validate_fmt(fmt: &WaveFmt) -> Result<(), String> {
    if fmt.channels == 0 {
        return Err("fmt 块声明声道数为 0".to_string());
    }
    if fmt.sample_rate == 0 {
        return Err("fmt 块声明采样率为 0".to_string());
    }
    match fmt.format_tag {
        WAVE_FORMAT_PCM => {
            if SampleFormat::from_bits(fmt.bits_per_sample).is_none() {
                return Err(format!(
                    "不支持的 PCM 位深：{}bit（支持 8/16/24/32）",
                    fmt.bits_per_sample
                ));
            }
        }
        WAVE_FORMAT_IEEE_FLOAT => {
            if !matches!(fmt.bits_per_sample, 32 | 64) {
                return Err(format!(
                    "不支持的浮点位深：{}bit（支持 32/64）",
                    fmt.bits_per_sample
                ));
            }
        }
        other => {
            return Err(format!(
                "不支持的 WAV 编码标签 0x{:04X}（仅支持 PCM=0x0001 与 IEEE 浮点=0x0003）",
                other
            ))
        }
    }
    Ok(())
}

/// 解码整段 WAV 为 PCM
pub fn decode_wav(bytes: &[u8]) -> Result<Pcm, String> {
    let probe = probe_wav(bytes)?;
    let start = probe.data_offset as usize;
    if start > bytes.len() {
        return Err(format!(
            "data 块偏移 {} 超出文件长度 {}",
            start,
            bytes.len()
        ));
    }
    let available = bytes.len() - start;
    let len = match probe.data_bytes {
        Some(n) => std::cmp::min(n as usize, available),
        None => available,
    };
    let samples = decode_samples(&bytes[start..(start + len)], &probe.fmt)?;
    let pcm = Pcm {
        sample_rate: probe.fmt.sample_rate,
        channels: probe.fmt.channels,
        samples,
    };
    pcm.validate()?;
    Ok(pcm)
}

/// 把 PCM 样本按容器声明的格式解码到 `[-1.0, 1.0]`
fn decode_samples(data: &[u8], fmt: &WaveFmt) -> Result<Vec<f32>, String> {
    let channels = fmt.channels as usize;
    let bps = fmt.bits_per_sample;
    let bytes_per_sample = (bps / 8) as usize;
    if bytes_per_sample == 0 {
        return Err(format!("位深 {} 折算出每样本 0 字节", bps));
    }
    let frame_bytes = channels * bytes_per_sample;
    // 尾部不足一整帧的残余直接丢掉：它是截断，不是数据
    let usable = data.len() - (data.len() % frame_bytes);
    let mut out = Vec::with_capacity(usable / bytes_per_sample);

    for chunk in data[..usable].chunks_exact(bytes_per_sample) {
        let v = if fmt.format_tag == WAVE_FORMAT_IEEE_FLOAT {
            match bps {
                32 => f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
                64 => {
                    let mut b = [0u8; 8];
                    b.copy_from_slice(chunk);
                    f64::from_le_bytes(b) as f32
                }
                _ => unreachable!("validate_fmt 已排除其它浮点位深"),
            }
        } else {
            match bps {
                8 => {
                    // WAVE 的 8bit PCM 是无符号的，中点在 128
                    (chunk[0] as f32 - 128.0) / 128.0
                }
                16 => i16::from_le_bytes([chunk[0], chunk[1]]) as f32 / 32768.0,
                24 => {
                    // 3 字节小端左移 8 位、再算术右移 8 位 = 24 位两补数的符号扩展。
                    //
                    // 这里**不能**只靠 from_le_bytes 补零：i32 的符号位在 bit31，
                    // 而 24 位的符号位在 bit23。补零后 0xC00000 会被读成 +12582912
                    // 而不是 -4194304 —— 数值对、时长对、声道对，声音却整个反相，
                    // 而且不抛错。写这条时我先按"补零即符号扩展"推理过一遍，是错的，
                    // 是上面那条 `二十四位负值…` 用例把它打红的。
                    let raw = (i32::from_le_bytes([chunk[0], chunk[1], chunk[2], 0]) << 8) >> 8;
                    raw as f32 / 8_388_608.0
                }
                32 => {
                    i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f32
                        / 2_147_483_648.0
                }
                _ => unreachable!("validate_fmt 已排除其它位深"),
            }
        };
        out.push(v);
    }
    Ok(out)
}

/// 编码为 WAV 字节流
pub fn encode_wav(pcm: &Pcm, format: SampleFormat) -> Result<Vec<u8>, String> {
    pcm.validate()?;
    if format.bytes() * pcm.channels as usize == 0 {
        return Err("每帧字节数为 0".to_string());
    }

    let channels = pcm.channels;
    let sample_rate = pcm.sample_rate;
    let bps = format.bits();
    let bytes_per_sample = format.bytes();
    let frame_bytes = bytes_per_sample * channels as usize;
    let data_len = pcm.samples.len() * bytes_per_sample;
    // RIFF 长度字段只统计 "WAVE" 及其后的块，且不计开头 8 字节
    let riff_size = 4 + (8 + 16) + (8 + data_len);

    let mut out = Vec::with_capacity(8 + riff_size);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(riff_size as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");

    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(
        &if format.is_float() {
            WAVE_FORMAT_IEEE_FLOAT
        } else {
            WAVE_FORMAT_PCM
        }
        .to_le_bytes(),
    );
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&((sample_rate as usize * frame_bytes) as u32).to_le_bytes());
    out.extend_from_slice(&(frame_bytes as u16).to_le_bytes());
    out.extend_from_slice(&bps.to_le_bytes());

    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    for &s in &pcm.samples {
        match format {
            SampleFormat::U8 => {
                // 8bit WAVE 是无符号中点表示：中点 128，半幅 128。
                // 用 +1.0 乘满量程会得到 256，超出 u8，故夹到 255 ——
                // 代价是 +1.0 存成 127/128（8bit 格式本身表达不了满幅正）。
                let q = (clamp(s) * 128.0 + 128.0).round() as i32;
                out.push(q.clamp(0, 255) as u8);
            }
            SampleFormat::I16 => {
                // 乘 2^15 而不是 32767，理由见函数末尾「刻度必须两侧一致」
                let v = (clamp(s) * 32768.0).round() as i64;
                let q = v.clamp(-32768, 32767) as i16;
                out.extend_from_slice(&q.to_le_bytes());
            }
            SampleFormat::I24 => {
                let v = (clamp(s) * 8_388_608.0).round() as i64;
                let b = v.clamp(-8_388_608, 8_388_607) as i32;
                out.extend_from_slice(&b.to_le_bytes()[..3]);
            }
            SampleFormat::I32 => {
                let v = (clamp(s) * 2_147_483_648.0).round() as i64;
                out.extend_from_slice(
                    &(v.clamp(i32::MIN as i64, i32::MAX as i64) as i32).to_le_bytes(),
                );
            }
            SampleFormat::F32 => out.extend_from_slice(&s.to_le_bytes()),
        }
    }

    Ok(out)
}

#[inline]
fn clamp(s: f32) -> f32 {
    if s.is_nan() {
        0.0
    } else {
        s.clamp(-1.0, 1.0)
    }
}

// ── 关于「解码/编码刻度必须两侧一致」 ────────────────────────────────
//
// 解码除 2^(bits-1)、编码也必须乘 2^(bits-1)，两端用同一个刻度。
// 初版编码写的是「乘 32767」（满量程正端），解码写的是「除 32768」，
// 单次往返的误差只有 1 LSB，看不出来；但值因此**不在编码网格上**：
//
//     decode(encode(-0.7)) = -22937/32768 = -0.69998169
//     再 encode：-0.69998169 × 32767 = -22936.3 → round → -22936
//     再 decode = -0.6999512        （又掉了 1 LSB）
//
// 每转一次码掉 1 LSB，方向恒定朝零 ⇒ **反复转码会让声音一路淡到静音**，
// 而每一步都"成功"、都返回 Ok、不抛错。链式操作
//（trim → normalize → fade 各写一次 WAV）正好是这个形状。
// 写测试时我先写的是"两次往返误差 < 1e-6"这种想当然的紧公差，
// 被这条性质打红后才发现根因在刻度不对称，不在公差。
// 修法：两侧统一用 2^(bits-1)，再把结果夹进该位深能表示的范围。
