//! cap-audio 行为测试。
//!
//! **夹具一律手搓字节，不调用本 crate 的 `encode_wav`** —— 用被测的编码器去
//! 造被测解码器的输入，等于让编码器与解码器共享同一个 bug：一处写错，
//! 「编码→解码→比对」会自己跟自己对上，测试全绿而文件是坏的。
//! 所以下面所有 WAV 都是按 RIFF 规范手工拼出来的字节。

// 测试名是中文描述式的，术语里的 ASCII 部分（WAV / EXTENSIBLE / RIFF / DSP）天然
// 含大写，clippy 的 non_snake_case 会逐个报错。与其把术语降写换取"看起来合规"，
// 不如显式豁免并在此说明——测试名的可读性优先。
#![allow(non_snake_case)]

use cap_audio::dsp;
use cap_audio::pcm::{decode_wav, encode_wav, probe_wav, SampleFormat};
use cap_audio::{self as audio, Pcm};
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
            "cap-audio-{}-{}-{}-{}",
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

/// 手搓一个标准 PCM WAV：fmt 在前、data 在后。
fn build_wav(channels: u16, sample_rate: u32, bits: u16, data: &[u8]) -> Vec<u8> {
    build_wav_with_prelude(channels, sample_rate, bits, data, &[])
}

/// 在 fmt 之前插入额外块（用于验证"不能按固定偏移取 fmt"）。
fn build_wav_with_prelude(
    channels: u16,
    sample_rate: u32,
    bits: u16,
    data: &[u8],
    prelude: &[(u32, Vec<u8>)],
) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    for (id, payload) in prelude {
        body.extend_from_slice(&id.to_le_bytes());
        body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        body.extend_from_slice(payload);
        if payload.len() % 2 == 1 {
            body.push(0); // 奇数长度块后有 1 字节填充
        }
    }

    let block_align = channels as usize * (bits / 8) as usize;
    body.extend_from_slice(b"fmt ");
    body.extend_from_slice(&16u32.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes()); // PCM
    body.extend_from_slice(&channels.to_le_bytes());
    body.extend_from_slice(&sample_rate.to_le_bytes());
    body.extend_from_slice(&((sample_rate as usize * block_align) as u32).to_le_bytes());
    body.extend_from_slice(&(block_align as u16).to_le_bytes());
    body.extend_from_slice(&bits.to_le_bytes());

    body.extend_from_slice(b"data");
    body.extend_from_slice(&(data.len() as u32).to_le_bytes());
    body.extend_from_slice(data);

    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    // RIFF 长度字段 = 文件长度 - 8，**含 "WAVE" 这四字节**。
    // 初版这里写成 body.len()，漏了 4 字节，于是解析器（正确地）按声明长度
    // 把 data 块尾部截掉：8bit 用例直接报「缺少 data 块」，16bit 用例帧数少 1。
    // 解析器没做错，是夹具违反 RIFF 规范 —— 这也说明严格按声明长度截断是对的。
    out.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(&body);
    out
}

fn i16_bytes(vals: &[i16]) -> Vec<u8> {
    let mut v = Vec::new();
    for x in vals {
        v.extend_from_slice(&x.to_le_bytes());
    }
    v
}

fn f64_close(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() <= tol
}

// ───────────────────────── WAV 容器解析 ─────────────────────────

#[test]
fn wav_头能解析出声道率位深与帧数() {
    let data = i16_bytes(&[0, 1000, 0, -1000, 0, 2000]);
    let bytes = build_wav(2, 44100, 16, &data);
    let probe = probe_wav(&bytes).unwrap();
    assert_eq!(probe.fmt.channels, 2);
    assert_eq!(probe.fmt.sample_rate, 44100);
    assert_eq!(probe.fmt.bits_per_sample, 16);
    // 6 个样本 ÷ 2 声道 = 3 帧
    assert_eq!(probe.frames(bytes.len() as u64), 3);
}

