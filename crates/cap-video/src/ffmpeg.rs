//! ffmpeg 适配层：可用性探测 + 抽帧。
//!
//! 容器元数据自己就能解析（[`crate::mp4`] / [`crate::matroska`]），
//! 但**解码必须借外部 ffmpeg**——纯 Rust 生态没有可用的全格式视频解码器，
//! 与其绑一个残缺的支持矩阵，不如把「找得到 ffmpeg 吗 / 它是哪一版 / 抽帧成功吗」
//! 这三件事做扎实，并把失败原因如实报出来。
//!
//! ## 三条不做的事（都是踩过才知道的）
//!
//! 1. **不 spawn 完就不回收。** 每个子进程都在本模块内被 wait 或 kill 掉，
//!    不留孤儿。ffmpeg 跑飞（挂死的网络输入、坏盘）时由超时兜底。
//! 2. **不用 `kill(-1, ...)`。** 负 PID 杀的是**调用方的整个进程组**——
//!    在 Tauri 应用里那一组里还有前端 webview 和用户的其它程序。
//!    本模块只 kill 自己 spawn 出来的那一个 child 句柄。
//! 3. **不给子进程无上限的输出。** ffmpeg 的 stderr 是逐帧刷的，
//!    一次坏文件的量级能到几十 MB。stdout/stderr 都在 [`OUTPUT_CAP`] 处截断。
//!
//! 这三条与 `z-biz-tool-worker` 仓 `agent_proxy.rs` 的教训同源，
//! 那边是「子进程要按进程组回收」，这边是「短命子进程也要有上限和回收」。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// 单条输出流的截断上限
const OUTPUT_CAP: usize = 64 * 1024;
/// 默认子进程超时
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// ffmpeg 探测结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfmpegInfo {
    pub path: PathBuf,
    /// `ffmpeg -version` 的首行，如 `ffmpeg version 7.1 Copyright (c) 2000-2024 the FFmpeg developers`
    pub version_line: String,
}

/// 在 `PATH` 里找**文件存在且有可执行位**的 ffmpeg。
///
/// ⚠️ 这**不等于**能用。文件存在 ≠ 跑得起来：本机就有一个 ffmpeg 文件齐全、
/// 可执行位也正常，但缺 `libx265.215.dylib`，一 spawn 就 dyld 报错。
/// 判断「能不能用」请用 [`available`]。
pub fn locate(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // 只认带可执行位的，普通同名文件不算
                if let Ok(meta) = std::fs::metadata(&candidate) {
                    if meta.permissions().mode() & 0o111 == 0 {
                        continue;
                    }
                }
            }
            return Some(candidate);
        }
    }
    None
}

/// ffmpeg 是否**真的跑得起来**。
///
/// 实现是「真跑一次 `ffmpeg -version`」，而不是「文件存在」。
/// 写第一版时我图省事用了 `locate().is_some()`，被本机那个
/// 「文件在但动态库缺失」的 ffmpeg 直接打脸：那台机器上 `available()` 报 true，
/// 随后的抽帧却必然失败，而调用方拿到的是一整屏 dyld 路径列表。
/// 「可用」这个词必须意味着**可执行**，否则它就是个误导性的乐观信号。
pub fn available() -> bool {
    probe().is_ok()
}

/// 探测 ffmpeg 路径与版本。
///
/// 区分两种失败，错误串必须能让调用方分辨该做什么：
/// - **没装** → 提示安装；
/// - **装了但跑不起来**（缺动态库 / 架构不匹配 / 权限） → 提示重装。
///   这两种混成一句话时，用户会去反复重装一个根本没装的程序。
pub fn probe() -> Result<FfmpegInfo, String> {
    let path =
        match locate("ffmpeg") {
            Some(p) => p,
            None => return Err(
                "未找到 ffmpeg：视频抽帧需要外部 ffmpeg 可执行文件，请先安装 ffmpeg 再使用本功能"
                    .to_string(),
            ),
        };
    match run(&path, &["-version"], DEFAULT_TIMEOUT) {
        Ok(out) => {
            let first = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if first.is_empty() {
                return Err(describe_probe_failure(
                    Some(&path),
                    "`ffmpeg -version` 无输出",
                ));
            }
            Ok(FfmpegInfo {
                path,
                version_line: first,
            })
        }
        Err(e) => Err(describe_probe_failure(Some(&path), &e)),
    }
}

