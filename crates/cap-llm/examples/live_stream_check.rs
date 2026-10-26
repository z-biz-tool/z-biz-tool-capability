//! 端到端冒烟：对着一个**真实 HTTP 服务**跑非流式 + 流式，并检查中文完整性。
//!
//! 用途有两个：
//! 1. 没有 API key 也能验证整条链路（配一个本地 mock，见下）；
//! 2. 换真实厂商时改 `CAP_LLM_BASE_URL` / `CAP_LLM_MODEL` 即可复用，不用改代码。
//!
//! ```bash
//! # 用本地 mock（会把中文故意拆到多个 HTTP chunk 里）
//! CAP_LLM_BASE_URL=http://127.0.0.1:8799/v1 CAP_LLM_MODEL=mock \
//!   cargo run -p cap-llm --example live_stream_check
//!
//! # 用真实厂商
//! CAP_LLM_BASE_URL=https://api.minimaxi.com/v1 CAP_LLM_MODEL=MiniMax-M3.1-Flash \
//! CAP_LLM_API_KEY=sk-xxx \
//!   cargo run -p cap-llm --example live_stream_check
//! ```
//!
//! 退出码：0 = 两项都通过；1 = 有失败。CI 可以直接用。

use std::time::Duration;

use cap_llm::{chat, chat_stream, Delta, LlmConfig, Message, ProviderKind};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let Ok(base_url) = std::env::var("CAP_LLM_BASE_URL") else {
        eprintln!("需要 CAP_LLM_BASE_URL（例如 http://127.0.0.1:8799/v1）");
        std::process::exit(2);
    };

    let cfg = LlmConfig {
        kind: ProviderKind::parse(&std::env::var("CAP_LLM_PROVIDER").unwrap_or("openai".into()))
            .expect("CAP_LLM_PROVIDER 无法解析"),
        base_url,
        api_key: std::env::var("CAP_LLM_API_KEY").unwrap_or_default(),
        model: std::env::var("CAP_LLM_MODEL").unwrap_or("mock".into()),
        timeout: Duration::from_secs(60),
        temperature: Some(0.2),
        max_tokens: Some(64),
    };

    let mut failed = false;

    // ── 1. 非流式 ────────────────────────────────────────────
    println!("[1/2] 非流式 chat …");
    match chat(&cfg, &[Message::user("你好，请回一句中文")]).await {
        Ok(r) => {
            println!(
                "  ✓ {} | {}ms | tokens={:?}",
                r.content, r.latency.as_millis(), r.usage.as_ref().and_then(|u| u.total_tokens)
            );
            if r.content.is_empty() {
                println!("  ✗ 正文为空");
                failed = true;
            }
        }
        Err(e) => {
            println!("  ✗ {e}");
            failed = true;
        }
    }

    // ── 2. 流式（重点是中文不能碎）───────────────────────────
    println!("[2/2] 流式 chat_stream …");
    let mut pieces = 0usize;
    let mut saw_done = false;
    match chat_stream(&cfg, &[Message::user("请输出一段中文")], |d| match d {
        Delta::Text(t) => {
            pieces += 1;
            print!("{t}");
        }
        Delta::Done => {
            saw_done = true;
        }
    })
    .await
    {
        Ok(r) => {
            println!();
            println!(
                "  ✓ {} 帧 / {} 字 / {}ms / done={}",
                pieces,
                r.content.chars().count(),
                r.latency.as_millis(),
                saw_done
            );
            if r.content.is_empty() {
                println!("  ✗ 流式正文为空");
                failed = true;
            }
            // 关键断言：中文没有被拆坏。U+FFFD 是解码失败的替换符，
            // 出现一次就说明「字节被当成字符切了」。
            if r.content.contains('\u{FFFD}') {
                println!("  ✗ 流式正文里出现替换符 U+FFFD —— 中文被拆坏了");
                failed = true;
            }
            if !saw_done {
                println!("  ✗ 没有收到 Done 事件");
                failed = true;
            }
        }
        Err(e) => {
            println!("\n  ✗ {e}");
            failed = true;
        }
    }

    if failed {
        eprintln!("\n冒烟失败");
        std::process::exit(1);
    }
    println!("\n冒烟通过");
}
