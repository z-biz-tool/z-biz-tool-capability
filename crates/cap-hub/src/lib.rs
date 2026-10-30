//! zbiz:// 家族握手协议（构造 / 解析 / 待处理队列 / 产品清单）
//!
//! 语法照 `z-biz-tool-lead/doc/矩阵规划/04_连接度：家族面板与统一协议.md`：
//!
//! ```text
//! zbiz://<product>/<action>?<key>=<value>&...
//! ```
//!
//! 例：`zbiz://note/open?file=/tmp/a.md&from=box`
//!
//! # 这是本 crate 的来历
//!
//! 代码**原样来自** `z-biz-tool-box/src-tauri/src/handoff.rs`（390 行，12 条单测），
//! 那是 box 仓里已落地、已实测的实现。
//!
//! **提升为共享 crate 的原因**：13 个仓的 `single-instance` 回调目前全是
//! `_argv`（丢弃）—— box 发过去的深链，目标 app 收到了但没处理。
//! 若每个仓各写一遍接收逻辑，会得到 13 份**迟早会不一致**的代码，
//! 那正是家族要消除的东西。
//!
//! box 侧原文件里那句注释「等第二个应用也要收深链时，这个文件能整体上提成
//! 共享 crate，不用边搬边改」—— 就是这一步。
//!
//! # 为什么是 URL 而不是一堆命令行 flag
//!
//! 一个 token 就能带齐「去哪个应用、做什么、带什么上下文」。它在三种传输里是同一串字节：
//! argv（本文件的落地方式）、OS 的 scheme 唤起（阶段 3 后续）、以及日志。
//! 换成 `--product note --file /x` 这种多参数形态，每个接收方都要自己解析 argv，
//! 而 argv 里还混着 macOS 的 `-psn_0_12345` 和 Windows 的 exe 路径。
//!
//! # 模块划分
//!
//! - `lib.rs`（本文件）—— **纯函数**：语法编解码、待处理队列。**刻意不 import tauri**，
//!   这样它能被单测直接覆盖，也能被任何形态的宿主复用。
//! - `manifest.rs` —— 产品清单与校验。
//! - `receiver.rs` —— **Tauri 胶水**：把 `AppHandle` 递进来，把队列接上事件通道。
//!   这一层依赖 tauri，所以单独成模块，让核心保持零依赖。
//!
//! # 与 TypeScript 侧的对应
//!
//! `z-biz-tool-shared/src/protocol/deeplink.ts` 是本 crate 的 TS 等价物。
//! 两边的字节必须完全一致 —— 由 `z-biz-tool-shared/tests/fixtures/deeplink-golden.json`
//! 里的 golden 样本逐字节比对（样本由本 crate 的 `build()` 实际产出）。
//! **改一边必须改另一边，并重跑两边的单测。**

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Mutex;

pub mod manifest;
#[cfg(feature = "tauri")]
pub mod receiver;

/// 前端监听的事件名。payload 无意义（`()`），只当「队列里有新深链了」的信号。
///
/// 和 mind 的 `open-path` 是同一套约定：信号不带数据，前端收到后用命令把队列取空。
/// 冷启动时前端还没挂监听，那次信号会丢——所以前端 mount 后必须再主动 take 一次。
pub const EVENT: &str = "zbiz-open";

const SCHEME: &str = "zbiz://";

/// 一条握手请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    pub product: String,
    pub action: String,
    pub params: BTreeMap<String, String>,
}

impl Handoff {
    /// 当前 box 只按 `params.key` 在前端路由，没有走这两个访问器；
    /// 但它们是接收方（下一个接深链的应用）唯一需要的入口，删了对方还得再写一遍。
    #[allow(dead_code)]
    pub fn param(&self, key: &str) -> Option<&str> {
        self.params.get(key).map(String::as_str)
    }

    /// 约定的交接主体：一个本地路径。
    #[allow(dead_code)]
    pub fn file(&self) -> Option<&str> {
        self.param("file")
    }
}

// ── 编解码 ────────────────────────────────────────────────────────────────
//
// 只保留 RFC3986 的 unreserved 再额外放行 `/` 与 `:`：
// 前者让路径在日志里仍然可读（`?file=/tmp/a.md` 而不是 `%2Ftmp%2Fa.md`），
// 后者留给 Windows 盘符（`C:\x` → `C:%5Cx`，冒号不被转义才认得出是盘符）。
// 其余字节一律转义，尤其是 `&` `=` `#` `+` 空格和所有非 ASCII。

fn is_literal(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/' | b':')
}

fn hex_digit(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ => (b'A' + (n - 10)) as char,
    }
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 按字节百分号转义。非 ASCII 按 UTF-8 逐字节转义（中文路径不会被打断）。
pub fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 8);
    for &b in value.as_bytes() {
        if is_literal(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0x0F));
        }
    }
    out
}

