//! PCM 上的信号处理原语。
//!
//! 全部是纯计算：输入 `&Pcm`、输出新的 `Pcm`，不碰文件、不持状态，
//! 因此每条都能在单测里用「手算得出期望值」的短数组验证。
//!
//! **交错数据的陷阱**：本 crate 的样本是交错的，所以任何「按样本遍历」的写法
//! 在多声道下都会出错 —— 最典型的是 [`reverse`]：逐样本反转会让立体声的
//! 左右声道对调（听感上像是左右镜像）。所有需要逐帧操作的地方一律先取帧。

use crate::pcm::Pcm;

fn frame_count(pcm: &Pcm) -> usize {
    pcm.frames()
}

/// 线性重采样到目标采样率。
///
/// 用线性插值而不是丢点/插点：后者会在高频处产生混叠，听感是刺耳的哨音。
/// 长度按 `round(旧帧数 × 新/旧)` 算，时长因此保持不变（±半帧）。
pub fn resample(pcm: &Pcm, new_rate: u32) -> Result<Pcm, String> {
    pcm.validate()?;
    if new_rate == 0 {
        return Err("目标采样率不能为 0".to_string());
    }
    if new_rate == pcm.sample_rate {
        return Ok(pcm.clone());
    }
    let ch = pcm.channels as usize;
    let old_frames = frame_count(pcm);
    if old_frames == 0 {
        return Ok(Pcm {
            sample_rate: new_rate,
            channels: pcm.channels,
            samples: Vec::new(),
        });
    }

    let ratio = new_rate as f64 / pcm.sample_rate as f64;
    let new_frames = ((old_frames as f64) * ratio).round() as usize;
    let new_frames = new_frames.max(1);
    let mut samples = Vec::with_capacity(new_frames * ch);

    for i in 0..new_frames {
        // 源位置：i / ratio，并夹在最后一帧内 —— 否则 ratio > 1 时会越界
        let src = i as f64 / ratio;
        let i0 = (src.floor() as usize).min(old_frames - 1);
        let i1 = (i0 + 1).min(old_frames - 1);
        let t = (src - i0 as f64) as f32;
        let a = pcm.frame(i0);
        let b = pcm.frame(i1);
        for c in 0..ch {
            let av = *a.get(c).unwrap_or(&0.0);
            let bv = *b.get(c).unwrap_or(&0.0);
            samples.push(av + (bv - av) * t);
        }
    }

    Ok(Pcm {
        sample_rate: new_rate,
        channels: pcm.channels,
        samples,
    })
}

/// 线性增益（`1.0` = 不变，`0.5` = 减半，`2.0` = 翻倍）
pub fn gain(pcm: &Pcm, linear: f32) -> Pcm {
    Pcm {
        sample_rate: pcm.sample_rate,
        channels: pcm.channels,
        samples: pcm.samples.iter().map(|s| s * linear).collect(),
    }
}

/// 线性淡入（秒）。超过总时长时按总时长处理，不报错。
pub fn fade_in(pcm: &Pcm, secs: f32) -> Pcm {
    ramp(pcm, secs, true)
}

/// 线性淡出（秒）
pub fn fade_out(pcm: &Pcm, secs: f32) -> Pcm {
    ramp(pcm, secs, false)
}

fn ramp(pcm: &Pcm, secs: f32, up: bool) -> Pcm {
    let ch = pcm.channels as usize;
    let total = frame_count(pcm);
    // 斜坡长度以帧计，并夹进 [0, 总帧数]：fade(999s) 在 1s 音频上应等价于 fade(1s)
    let ramp_frames = if secs <= 0.0 || total == 0 {
        0
    } else {
        ((secs as f64) * pcm.sample_rate as f64).round() as usize
    };
    let ramp_frames = ramp_frames.min(total);

    let mut samples = pcm.samples.clone();
    for f in 0..ramp_frames {
        let k = (f as f32 + 1.0) / (ramp_frames as f32 + 1.0);
        let k = if up { k } else { 1.0 - k };
        for c in 0..ch {
            let idx = f * ch + c;
            if idx < samples.len() {
                samples[idx] *= k;
            }
        }
    }
    Pcm {
        sample_rate: pcm.sample_rate,
        channels: pcm.channels,
        samples,
    }
}