#[test]
fn fmt_前面夹着别的块也照样解析得出() {
    // 按固定偏移取 fmt 的实现会在这里读到垃圾。
    // LIST 块刻意用奇数长度 3，逼出"块后 1 字节填充"这条规则。
    let prelude = vec![
        (0x4C49_5354u32, b"INFO".to_vec()), // "LIST"
        (0x4A554E4Bu32, vec![1, 2, 3]),     // "JUNK" 奇数长度
    ];
    let data = i16_bytes(&[0, 100, 200, 300]);
    let bytes = build_wav_with_prelude(1, 8000, 16, &data, &prelude);
    let probe = probe_wav(&bytes).unwrap();
    assert_eq!(probe.fmt.sample_rate, 8000, "fmt 前的块把解析带偏了");
    assert_eq!(probe.fmt.channels, 1);
    let pcm = decode_wav(&bytes).unwrap();
    assert_eq!(pcm.samples.len(), 4);
    assert!((pcm.samples[1] - 100.0 / 32768.0).abs() < 1e-6);
}

#[test]
fn extensible_标签能解析出真实编码() {
    // WAVE_FORMAT_EXTENSIBLE(0xFFFE)：真实标签藏在 SubFormat GUID 头两字节，
    // 偏移 24（不是 18，也不是 26 —— 那是 GUID 末尾）。
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&0xFFFEu16.to_le_bytes());
    fmt.extend_from_slice(&1u16.to_le_bytes()); // channels
    fmt.extend_from_slice(&48000u32.to_le_bytes());
    fmt.extend_from_slice(&96000u32.to_le_bytes()); // byte rate
    fmt.extend_from_slice(&2u16.to_le_bytes()); // block align
    fmt.extend_from_slice(&16u16.to_le_bytes()); // bits
    fmt.extend_from_slice(&22u16.to_le_bytes()); // cbSize
    fmt.extend_from_slice(&16u16.to_le_bytes()); // wValidBitsPerSample
    fmt.extend_from_slice(&4u32.to_le_bytes()); // dwChannelMask
    let mut guid = vec![0u8; 16];
    guid[0..2].copy_from_slice(&1u16.to_le_bytes()); // PCM
    fmt.extend_from_slice(&guid);
    assert_eq!(fmt.len(), 40, "EXTENSIBLE fmt 块应为 40 字节");

    let mut body = Vec::new();
    body.extend_from_slice(b"fmt ");
    body.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    body.extend_from_slice(&fmt);
    let data = i16_bytes(&[0, 5]);
    body.extend_from_slice(b"data");
    body.extend_from_slice(&(data.len() as u32).to_le_bytes());
    body.extend_from_slice(&data);
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(&body);

    let probe = probe_wav(&bytes).unwrap();
    assert_eq!(probe.fmt.format_tag, 1, "EXTENSIBLE 未展开成真实标签");
    let pcm = decode_wav(&bytes).unwrap();
    assert_eq!(pcm.samples.len(), 2);
}

/// 构造一个 EXTENSIBLE 容器，GUID 里写指定的真实编码标签。
/// 抽出来是因为下面两条用例要复用它，且都**必须**依赖「真的读了 GUID」。
fn build_extensible(inner_tag: u16, bits: u16, data: &[u8]) -> Vec<u8> {
    let channels = 1u16;
    let frame_bytes = channels as usize * (bits / 8) as usize;
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&0xFFFEu16.to_le_bytes());
    fmt.extend_from_slice(&channels.to_le_bytes());
    fmt.extend_from_slice(&48000u32.to_le_bytes());
    fmt.extend_from_slice(&((48000 * frame_bytes) as u32).to_le_bytes());
    fmt.extend_from_slice(&(frame_bytes as u16).to_le_bytes()); // nBlockAlign
    fmt.extend_from_slice(&bits.to_le_bytes()); // wBitsPerSample
    fmt.extend_from_slice(&22u16.to_le_bytes()); // cbSize
    fmt.extend_from_slice(&bits.to_le_bytes()); // wValidBitsPerSample
    fmt.extend_from_slice(&4u32.to_le_bytes()); // dwChannelMask
    let mut guid = vec![0u8; 16];
    guid[0..2].copy_from_slice(&inner_tag.to_le_bytes());
    fmt.extend_from_slice(&guid);
    assert_eq!(fmt.len(), 40, "EXTENSIBLE fmt 块应为 40 字节");

    let mut body = Vec::new();
    body.extend_from_slice(b"fmt ");
    body.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    body.extend_from_slice(&fmt);
    body.extend_from_slice(b"data");
    body.extend_from_slice(&(data.len() as u32).to_le_bytes());
    body.extend_from_slice(data);
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(&body);
    bytes
}