/// 解百分号转义。非法转义或非 UTF-8 如实报错——不静默替换成替身字符，
/// 那样只会把「文件名莫名其妙变了」的问题推迟到用户发现文件打不开时。
pub fn decode(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(format!("百分号转义不完整: {value}"));
            }
            let hi = hex_value(bytes[i + 1]).ok_or_else(|| {
                format!("非法十六进制: %{}", &value[i + 1..(i + 3).min(value.len())])
            })?;
            let lo = hex_value(bytes[i + 2]).ok_or_else(|| {
                format!("非法十六进制: %{}", &value[i + 1..(i + 3).min(value.len())])
            })?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            // 注意：`+` 按 RFC3986 就是字面加号，不当空格处理。
            // 我们把真加号编成 %2B，所以这里没有歧义。
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|e| format!("转义后不是合法 UTF-8: {e}"))
}

// ── 校验 ──────────────────────────────────────────────────────────────────
//
// 这层校验不是洁癖：product 会拿去和 manifest 的 key 比对、也进日志；
// 允许它带 `/` `.` 就等于允许深链伪造出 `z-biz-tool-../x` 这种让人误读的东西。

fn check_token(what: &str, v: &str) -> Result<(), String> {
    if v.is_empty() {
        return Err(format!("{what} 不能为空"));
    }
    if v.len() > 32 {
        return Err(format!("{what} 过长（{} 字节）: {v}", v.len()));
    }
    if !v
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(format!("{what} 只允许字母/数字/-/_，实际: {v}"));
    }
    Ok(())
}

fn check_value(v: &str) -> Result<(), String> {
    // 控制字符（含 \0 \n \r）一律拒绝：它们会被原样带进 argv 和日志，
    // 日志里多一行伪造记录、或让某些调用方截断参数。
    if let Some(c) = v.chars().find(|c| c.is_control()) {
        return Err(format!("参数值含控制字符 {c:?}"));
    }
    Ok(())
}

/// 构造深链。params 按 BTreeMap 顺序输出，所以同样的输入永远得到同一串字节——
/// 单测可以拿整串做金标，而不必逐字段比。
pub fn build(
    product: &str,
    action: &str,
    params: &BTreeMap<String, String>,
) -> Result<String, String> {
    check_token("product", product)?;
    check_token("action", action)?;
    let mut out = format!("{SCHEME}{product}/{action}");
    let mut sep = '?';
    for (k, v) in params {
        check_token("参数名", k)?;
        check_value(v)?;
        out.push(sep);
        sep = '&';
        out.push_str(k);
        out.push('=');
        out.push_str(&encode(v));
    }
    Ok(out)
}

/// 解析深链。action 缺省为 `open`（`zbiz://note?file=x` 合法）。
pub fn parse(uri: &str) -> Result<Handoff, String> {
    let rest = uri
        .strip_prefix(SCHEME)
        .ok_or_else(|| format!("不是 zbiz:// 深链: {uri}"))?;

    // product 的边界是 `/` **或** `?`。
    // 只找 `/` 会把 `zbiz://note?file=/tmp/a.md` 那个路径里的斜杠当成 action 分隔符，
    // 于是 product 被读成 "note?file=" —— 这个坑是 action_defaults_to_open 这条测出来的。
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (product, rest) = (&rest[..end], &rest[end..]);
    check_token("product", product)?;
    let rest = rest.strip_prefix('/').unwrap_or(rest);

    let (action, query) = match rest.find('?') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, ""),
    };
    let action = if action.is_empty() { "open" } else { action };
    check_token("action", action)?;

    let mut params = BTreeMap::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, raw) = pair
            .split_once('=')
            .ok_or_else(|| format!("参数缺少 '=': {pair}"))?;
        check_token("参数名", k)?;
        if params.contains_key(k) {
            return Err(format!("参数重复: {k}"));
        }
        params.insert(k.to_string(), decode(raw)?);
    }

    Ok(Handoff {
        product: product.to_string(),
        action: action.to_string(),
        params,
    })
}

/// 从 argv 里挑出深链。混在里面的 exe 路径、`-psn_0_1` 之类直接忽略。
pub fn from_args<I: IntoIterator<Item = String>>(args: I) -> Vec<Handoff> {
    args.into_iter()
        .filter(|a| a.starts_with(SCHEME))
        .filter_map(|a| match parse(&a) {
            Ok(h) => Some(h),
            Err(e) => {
                // 用户手上没有日志可看，这句话是唯一的排查线索
                eprintln!("[box] 忽略无法解析的深链 {a}: {e}");
                None
            }
        })
        .collect()
}

// ── 待处理队列 ────────────────────────────────────────────────────────────

/// 取走即清空的深链队列。
///
/// 两条触发（冷启动 seed、已运行实例的 second-instance 回调）共用这一个队列，
/// 所以同一串深链不会被执行两遍。
#[derive(Default)]
pub struct Pending(Mutex<Vec<Handoff>>);