/// 按秒裁剪 `[start_secs, end_secs)`。越界端点夹到有效范围；
/// 起点不早于终点时返回空音频（不报错：调用方多半是在空文件上裁剪）。
pub fn trim(pcm: &Pcm, start_secs: f64, end_secs: f64) -> Result<Pcm, String> {
    pcm.validate()?;
    let total = frame_count(pcm);
    let start = (start_secs.max(0.0) * pcm.sample_rate as f64).round() as usize;
    let end = (end_secs.max(0.0) * pcm.sample_rate as f64).round() as usize;
    let start = start.min(total);
    let end = end.min(total);
    if end <= start {
        return Ok(Pcm {
            sample_rate: pcm.sample_rate,
            channels: pcm.channels,
            samples: Vec::new(),
        });
    }
    let ch = pcm.channels as usize;
    Ok(Pcm {
        sample_rate: pcm.sample_rate,
        channels: pcm.channels,
        samples: pcm.samples[start * ch..end * ch].to_vec(),
    })
}

/// 首尾相接。采样率或声道数不一致时**报错**而不是悄悄重采样 ——
/// 静默重采样会让调用方以为自己拼的是原素材。
pub fn concat(parts: &[Pcm]) -> Result<Pcm, String> {
    let mut out: Option<Pcm> = None;
    for p in parts {
        p.validate()?;
        match &out {
            None => out = Some(p.clone()),
            Some(acc) => {
                if acc.sample_rate != p.sample_rate {
                    return Err(format!(
                        "拼接失败：采样率不一致（{} vs {}）",
                        acc.sample_rate, p.sample_rate
                    ));
                }
                if acc.channels != p.channels {
                    return Err(format!(
                        "拼接失败：声道数不一致（{} vs {}）",
                        acc.channels, p.channels
                    ));
                }
            }
        }
    }
    let first = out.ok_or_else(|| "拼接失败：没有任何输入".to_string())?;
    let total: usize = parts.iter().map(|p| p.samples.len()).sum();
    let mut samples = Vec::with_capacity(total);
    for p in parts {
        samples.extend_from_slice(&p.samples);
    }
    Ok(Pcm {
        sample_rate: first.sample_rate,
        channels: first.channels,
        samples,
    })
}

/// 两段音频叠加（等权平均）。长度不等时按较长的一段补零。
pub fn mix(a: &Pcm, b: &Pcm) -> Result<Pcm, String> {
    a.validate()?;
    b.validate()?;
    if a.sample_rate != b.sample_rate {
        return Err(format!(
            "混音失败：采样率不一致（{} vs {}）",
            a.sample_rate, b.sample_rate
        ));
    }
    if a.channels != b.channels {
        return Err(format!(
            "混音失败：声道数不一致（{} vs {}）",
            a.channels, b.channels
        ));
    }
    let len = std::cmp::max(a.samples.len(), b.samples.len());
    let mut samples = Vec::with_capacity(len);
    for i in 0..len {
        let av = a.samples.get(i).copied().unwrap_or(0.0);
        let bv = b.samples.get(i).copied().unwrap_or(0.0);
        samples.push((av + bv) * 0.5);
    }
    Ok(Pcm {
        sample_rate: a.sample_rate,
        channels: a.channels,
        samples,
    })
}

/// 整体倒放。
///
/// **按帧反转，不按样本反转**：交错布局下逐样本反转会把立体声的左右声道对调
/// —— 数据量、时长、声道数全对，只有一耳朵听得出来，最难查。
pub fn reverse(pcm: &Pcm) -> Pcm {
    let ch = pcm.channels as usize;
    let total = frame_count(pcm);
    let mut samples = vec![0.0f32; total * ch];
    for f in 0..total {
        let src_frame = total - 1 - f;
        let (dst, src) = (f * ch, src_frame * ch);
        samples[dst..dst + ch].copy_from_slice(&pcm.samples[src..src + ch]);
    }
    Pcm {
        sample_rate: pcm.sample_rate,
        channels: pcm.channels,
        samples,
    }
}