#[test]
fn extensible_里写着浮点就按浮点解() {
    // 这条与上面那条 GUID 写 1 的用例是**互补**的：GUID 写 1 时，
    // "读 GUID"和"硬编码 tag=1"结果一致，测试分辨不出来（变异验证实测该变异存活）。
    // 这里 GUID 写 3（IEEE 浮点），硬编码 1 会把 0.5 的位模式当 int32 读成 0.496…
    let data = 0.5f32.to_le_bytes().to_vec();
    let pcm = decode_wav(&build_extensible(3, 32, &data)).unwrap();
    assert!(
        (pcm.samples[0] - 0.5).abs() < 1e-6,
        "EXTENSIBLE 里的浮点标签没被读出来，得到 {}（说明退化成了按 int32 解）",
        pcm.samples[0]
    );
}

#[test]
fn extensible_包着不支持的编码时必须拒绝_而不是当成_pcm_解() {
    // tag 2 = MS ADPCM：没有 ADPCM 解码器。若不看 GUID 而默认按 PCM，
    // 就会把压缩字节流当采样点解出一段**响度正常、完全错误**的音频 ——
    // 这是"不抛错的静默错误"里最难发现的一种。
    let err = decode_wav(&build_extensible(2, 4, &[0x11, 0x22, 0x33, 0x44])).unwrap_err();
    assert!(
        err.contains("编码标签"),
        "应明确报不支持的编码标签，实际错误：{}",
        err
    );
}

// ───────────────────────── 样本换算 ─────────────────────────

#[test]
fn 八位_wave_是无符号的_静默点在中点_而不是零() {
    // 128 是 8bit WAVE 的静音值。若按有符号处理，128 会变成 +1.0 —— 一段
    // 本该安静的素材会变成满幅直流噪声，而且不抛错。
    let bytes = build_wav(1, 8000, 8, &[128, 0, 255]);
    let pcm = decode_wav(&bytes).unwrap();
    assert!(
        f64_close(pcm.samples[0] as f64, 0.0, 1e-6),
        "静音点 128 应解为 0.0"
    );
    assert!((pcm.samples[1] - (-1.0)).abs() < 1e-6, "0 应解为 -1.0");
    assert!(f64_close(pcm.samples[2] as f64, 127.0 / 128.0, 1e-6));
}

#[test]
fn 二十四位负值做符号扩展而不是当成正数() {
    // 0x800000 是 -8388608（满幅负）；不符号扩展会读成 +8388608 ⇒ 声音整体反相。
    let data = vec![0x00, 0x00, 0x80, 0x00, 0x00, 0x00];
    let bytes = build_wav(1, 8000, 24, &data);
    let pcm = decode_wav(&bytes).unwrap();
    assert!(
        (pcm.samples[0] - (-1.0)).abs() < 1e-6,
        "24bit 负值未符号扩展，得到 {}",
        pcm.samples[0]
    );
    assert!(pcm.samples[1].abs() < 1e-6);
}

#[test]
fn 浮点_wave_按_float_解而不是按_int32() {
    let data = [0.5f32.to_le_bytes(), (-0.25f32).to_le_bytes()].concat();
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&48000u32.to_le_bytes());
    fmt.extend_from_slice(&192000u32.to_le_bytes());
    fmt.extend_from_slice(&4u16.to_le_bytes());
    fmt.extend_from_slice(&32u16.to_le_bytes());
    let mut body = Vec::new();
    body.extend_from_slice(b"fmt ");
    body.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    body.extend_from_slice(&fmt);
    body.extend_from_slice(b"data");
    body.extend_from_slice(&(data.len() as u32).to_le_bytes());
    body.extend_from_slice(&data);
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(&body);

    let pcm = decode_wav(&bytes).unwrap();
    assert!((pcm.samples[0] - 0.5).abs() < 1e-6, "浮点样本被当整数解了");
    assert!((pcm.samples[1] + 0.25).abs() < 1e-6);
}

// ───────────────────────── 错误路径（必须是错误，不是 panic） ─────────────────────────