/// 区分「没装」与「装了但跑不起来」，并给各自可执行的下一步。
///
/// 单独抽成函数是为了能直接测这两个分支：真机上有 ffmpeg 时 `None` 分支
/// 永远走不到，而在没装 ffmpeg 的机器上 `Some` 分支走不到。
/// 混成一句话的后果是用户去反复重装一个根本没装的程序。
fn describe_probe_failure(found: Option<&Path>, detail: &str) -> String {
    match found {
        None => "未找到 ffmpeg：视频抽帧需要外部 ffmpeg 可执行文件，请先安装 ffmpeg 再使用本功能".to_string(),
        Some(p) => format!(
            "找到 ffmpeg（{}）但无法执行：{}。这通常是安装损坏（缺动态库或架构不匹配），请重装 ffmpeg，而不是重新安装。",
            p.display(),
            clip(detail, 400)
        ),
    }
}

/// 把一段可能极长的报错（ffmpeg 的 dyld spew 能有一千多字符）压到可读长度
fn clip(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars).collect();
    format!("{}…（已截断）", head.trim_end())
}

/// 确保目标文件的父目录存在
fn ensure_parent(dest: &Path) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录失败 {}: {}", parent.display(), e))?;
        }
    }
    Ok(())
}

struct Output {
    stdout: Vec<u8>,
    /// 是否因超过上限而被截断（stderr 只在失败路径上当场用掉，不往回传）
    truncated: bool,
}

/// 起一个读线程把流读完并截断，返回接收端。
///
/// 抽成独立函数是因为 `ChildStdout` 与 `ChildStderr` 是两个不同的类型，
/// 没法塞进同一个数组——泛型参数在这里是唯一干净写法。
fn spawn_reader<R: Read + Send + 'static>(
    mut stream: R,
    which: &'static str,
) -> mpsc::Receiver<(&'static str, Vec<u8>, bool)> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut limited = (&mut stream).take((OUTPUT_CAP + 1) as u64);
        let _ = limited.read_to_end(&mut buf);
        let truncated = buf.len() > OUTPUT_CAP;
        if truncated {
            buf.truncate(OUTPUT_CAP);
            buf.extend_from_slice("\n...[输出已截断]".as_bytes());
        }
        let _ = tx.send((which, buf, truncated));
    });
    rx
}

/// 跑一个子进程并**保证回收**，带超时与输出上限。
///
/// 超时后只 `kill` 这一个 child（不是进程组），再 `wait` 收尸，
/// 因此不会留下孤儿进程，也不会误伤调用方的其它进程。
fn run(exe: &Path, args: &[&str], timeout: Duration) -> Result<Output, String> {
    let mut child = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动 {} 失败: {}", exe.display(), e))?;

    // 两条管道各起一个读线程并各自截断。不起线程的话，
    // 父进程 wait 期间管道写满会让子进程阻塞在 write 上——
    // 那就是「进程卡住但看起来还活着」，最难查的一种死锁。
    let mut receivers = Vec::new();
    if let Some(out) = child.stdout.take() {
        receivers.push(spawn_reader(out, "stdout"));
    }
    if let Some(err) = child.stderr.take() {
        receivers.push(spawn_reader(err, "stderr"));
    }

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    // kill 之后必须再 wait，否则子进程会变成僵尸
                    let _ = child.wait();
                    return Err(format!(
                        "{} 执行超过 {}s 已被强制终止",
                        exe.display(),
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("等待 {} 失败: {}", exe.display(), e)),
        }
    };

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut truncated = false;
    for rx in receivers {
        if let Ok((which, buf, was_truncated)) = rx.recv() {
            if which == "stdout" {
                stdout = buf;
            } else {
                stderr = buf;
            }
            truncated |= was_truncated;
        }
    }

    if !status.success() {
        let tail = String::from_utf8_lossy(&stderr);
        let tail: String = tail.lines().rev().take(3).collect::<Vec<_>>().join(" / ");
        return Err(format!(
            "{} 退出码 {:?}：{}",
            exe.display(),
            status.code(),
            if tail.is_empty() {
                "无 stderr".to_string()
            } else {
                tail
            }
        ));
    }

    Ok(Output { stdout, truncated })
}