/// 多声道降为单声道（各声道等权平均）
pub fn to_mono(pcm: &Pcm) -> Result<Pcm, String> {
    pcm.validate()?;
    if pcm.channels == 1 {
        return Ok(pcm.clone());
    }
    let ch = pcm.channels as usize;
    let frames = frame_count(pcm);
    let mut samples = Vec::with_capacity(frames);
    for f in 0..frames {
        let frame = pcm.frame(f);
        let sum: f32 = frame.iter().sum();
        samples.push(sum / ch as f32);
    }
    Ok(Pcm {
        sample_rate: pcm.sample_rate,
        channels: 1,
        samples,
    })
}

/// 拆成单声道列表（声道数 == 原声道数）
pub fn split_channels(pcm: &Pcm) -> Result<Vec<Pcm>, String> {
    pcm.validate()?;
    let ch = pcm.channels as usize;
    let frames = frame_count(pcm);
    let mut out = Vec::with_capacity(ch);
    for c in 0..ch {
        let mut samples = Vec::with_capacity(frames);
        for f in 0..frames {
            samples.push(pcm.samples[f * ch + c]);
        }
        out.push(Pcm {
            sample_rate: pcm.sample_rate,
            channels: 1,
            samples,
        });
    }
    Ok(out)
}

/// 由若干单声道流重新交织成多声道。
///
/// [`split_channels`] 的逆运算：传进来的每个 `Pcm` 必须是单声道、
/// 帧数一致，否则报错（不等长就没法确定交织后的帧数，猜不得）。
pub fn interleave(parts: &[Pcm]) -> Result<Pcm, String> {
    if parts.is_empty() {
        return Err("交织失败：没有任何输入".to_string());
    }
    let mut sample_rate = 0u32;
    let mut frames: Option<usize> = None;
    for (idx, p) in parts.iter().enumerate() {
        p.validate()?;
        if p.channels != 1 {
            return Err(format!(
                "交织失败：第 {} 路是 {} 声道，交织的输入必须是单声道",
                idx, p.channels
            ));
        }
        if sample_rate == 0 {
            sample_rate = p.sample_rate;
        } else if p.sample_rate != sample_rate {
            return Err(format!(
                "交织失败：采样率不一致（{} vs {}）",
                sample_rate, p.sample_rate
            ));
        }
        match frames {
            None => frames = Some(p.frames()),
            Some(n) if n == p.frames() => {}
            Some(n) => return Err(format!("交织失败：帧数不一致（{} vs {}）", n, p.frames())),
        }
    }
    let frames = frames.unwrap_or(0);
    let ch = parts.len();
    let mut samples = Vec::with_capacity(frames * ch);
    for f in 0..frames {
        for p in parts {
            samples.push(p.samples.get(f).copied().unwrap_or(0.0));
        }
    }
    Ok(Pcm {
        sample_rate,
        channels: ch as u16,
        samples,
    })
}

/// 峰值绝对值（无声时为 0.0）
pub fn peak(pcm: &Pcm) -> f32 {
    pcm.samples.iter().fold(0.0f32, |acc, &s| acc.max(s.abs()))
}

/// 均方根
pub fn rms(pcm: &Pcm) -> f32 {
    if pcm.samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = pcm.samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum / pcm.samples.len() as f64).sqrt() as f32
}

/// 均方根电平（dBFS）。全零或空音频返回 `-inf` 之外的 `-120.0` 下限，
/// 免得下游拿 NaN 去算增益。
pub fn rms_dbfs(pcm: &Pcm) -> f32 {
    let r = rms(pcm) as f64;
    if r <= 0.0 {
        -120.0
    } else {
        (20.0 * r.log10()).clamp(-120.0, 0.0) as f32
    }
}

/// 峰值归一化到 `target_peak`（须在 `(0, 1]`）。
/// 已是静音（峰值 0）时原样返回：除以 0 会得到 NaN 并污染整个输出。
pub fn normalize_peak(pcm: &Pcm, target_peak: f32) -> Result<Pcm, String> {
    pcm.validate()?;
    if !(target_peak > 0.0 && target_peak <= 1.0) {
        return Err(format!("目标峰值必须落在 (0, 1]，收到 {}", target_peak));
    }
    let p = peak(pcm);
    if p <= 0.0 {
        return Ok(pcm.clone());
    }
    Ok(gain(pcm, target_peak / p))
}

/// 静音检测：峰值是否低于阈值
pub fn is_silent(pcm: &Pcm, threshold: f32) -> bool {
    peak(pcm) < threshold
}