#[test]
fn 畸形输入只报错不崩溃() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("空文件", vec![]),
        ("只有 RIFF 四字节", b"RIFF".to_vec()),
        ("RIFF 但不是 WAVE", {
            let mut v = b"RIFF".to_vec();
            v.extend_from_slice(&4u32.to_le_bytes());
            v.extend_from_slice(b"AVI ");
            v
        }),
        ("截断的 fmt 块", {
            let mut v = b"RIFF".to_vec();
            v.extend_from_slice(&100u32.to_le_bytes());
            v.extend_from_slice(b"WAVE");
            v.extend_from_slice(b"fmt ");
            v.extend_from_slice(&16u32.to_le_bytes());
            v.extend_from_slice(&[1, 0]); // 只有 2 字节
            v
        }),
        ("声道数为 0", build_wav(0, 8000, 16, &[0, 0])),
        ("采样率为 0", {
            let data = i16_bytes(&[0, 0]);
            build_wav(1, 0, 16, &data)
        }),
        ("不支持的位深", {
            let data = vec![0u8; 12];
            build_wav(1, 8000, 12, &data)
        }),
    ];
    for (name, bytes) in cases {
        let r = std::panic::catch_unwind(|| decode_wav(&bytes));
        assert!(r.is_ok(), "{}：应当返回 Err 而不是 panic", name);
        assert!(r.unwrap().is_err(), "{}：应当被拒绝，实际解出了音频", name);
    }
}

#[test]
fn 块对齐与声道位深自相矛盾时报错() {
    // nBlockAlign 声明 4，但 1 声道 × 16bit 推出的是 2。信哪个都会错位，
    // 所以必须显式报错而不是猜。
    let data = i16_bytes(&[0, 1, 2, 3]);
    let mut body = Vec::new();
    body.extend_from_slice(b"fmt ");
    body.extend_from_slice(&16u32.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&8000u32.to_le_bytes());
    body.extend_from_slice(&32000u32.to_le_bytes());
    body.extend_from_slice(&4u16.to_le_bytes()); // ← 谎报块对齐
    body.extend_from_slice(&16u16.to_le_bytes());
    body.extend_from_slice(b"data");
    body.extend_from_slice(&(data.len() as u32).to_le_bytes());
    body.extend_from_slice(&data);
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(&body);

    let err = probe_wav(&bytes).unwrap_err();
    assert!(
        err.contains("块对齐"),
        "错误信息应点明块对齐矛盾，实际：{}",
        err
    );
}

// ───────────────────────── 往返 ─────────────────────────

#[test]
fn 五种位深都能原样往返() {
    // 16/24/32 位取 1LSB 量化误差为验收标准；8 位因容器本身是无符号中点表示，
    // 满量程 ±1 不可表达，单独按 1/128 验收并在断言里写明。
    let src = Pcm::new(
        44100,
        2,
        vec![0.0, 0.5, -0.5, 0.25, -0.25, 1.0, -1.0, 0.125],
    )
    .unwrap();
    for (fmt, tol) in [
        (SampleFormat::I16, 1.0 / 32767.0),
        (SampleFormat::I24, 1.0 / 8_388_607.0),
        (SampleFormat::I32, 1.0 / 2_147_483_647.0),
        (SampleFormat::F32, 1e-7),
        (SampleFormat::U8, 1.0 / 128.0),
    ] {
        let bytes = encode_wav(&src, fmt).unwrap();
        let back = decode_wav(&bytes).unwrap();
        assert_eq!(back.sample_rate, src.sample_rate, "{:?} 采样率丢了", fmt);
        assert_eq!(back.channels, src.channels, "{:?} 声道数丢了", fmt);
        assert_eq!(
            back.samples.len(),
            src.samples.len(),
            "{:?} 样本数变了",
            fmt
        );
        for (i, (a, b)) in src.samples.iter().zip(back.samples.iter()).enumerate() {
            assert!(
                (a - b).abs() <= tol,
                "{:?} 第 {} 个样本漂移：期望 {} 实得 {}",
                fmt,
                i,
                a,
                b
            );
        }
    }
}