/// 抽一帧存成图片，返回写入字节数。
///
/// `time_secs` 为负时取首帧。`-ss` 放在 `-i` **之前**做输入侧跳转：快且准确，
/// 放在之后是输出侧解码，得从 0 解到目标时间点，大文件上会慢一个数量级。
pub fn extract_frame(
    src: impl AsRef<Path>,
    time_secs: f64,
    dest: impl AsRef<Path>,
    timeout: Duration,
) -> Result<u64, String> {
    let src = src.as_ref();
    let dest = dest.as_ref();
    // 先查本地文件，再去探测 ffmpeg：源文件不存在是零成本的判断，
    // 不该为了报这个错先 spawn 一个子进程。
    if !src.is_file() {
        return Err(format!("源视频不存在: {}", src.display()));
    }
    let ff = probe()?;
    ensure_parent(dest)?;

    let ss = if time_secs.is_finite() && time_secs > 0.0 {
        format!("{:.3}", time_secs)
    } else {
        // 负数或 NaN：回到开头。用 0 而不是省略 -ss，
        // 省略时 ffmpeg 会等第一帧，而某些流的第一帧在数秒之后。
        "0".to_string()
    };

    let t = timeout.max(Duration::from_secs(5));
    let out = run(
        &ff.path,
        &[
            "-hide_banner",
            "-loglevel",
            "error",
            "-ss",
            &ss,
            "-i",
            &src.to_string_lossy(),
            "-frames:v",
            "1",
            "-q:v",
            "2",
            "-y",
            &dest.to_string_lossy(),
        ],
        t,
    )?;
    if out.truncated {
        // 不算失败，但值得知道：ffmpeg 报了大量告警
        eprintln!(
            "[cap-video] ffmpeg 输出超过 {}KB 已被截断",
            OUTPUT_CAP / 1024
        );
    }
    let size = std::fs::metadata(dest)
        .map_err(|e| format!("ffmpeg 退出成功但目标文件不存在 {}: {}", dest.display(), e))?
        .len();
    if size == 0 {
        return Err(format!(
            "抽帧产出 0 字节：时间点 {:.3}s 可能超出视频时长，或该流没有视频轨",
            time_secs
        ));
    }
    Ok(size)
}

/// 取缩略图（按最长边缩放到 `max_size`），返回 `(字节数, 宽, 高)`。
///
/// 签名与 cap-img 的 `thumbnail` 对齐，方便两个 crate 的调用方用同一套心智。
pub fn thumbnail(
    src: impl AsRef<Path>,
    dest: impl AsRef<Path>,
    max_size: u32,
) -> Result<(u64, u32, u32), String> {
    let src = src.as_ref();
    let dest = dest.as_ref();
    if max_size == 0 {
        return Err("缩略图尺寸必须大于 0".to_string());
    }
    if !src.is_file() {
        return Err(format!("源视频不存在: {}", src.display()));
    }
    let ff = probe()?;
    ensure_parent(dest)?;
    // scale='min(max,iw)':-2 —— 只缩不放，且高度取偶数（多数编码器要求）
    let vf = format!("scale='min({0},iw)':-2", max_size);
    run(
        &ff.path,
        &[
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            &src.to_string_lossy(),
            "-frames:v",
            "1",
            "-vf",
            &vf,
            "-y",
            &dest.to_string_lossy(),
        ],
        DEFAULT_TIMEOUT,
    )?;
    let meta = std::fs::metadata(dest)
        .map_err(|e| format!("ffmpeg 退出成功但目标文件不存在 {}: {}", dest.display(), e))?;
    if meta.len() == 0 {
        return Err("缩略图产出 0 字节".to_string());
    }
    // 尺寸由 ffmpeg 决定，容器层拿不到，这里从文件名不可靠 —— 交回调用方按需解析
    Ok((meta.len(), 0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两个分支的报错必须可区分：混成一句的话，用户会去反复重装一个根本没装的程序。
    ///
    /// 变异验证实测：把 `None` 分支的文案换成「找到了但坏了」，集成测试全绿 ——
    /// 因为真机上有 ffmpeg（虽然是坏的），`None` 分支根本走不到。
    /// 抽成纯函数后才测得到。
    #[test]
    fn 没装与装了但坏了_错误文案必须不同且各自可执行() {
        let missing = describe_probe_failure(None, "");
        assert!(missing.contains("安装"), "没装时应指向安装：{}", missing);
        assert!(
            !missing.contains("重装"),
            "没装时提「重装」会误导：{}",
            missing
        );

        let broken = describe_probe_failure(Some(Path::new("/usr/bin/ffmpeg")), "缺 libx265.dylib");
        assert!(
            broken.contains("重装"),
            "装了却坏了时应指向重装：{}",
            broken
        );
        assert!(
            broken.contains("/usr/bin/ffmpeg"),
            "应带上路径便于定位：{}",
            broken
        );
        assert!(
            broken.contains("缺 libx265.dylib"),
            "应带上具体原因：{}",
            broken
        );
    }

    #[test]
    fn 超长报错被压住_避免把整屏_dyld_spew_甩给调用方() {
        let long = "x".repeat(5000);
        let out = describe_probe_failure(Some(Path::new("/usr/bin/ffmpeg")), &long);
        assert!(
            out.chars().count() < 1000,
            "报错未截断，长度 {}",
            out.chars().count()
        );
        assert!(out.contains("已截断"), "应标明已截断");
    }

    #[test]
    fn 短报错原样保留不被截断() {
        let out = describe_probe_failure(Some(Path::new("/f")), "boom");
        assert!(out.contains("boom"));
        assert!(!out.contains("已截断"));
    }
}