impl Pending {
    /// 入队，返回真正被收下的条数（重复的被丢掉）。
    pub fn push(&self, items: Vec<Handoff>) -> usize {
        if items.is_empty() {
            return 0;
        }
        let mut q = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut added = 0;
        for it in items {
            if !q.contains(&it) {
                q.push(it);
                added += 1;
            }
        }
        added
    }

    pub fn take(&self) -> Vec<Handoff> {
        let mut q = self.0.lock().unwrap_or_else(|e| e.into_inner());
        q.drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn round_trip_plain_path() {
        let p = params(&[("file", "/tmp/a.md"), ("from", "box")]);
        let uri = build("note", "open", &p).unwrap();
        assert_eq!(uri, "zbiz://note/open?file=/tmp/a.md&from=box");
        assert_eq!(
            parse(&uri).unwrap(),
            Handoff {
                product: "note".into(),
                action: "open".into(),
                params: p,
            }
        );
    }

    /// 中文与空格是最容易踩的两个：前者按字节转义不能截断多字节字符，
    /// 后者不转义会让 argv 被拆成两个参数。
    #[test]
    fn round_trip_cjk_and_space() {
        let p = params(&[("file", "/Users/zifang/文稿/季度 总结.md")]);
        let uri = build("note", "open", &p).unwrap();
        assert!(uri.contains("%E6%96%87"), "中文应逐字节转义: {uri}");
        assert!(uri.contains("%20"), "空格应转义: {uri}");
        assert!(!uri.contains(' '), "深链里不能留裸空格: {uri}");
        assert_eq!(
            parse(&uri).unwrap().file(),
            Some("/Users/zifang/文稿/季度 总结.md")
        );
    }

    /// 分隔符本身出现在值里时不能把协议拆散。`&` `=` `#` `+` 各来一个。
    #[test]
    fn round_trip_values_with_separators() {
        let nasty = "a&b=c#d+e%f";
        let p = params(&[("q", nasty)]);
        let uri = build("box", "tool", &p).unwrap();
        assert_eq!(uri.matches('&').count(), 0);
        assert_eq!(uri.matches('=').count(), 1, "值里的 = 必须被转义: {uri}");
        assert_eq!(parse(&uri).unwrap().param("q"), Some(nasty));
    }

    #[test]
    fn action_defaults_to_open() {
        let h = parse("zbiz://note?file=/tmp/a.md").unwrap();
        assert_eq!(h.action, "open");
        assert_eq!(h.product, "note");
    }

    #[test]
    fn rejects_foreign_scheme() {
        assert!(parse("https://example.com/open?file=/x").is_err());
        assert!(parse("/tmp/a.md").is_err());
        assert!(parse("zbiz:/note").is_err());
    }

    /// product 和参数名是白名单字符：放行 `/` `.` 就等于允许拼出伪路径。
    #[test]
    fn rejects_invalid_token() {
        assert!(parse("zbiz://no te/open").is_err());
        assert!(parse("zbiz://note/../etc").is_err());
        assert!(parse("zbiz://note/open?fi le=x").is_err());
        assert!(build("note", "open", &params(&[("a.b", "1")])).is_err());
    }

    #[test]
    fn rejects_control_chars() {
        assert!(build("note", "open", &params(&[("file", "/tmp/a\nb")])).is_err());
        assert!(build("note", "open", &params(&[("file", "/tmp/a\0b")])).is_err());
    }

    #[test]
    fn rejects_bad_escape() {
        assert!(parse("zbiz://note/open?file=%2").is_err());
        assert!(parse("zbiz://note/open?file=%ZZ").is_err());
    }

    #[test]
    fn rejects_duplicate_param() {
        assert!(parse("zbiz://note/open?file=/a&file=/b").is_err());
    }

    #[test]
    fn empty_value_still_parses() {
        // `?flag=` 这种空值是合法的：请求体里没有值不等于格式错
        let h = parse("zbiz://box/tool?flag=").unwrap();
        assert_eq!(h.param("flag"), Some(""));
    }

    #[test]
    fn from_args_picks_only_deeplinks() {
        let args = vec![
            "/Applications/z-biz-tool-note.app/Contents/MacOS/z-biz-tool-note".to_string(),
            "-psn_0_12345".to_string(),
            "zbiz://note/open?file=/tmp/a.md".to_string(),
            "zbiz://坏深链".to_string(),
        ];
        let got = from_args(args);
        assert_eq!(got.len(), 1, "只有一条合法深链: {got:?}");
        assert_eq!(got[0].product, "note");
    }

    #[test]
    fn queue_dedupes_and_drains() {
        let q = Pending::default();
        let a = parse("zbiz://note/open?file=/tmp/a.md").unwrap();
        assert_eq!(q.push(vec![a.clone(), a.clone()]), 1, "重复只收一份");
        assert_eq!(q.take(), vec![a]);
        assert!(q.take().is_empty(), "取走后必须清空");
    }
}