#[test]
fn 反复转码不会累积偏移_更不会淡到静音() {
    // 这条测试是因为初版真出过 bug 才写的：解码除 2^15、编码乘 32767，
    // 两侧刻度不一致 ⇒ 值不在编码网格上 ⇒ 每转一次掉 1 LSB，方向恒定朝零。
    // 症状是"反复转码声音一路淡到静音"，每一步都返回 Ok、都不报错。
    //
    // 所以这里断言的不是"误差够小"，而是**逐位相等**：
    // 第一次往返把值落到了该位深的量化网格上，之后每一步都必须是恒等变换。
    let src = Pcm::new(
        8000,
        2,
        vec![0.3, -0.7, 0.0, 0.9, -0.123456, 0.55, 0.999, -0.999],
    )
    .unwrap();
    for fmt in [
        SampleFormat::I16,
        SampleFormat::I24,
        SampleFormat::I32,
        SampleFormat::F32,
    ] {
        let mut cur = src.clone();
        let mut first: Option<Vec<f32>> = None;
        for round in 1..=50 {
            cur = decode_wav(&encode_wav(&cur, fmt).unwrap()).unwrap();
            if round == 1 {
                first = Some(cur.samples.clone());
            } else {
                assert_eq!(
                    &cur.samples,
                    first.as_ref().unwrap(),
                    "{:?} 第 {} 次转码后样本变了：反复转码会累积偏移",
                    fmt,
                    round
                );
            }
        }
        // 峰值也不许一路衰减
        let first_peak = dsp::peak(&decode_wav(&encode_wav(&src, fmt).unwrap()).unwrap());
        assert!(
            (dsp::peak(&cur) - first_peak).abs() < 1e-6,
            "{:?} 转码 50 次后峰值从 {} 变成 {}",
            fmt,
            first_peak,
            dsp::peak(&cur)
        );
    }
}

#[test]
fn 超出满量程的输入被夹住而不是回绕() {
    // gain(10.0) 之后必然越界。若不夹，1.5 会绕成 -0.5 ⇒ 咔哒声。
    let loud = Pcm::new(8000, 1, vec![0.5, -0.5]).unwrap();
    let boosted = dsp::gain(&loud, 10.0);
    let bytes = encode_wav(&boosted, SampleFormat::I16).unwrap();
    let back = decode_wav(&bytes).unwrap();
    assert!((back.samples[0] - 1.0).abs() < 1e-4, "正峰应夹在 +1.0");
    assert!((back.samples[1] + 1.0).abs() < 1e-4, "负峰应夹在 -1.0");
}

// ───────────────────────── DSP ─────────────────────────

#[test]
fn 倒放按帧反转_立体声左右声道不串() {
    // 逐样本反转的实现：数据量、时长、声道数全对，只有声道配对错了。
    // 断言用「每个样本等于原样本同位置」—— 逐样本反转会立刻露馅。
    let stereo = Pcm::new(8000, 2, vec![0.1, -0.1, 0.2, -0.2, 0.3, -0.3]).unwrap();
    let rev = dsp::reverse(&stereo);
    assert_eq!(rev.samples, vec![0.3, -0.3, 0.2, -0.2, 0.1, -0.1]);
    // 显式再钉一遍声道配对：每帧左声道恒为正、右声道恒为其相反数
    for f in 0..rev.frames() {
        let fr = rev.frame(f);
        assert!(
            (fr[0] + fr[1]).abs() < 1e-7,
            "第 {} 帧左右声道被对调了：{:?}",
            f,
            fr
        );
    }
}

#[test]
fn 重采样保持时长() {
    let src = Pcm::new(
        8000,
        1,
        (0..8000).map(|i| (i as f32 / 8000.0) * 2.0 - 1.0).collect(),
    )
    .unwrap();
    for target in [4000u32, 11025, 16000, 44100, 48000] {
        let out = dsp::resample(&src, target).unwrap();
        assert!(
            f64_close(
                out.duration_secs(),
                src.duration_secs(),
                1.0 / target as f64
            ),
            "重采样到 {}Hz 后时长从 {}s 变成 {}s",
            target,
            src.duration_secs(),
            out.duration_secs()
        );
    }
}

#[test]
fn 重采样保留低频信号的形状() {
    // 0.1 秒的 100Hz 正弦：降采样后每周期仍应约 10 个点。
    let rate = 8000u32;
    let src = Pcm::new(
        rate,
        1,
        (0..800)
            .map(|i| (2.0 * std::f32::consts::PI * 100.0 * i as f32 / rate as f32).sin())
            .collect(),
    )
    .unwrap();
    let out = dsp::resample(&src, 4000).unwrap();
    assert_eq!(out.sample_rate, 4000);
    // 峰值应保留（线性插值不改变包络量级）
    assert!(
        (dsp::peak(&out) - dsp::peak(&src)).abs() < 0.02,
        "重采样后峰值从 {} 变成 {}，低频被削掉了",
        dsp::peak(&src),
        dsp::peak(&out)
    );
}

