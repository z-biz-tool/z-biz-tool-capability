//! 门面行为测试：验证 `lib.rs` 暴露的公共 API，而不是内部实现。
//!
//! 模块内测试（68 条）覆盖协议细节与解码边界，这里只钉四件**下游真正依赖**的事：
//!
//! 1. 十套实现用过的配置写法都能解析（迁移不能要求用户改配置）
//! 2. 一份真实形态的响应体，四种协议都能取到正文
//! 3. 错误分类能让调用方决定「提示用户」还是「重试」
//! 4. 密钥不出现在任何错误文本里

use cap_llm::protocol::{build_request_body, parse_response, parse_stream_delta};
use cap_llm::{ErrorKind, LlmConfig, LlmError, Message, ProviderKind, Role};
use serde_json::json;

fn cfg(kind: ProviderKind) -> LlmConfig {
    LlmConfig {
        kind,
        base_url: String::new(),
        api_key: "sk-secret-abcdefghijklmnop".into(),
        model: "test-model".into(),
        ..Default::default()
    }
}

#[test]
fn version_is_exposed_for_logging() {
    assert!(!cap_llm::VERSION.is_empty());
}

#[test]
fn every_legacy_provider_spelling_parses() {
    // 这些字符串散落在十个仓的配置文件、设置面板、环境变量里。
    // 迁移的唯一要求是「用户不用改东西」，所以一个都不能漏。
    let legacy = [
        "openai",
        "anthropic",
        "ollama",
        "custom",
        "claude",
        "gemini",
        "minimax",
        "deepseek",
        "qwen",
        "local",
    ];
    for raw in legacy {
        let kind = ProviderKind::parse(raw).unwrap_or_else(|e| panic!("{raw} 解析失败: {e}"));
        // 每个都该落到一个能用的协议上
        assert!(
            matches!(
                kind.protocol(),
                cap_llm::Protocol::OpenAiCompat
                    | cap_llm::Protocol::Anthropic
                    | cap_llm::Protocol::Gemini
                    | cap_llm::Protocol::Ollama
            ),
            "{raw} 没有可用协议"
        );
    }
}

#[test]
fn one_message_list_produces_four_valid_protocol_bodies() {
    // 同一份输入，四种协议各生成一份合法报文 —— 这是「归并」的核心承诺
    let msgs = [Message::system("你是助手"), Message::user("你好")];

    let openai = build_request_body(&cfg(ProviderKind::OpenAi), &msgs, false, &[]);
    assert_eq!(openai["messages"].as_array().unwrap().len(), 2);
    assert_eq!(openai["messages"][0]["role"], "system");

    let anthropic = build_request_body(&cfg(ProviderKind::Anthropic), &msgs, false, &[]);
    assert_eq!(anthropic["system"], "你是助手");
    assert_eq!(anthropic["messages"].as_array().unwrap().len(), 1);
    assert!(anthropic["max_tokens"].is_number(), "Anthropic 必须带 max_tokens");

    let gemini = build_request_body(&cfg(ProviderKind::Gemini), &msgs, false, &[]);
    assert_eq!(gemini["systemInstruction"]["parts"][0]["text"], "你是助手");
    assert_eq!(gemini["contents"].as_array().unwrap().len(), 1);

    let ollama = build_request_body(&cfg(ProviderKind::Ollama), &msgs, false, &[]);
    assert_eq!(ollama["messages"].as_array().unwrap().len(), 2);
}

#[test]
fn real_shaped_responses_from_four_vendors_all_yield_text() {
    let cases = [
        (
            ProviderKind::OpenAi,
            json!({ "choices": [{ "message": { "content": "四家校" } }] }),
            "四家校",
        ),
        (
            ProviderKind::Anthropic,
            json!({ "content": [{ "type": "text", "text": "四家校" }] }),
            "四家校",
        ),
        (
            ProviderKind::Gemini,
            json!({ "candidates": [{ "content": { "parts": [{ "text": "四家校" }] } }] }),
            "四家校",
        ),
        (
            ProviderKind::Ollama,
            json!({ "message": { "content": "四家校" }, "done": true }),
            "四家校",
        ),
    ];
    for (kind, payload, want) in cases {
        let reply = parse_response(&cfg(kind), &payload)
            .unwrap_or_else(|e| panic!("{kind:?} 解析失败: {e}"));
        assert_eq!(reply.content, want, "{kind:?} 取错了正文");
    }
}

#[test]
fn stream_deltas_follow_each_vendors_own_shape() {
    let cases = [
        (
            ProviderKind::OpenAi,
            json!({ "choices": [{ "delta": { "content": "字" } }] }),
        ),
        (
            ProviderKind::Anthropic,
            json!({ "type": "content_block_delta", "delta": { "text": "字" } }),
        ),
        (
            ProviderKind::Gemini,
            json!({ "candidates": [{ "content": { "parts": [{ "text": "字" }] } }] }),
        ),
        (
            ProviderKind::Ollama,
            json!({ "message": { "content": "字" }, "done": false }),
        ),
    ];
    for (kind, payload) in cases {
        let (text, _, _) = parse_stream_delta(&cfg(kind), &payload);
        assert_eq!(text.as_deref(), Some("字"), "{kind:?} 流式增量取错");
    }
}

#[test]
fn error_kinds_let_callers_decide_retry_vs_prompt() {
    // 「没配 key」该弹设置面板；「超时」该重试；两者不能混成一个字符串
    let not_configured = LlmError::not_configured("未填 API Key");
    assert_eq!(not_configured.kind, ErrorKind::NotConfigured);
    assert!(!not_configured.kind.is_retryable());

    let timeout = LlmError::new(ErrorKind::Timeout, "超时");
    assert!(timeout.kind.is_retryable());

    let unauthorized = LlmError::http(401, "key 无效");
    assert_eq!(unauthorized.kind, ErrorKind::Http);
    assert_eq!(unauthorized.status, Some(401));
    // 401 重试一百次还是 401
    assert!(!unauthorized.kind.is_retryable());
    // Display 要带状态码，否则日志里只剩「key 无效」，看不出是哪一层拒的
    assert!(unauthorized.to_string().contains("401"));
}

#[test]
fn role_enum_prevents_typos_at_compile_time() {
    // 旧实现用字符串写 role，拼错只会在服务端 400
    assert_eq!(Role::System.as_str(), "system");
    assert_eq!(Role::User.as_str(), "user");
    assert_eq!(Role::Assistant.as_str(), "assistant");
}

#[test]
fn api_key_never_appears_in_redacted_text() {
    let key = "sk-secret-abcdefghijklmnop";
    let leaked = format!("upstream said: your Authorization: Bearer {key} is invalid");
    let safe = cap_llm::redact(&leaked, key);
    assert!(!safe.contains(key));
}

#[test]
fn truncation_never_panics_on_cjk() {
    // 旧实现 `&s[..300]` 在中文上会 panic
    let long = "很长的中文错误信息".repeat(100);
    let out = cap_llm::truncate_chars(&long, 10);
    assert!(out.chars().count() <= 11);
    assert!(!out.is_empty());
}
