//! SSE 解码：字节流 → 完整行 → `data:` 负载。
//!
//! ## 为什么单独一个文件
//!
//! 这是十套实现里**最容易写错、最难靠肉眼发现**的一处。HTTP 分块不保证
//! 按字符边界切，一个三字节的汉字完全可能被劈成两个 chunk 到达：
//!
//! ```text
//! chunk1 = [0xE4, 0xBD]        // "你" 的前两字节
//! chunk2 = [0xA0, ...]         // "你" 的第三字节 + 后续
//! ```
//!
//! 每收到一个 chunk 就 `String::from_utf8_lossy` 一次，会得到 `�`、
//! 中文输出里夹着方块，而且**只在大模型吐长回答时偶发**，本地测不出来。
//! pet 仓在 2026 年为此专门修过一轮（`decode_streamed`），本文件是那次的
//! 推广版：把「不完整尾字节先留着」做成一个有状态解码器，并覆盖 CRLF / 非法字节。
//!
//! ## 三层职责
//!
//! ```text
//! bytes ──[Utf8LineDecoder]──> 行（保留未完成的尾巴）──[parse_data]──> JSON 负载
//! ```

/// 增量 UTF-8 行解码器。
///
/// 内部持有两个缓冲：`pending_bytes`（不完整的 UTF-8 序列）与
/// `pending_text`（不完整的行）。两者都必须跨 `push` 保留，否则就会
/// 出现「半字」或「半行」。
#[derive(Debug, Default)]
pub struct Utf8LineDecoder {
    /// 尚未构成完整 UTF-8 字符的尾部字节
    pending_bytes: Vec<u8>,
    /// 已解码但还没遇到换行的文本
    pending_text: String,
}

impl Utf8LineDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一段字节，返回本次能凑出的**完整行**（已去掉行尾 `\r`）。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.absorb(chunk);
        self.drain_lines()
    }

    /// 流结束时调用：把残留的「无换行结尾最后一行」也吐出来。
    /// 少了这一步，最后一个 chunk 若不带换行就会被整段丢掉 ——
    /// 表现是「回答总是少最后一句」。
    pub fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        // 还有残留字节就按 lossy 收尾（真非法字节，不能无限等）
        if !self.pending_bytes.is_empty() {
            let text = String::from_utf8_lossy(&self.pending_bytes).to_string();
            self.pending_bytes.clear();
            self.pending_text.push_str(&text);
        }
        if !self.pending_text.is_empty() {
            let line = std::mem::take(&mut self.pending_text);
            let line = line.trim_end_matches('\r').to_string();
            if !line.is_empty() {
                out.push(line);
            }
        }
        out
    }

    fn absorb(&mut self, chunk: &[u8]) {
        self.pending_bytes.extend_from_slice(chunk);

        // 循环而不是一次：吞掉一个非法字节后，**后面可能还有合法字节**。
        // 只处理一轮的话，`[0xFF, b'\n']` 里那个 `\n` 会永远留在 pending_bytes 里，
        // 行就切不出来 —— 表现为「流卡住不收尾」。
        loop {
            if self.pending_bytes.is_empty() {
                return;
            }
            match std::str::from_utf8(&self.pending_bytes) {
                Ok(text) => {
                    self.pending_text.push_str(text);
                    self.pending_bytes.clear();
                    return;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        if let Ok(text) = std::str::from_utf8(&self.pending_bytes[..valid]) {
                            self.pending_text.push_str(text);
                        }
                        self.pending_bytes.drain(..valid);
                        // 剩下的从头再来一轮判断
                        continue;
                    }
                    // valid == 0：开头就是坏字节
                    match e.error_len() {
                        // `error_len().is_some()` ⇒ 真非法字节，不是"还没收全"。
                        // 这时候不能一直等（会卡死整个流），用替换符吞掉继续。
                        Some(bad_len) => {
                            let take = bad_len.min(self.pending_bytes.len()).max(1);
                            self.pending_text.push('\u{FFFD}');
                            self.pending_bytes.drain(..take);
                            continue;
                        }
                        // `error_len().is_none()` ⇒ 只是尾部还没收全，留着等下一个 chunk
                        None => return,
                    }
                }
            }
        }
    }

    fn drain_lines(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(idx) = self.pending_text.find('\n') {
            let mut line = self.pending_text[..idx].to_string();
            if line.ends_with('\r') {
                line.pop();
            }
            self.pending_text.drain(..=idx);
            out.push(line);
        }
        out
    }
}

/// 从一行 SSE 里取出 `data:` 负载。
///
/// 返回 `None` 表示这行不携带负载：空行（事件分隔）、`:` 开头的注释
/// （很多网关用 `: ping` 保活）、`event:` / `id:` / `retry:` 字段。
pub fn parse_data(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with(':') {
        return None;
    }
    let payload = trimmed.strip_prefix("data:")?;
    // 规范上 data: 后可以有一个空格，也可以没有
    let payload = payload.strip_prefix(' ').unwrap_or(payload);
    if payload.is_empty() {
        return None;
    }
    Some(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_lines_across_chunks() {
        let mut d = Utf8LineDecoder::new();
        assert_eq!(d.push(b"data: a\n"), vec!["data: a"]);
        assert_eq!(d.push(b"data: b"), Vec::<String>::new());
        assert_eq!(d.push(b"\n"), vec!["data: b"]);
    }

    #[test]
    fn cjk_split_across_chunks_does_not_corrupt() {
        // "你好" 的字节序列被逐字节切开
        let full = "你好".as_bytes();
        let mut d = Utf8LineDecoder::new();
        let mut lines = Vec::new();
        for b in full.chunks(1) {
            lines.extend(d.push(b));
        }
        lines.extend(d.push(b"\n"));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "你好", "跨 chunk 的中文被拆坏了");
    }

    #[test]
    fn incomplete_tail_is_held_not_replaced() {
        let mut d = Utf8LineDecoder::new();
        let bytes = "中".as_bytes();
        assert!(d.push(&bytes[..2]).is_empty());
        let out = d.push(&[bytes[2], b'\n']);
        assert_eq!(out, vec!["中"]);
    }

    #[test]
    fn finish_flushes_last_line_without_newline() {
        let mut d = Utf8LineDecoder::new();
        assert!(d.push(b"data: tail").is_empty());
        assert_eq!(d.finish(), vec!["data: tail"]);
    }

    #[test]
    fn finish_does_not_emit_empty() {
        let mut d = Utf8LineDecoder::new();
        d.push(b"data: x\n");
        assert!(d.finish().is_empty());
    }

    #[test]
    fn handles_crlf() {
        let mut d = Utf8LineDecoder::new();
        assert_eq!(d.push(b"data: a\r\n"), vec!["data: a"]);
    }

    #[test]
    fn invalid_bytes_do_not_stall_stream() {
        let mut d = Utf8LineDecoder::new();
        // 0xFF 永远不可能出现在合法 UTF-8 里
        let out = d.push(&[0xFF, b'\n']);
        assert_eq!(out.len(), 1);
        assert!(out[0].contains('\u{FFFD}'));
    }

    #[test]
    fn parse_data_extracts_payload() {
        assert_eq!(parse_data("data: {\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(parse_data("data:{\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(parse_data("data: [DONE]"), Some("[DONE]"));
    }

    #[test]
    fn parse_data_skips_non_payload_lines() {
        assert_eq!(parse_data(""), None);
        assert_eq!(parse_data(": ping"), None);
        assert_eq!(parse_data("event: content_block_delta"), None);
        assert_eq!(parse_data("id: 42"), None);
        assert_eq!(parse_data("data:"), None);
    }
}