#[test]
fn 拼接与混音在参数不一致时报错而不是静默重采样() {
    let a = Pcm::new(8000, 1, vec![0.1, 0.2]).unwrap();
    let b = Pcm::new(44100, 1, vec![0.1, 0.2]).unwrap();
    let c = Pcm::new(8000, 2, vec![0.1, 0.2, 0.1, 0.2]).unwrap();
    assert!(
        dsp::concat(&[a.clone(), b.clone()]).is_err(),
        "采样率不同却拼成了"
    );
    assert!(
        dsp::concat(&[a.clone(), c.clone()]).is_err(),
        "声道数不同却拼成了"
    );
    assert!(dsp::mix(&a, &b).is_err(), "采样率不同却混成了");
    assert!(dsp::concat(&[]).is_err(), "空输入应报错");
}

#[test]
fn 拼接结果等于各段顺序相连() {
    let a = Pcm::new(8000, 2, vec![1.0, 2.0, 3.0, 4.0]).unwrap();
    let b = Pcm::new(8000, 2, vec![5.0, 6.0]).unwrap();
    let m = dsp::concat(&[a.clone(), b]).unwrap();
    assert_eq!(m.samples, vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    assert_eq!(m.channels, 2);
    assert_eq!(m.frames(), 3);
}

#[test]
fn 混音是等权平均且长度取较长者() {
    let a = Pcm::new(8000, 1, vec![1.0, 0.0, 0.0]).unwrap();
    let b = Pcm::new(8000, 1, vec![0.0, 1.0]).unwrap();
    let m = dsp::mix(&a, &b).unwrap();
    assert_eq!(m.samples, vec![0.5, 0.5, 0.0]);
}

#[test]
fn 拆声道与降单声道互为逆运算() {
    let stereo = Pcm::new(8000, 3, vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6]).unwrap();
    let parts = dsp::split_channels(&stereo).unwrap();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[1].samples, vec![0.2, 0.5]);
    for p in &parts {
        assert_eq!(p.channels, 1);
        assert_eq!(p.frames(), 2);
    }
    let recombined = dsp::interleave(&parts).unwrap();
    assert_eq!(
        recombined.samples, stereo.samples,
        "拆开再交织回去应完全一致（注意：直接 concat 各声道流**不是**逆运算，\
         那会把 [L,R] 变成 [L,L,R,R]）"
    );
    assert_eq!(recombined.channels, 3);

    // 交织的输入必须是单声道、帧数一致、采样率一致
    assert!(
        dsp::interleave(std::slice::from_ref(&stereo)).is_err(),
        "多声道输入被交织了"
    );
    let short = Pcm::new(8000, 1, vec![0.9]).unwrap();
    assert!(dsp::interleave(&[parts[0].clone(), short]).is_err());
    assert!(dsp::interleave(&[]).is_err());

    let mono = dsp::to_mono(&stereo).unwrap();
    assert_eq!(mono.channels, 1);
    assert!((mono.samples[0] - 0.2).abs() < 1e-6);
    assert!((mono.samples[1] - 0.5).abs() < 1e-6);
}

#[test]
fn 静音输入下所有响度函数都不产生_nan() {
    // 峰值归一化若直接除以 peak=0，会把 NaN 写进整段输出 ——
    // 而 NaN 过一遍 WAV 编码会变成 0，看起来"只是没声音"，不报错。
    let silence = Pcm::new(8000, 2, vec![0.0; 16]).unwrap();
    assert_eq!(dsp::peak(&silence), 0.0);
    assert_eq!(dsp::rms(&silence), 0.0);
    assert!(dsp::rms_dbfs(&silence).is_finite());
    assert!(dsp::is_silent(&silence, 0.001));

    let norm = dsp::normalize_peak(&silence, 0.9).unwrap();
    for s in &norm.samples {
        assert!(s.is_finite(), "归一化把 NaN/Inf 写进了输出");
    }
    let bytes = encode_wav(&norm, SampleFormat::I16).unwrap();
    let back = decode_wav(&bytes).unwrap();
    for s in &back.samples {
        assert!(s.is_finite());
    }
}

#[test]
fn 峰值归一化把峰值顶到目标值() {
    let quiet = Pcm::new(8000, 1, vec![0.1, -0.1, 0.05, -0.05]).unwrap();
    let n = dsp::normalize_peak(&quiet, 0.8).unwrap();
    assert!((dsp::peak(&n) - 0.8).abs() < 1e-6);
    // 形状不变，只放大
    assert!((n.samples[0] / n.samples[1] - -1.0).abs() < 1e-5);
    assert!(
        dsp::normalize_peak(&quiet, 0.0).is_err(),
        "目标峰值 0 应被拒"
    );
    assert!(
        dsp::normalize_peak(&quiet, 1.5).is_err(),
        "目标峰值 >1 应被拒"
    );
}

#[test]
fn 淡入淡出超过总时长时按总时长夹住() {
    let tone = Pcm::new(8000, 1, vec![1.0; 80]).unwrap(); // 10ms
    let f = dsp::fade_in(&tone, 999.0);
    assert_eq!(f.samples.len(), tone.samples.len());
    // 第一帧最轻、末帧最重
    assert!(f.samples[0] < f.samples[79]);
    assert!(f.samples[79] > 0.9);
    assert!(f.samples.iter().all(|s| (0.0..=1.0).contains(s)));
}

#[test]
fn 裁剪按秒取对应区间() {
    let pcm = Pcm::new(1000, 1, (0..10).map(|i| i as f32).collect()).unwrap();
    let t = dsp::trim(&pcm, 0.002, 0.005).unwrap();
    assert_eq!(t.samples, vec![2.0, 3.0, 4.0]);
    // 越界端点夹住而不是报错
    assert_eq!(dsp::trim(&pcm, -5.0, 99.0).unwrap().samples.len(), 10);
    // 起点不早于终点 ⇒ 空
    assert!(dsp::trim(&pcm, 0.005, 0.005).unwrap().is_empty());
}

#[test]
fn 构造时拒绝样本数不被声道整除的数据() {
    // 交错数据被截断时长度不再是声道数的整数倍；不自检的话后续按帧取值会越界。
    assert!(Pcm::new(8000, 2, vec![0.1, 0.2, 0.3]).is_err());
    assert!(Pcm::new(8000, 0, vec![]).is_err());
    assert!(Pcm::new(0, 1, vec![]).is_err());
    // 绕过构造函数直接改 pub 字段，validate 必须兜住
    let mut hacked = Pcm::new(8000, 2, vec![0.1, 0.2]).unwrap();
    hacked.samples.push(0.3);
    assert!(hacked.validate().is_err());
}

// ───────────────────────── 文件级通路 ─────────────────────────

#[test]
fn info_只读头就能拿到时长_不依赖整文件() {
    let dir = TempDir::new("info");
    let p = dir.join("a.wav");
    let data = i16_bytes(&vec![1234i16; 8000 * 2]); // 1 秒双声道
    fs::write(&p, build_wav(2, 8000, 16, &data)).unwrap();

    let i = audio::info(&p).unwrap();
    assert_eq!(i.sample_rate, 8000);
    assert_eq!(i.channels, 2);
    assert_eq!(i.bits_per_sample, 16);
    assert!(!i.is_float);
    assert_eq!(i.frames, 8000);
    assert!(f64_close(i.duration_secs, 1.0, 1e-9));
    assert_eq!(i.format, "wav");
    assert_eq!(i.size, fs::metadata(&p).unwrap().len());
}

#[test]
fn 端到端_转码_裁剪_拼接_混音_归一化都能产出真文件() {
    let dir = TempDir::new("e2e");
    let a = dir.join("a.wav");
    let b = dir.join("b.wav");

    // 1s 立体声 100Hz 正弦
    let rate = 8000u32;
    let tone: Vec<f32> = (0..rate)
        .map(|i| 0.5 * (2.0 * std::f32::consts::PI * 100.0 * i as f32 / rate as f32).sin())
        .collect();
    let stereo = Pcm::new(rate, 2, tone.iter().flat_map(|v| [*v, *v]).collect()).unwrap();
    audio::write_pcm(&stereo, &a, SampleFormat::I16).unwrap();
    audio::write_pcm(&stereo, &b, SampleFormat::I16).unwrap();

    // 转码：降采样 + 降单声道
    let c = dir.join("c.wav");
    audio::convert(&a, &c, Some(16000), true, SampleFormat::I16).unwrap();
    let ci = audio::info(&c).unwrap();
    assert_eq!(ci.sample_rate, 16000);
    assert_eq!(ci.channels, 1);
    assert!(f64_close(ci.duration_secs, 1.0, 1e-3));

    // 裁剪出中间 0.2s
    let t = dir.join("t.wav");
    audio::trim(&c, &t, 0.4, 0.6, SampleFormat::I16).unwrap();
    let ti = audio::info(&t).unwrap();
    assert!(
        (ti.duration_secs - 0.2).abs() < 0.005,
        "裁剪后时长 {}",
        ti.duration_secs
    );

    // 拼接两段 ⇒ 时长相加
    let cc = dir.join("cc.wav");
    audio::concat(&[&c, &c], &cc, SampleFormat::I16).unwrap();
    let cci = audio::info(&cc).unwrap();
    assert!(
        f64_close(cci.duration_secs, ci.duration_secs * 2.0, 0.005),
        "拼接后 {} 应约为 {}",
        cci.duration_secs,
        ci.duration_secs * 2.0
    );

    // 混音两段 ⇒ 不该是静音，且幅度不超过单边
    let mx = dir.join("mix.wav");
    audio::mix(&a, &a, &mx, SampleFormat::I16).unwrap();
    let mp = audio::read_pcm(&mx).unwrap();
    assert!(!dsp::is_silent(&mp, 0.01), "自混音后成了静音");

    // 归一化 ⇒ 峰值到 0.9
    let nz = dir.join("norm.wav");
    audio::normalize(&a, &nz, 0.9, SampleFormat::I16).unwrap();
    let np = audio::read_pcm(&nz).unwrap();
    assert!(
        (dsp::peak(&np) - 0.9).abs() < 0.01,
        "归一化后峰值 {}",
        dsp::peak(&np)
    );

    // 淡入淡出：首帧应接近 0
    let fd = dir.join("fade.wav");
    audio::fade(&a, &fd, 0.1, 0.1, SampleFormat::I16).unwrap();
    let fp = audio::read_pcm(&fd).unwrap();
    assert!(
        fp.samples[0].abs() < 0.02,
        "淡入首帧仍在满幅：{}",
        fp.samples[0]
    );

    // 倒放：两次倒放应回到原样
    let r1 = dir.join("r1.wav");
    let r2 = dir.join("r2.wav");
    audio::reverse(&a, &r1, SampleFormat::I16).unwrap();
    audio::reverse(&r1, &r2, SampleFormat::I16).unwrap();
    let back = audio::read_pcm(&r2).unwrap();
    for (i, (x, y)) in stereo.samples.iter().zip(back.samples.iter()).enumerate() {
        assert!(
            (x - y).abs() < 2e-4,
            "倒放两次第 {} 个样本没回来：{} vs {}",
            i,
            x,
            y
        );
    }
}

#[test]
fn 文件级操作对不存在的文件给出可读错误() {
    let dir = TempDir::new("missing");
    let r = audio::read_pcm(dir.join("nope.wav"));
    assert!(r.is_err());
    assert!(
        r.unwrap_err().contains("失败"),
        "错误串应说明是文件操作失败"
    );
}

#[test]
fn 嗅探格式按头部而非扩展名() {
    assert_eq!(
        audio::sniff_format(&build_wav(1, 8000, 16, &[0, 0])),
        Some("wav")
    );
    assert_eq!(audio::sniff_format(b"OggS................"), Some("ogg"));
    assert_eq!(audio::sniff_format(b"fLaC................"), Some("flac"));
    assert_eq!(audio::sniff_format(b"\x00\x00\x00\x00garbage!!"), None);
    assert_eq!(audio::sniff_format(b"RI"), None, "太短的输入不得越界");
    // 扩展名说 mp3 但内容是 wav：内容优先
    assert_eq!(audio::detect_format("/x/y.MP3"), "mp3");
}
