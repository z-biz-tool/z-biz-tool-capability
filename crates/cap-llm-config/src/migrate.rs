//! 旧配置迁移 —— 把散落各处的历史配置搬进统一落点，并抹掉明文密钥。
//!
//! # 为什么要迁移
//!
//! 用户在改造前要在每个仓各配一遍。改造后如果不给迁移，存量用户要重配十遍
//! API Key —— 这是「统一」最容易翻车的地方：技术上统一了，用户体验却倒退。
//!
//! # 迁移的两半
//!
//! | 半 | 谁做 | 为什么 |
//! |---|---|---|
//! | **读旧配置 + 解密** | 各仓自己做 | terminal/mind/pet 用自建 AES-256-GCM，aigen 用 XChaCha20 信封。**能力层重复实现这四种加密是不划算的** —— 各仓已有能解开自己的代码，让它们读出来再写进共享落点即可 |
//! | **写共享落点 + 抹明文** | 本 crate | 落点、原子写、密钥库写入规则必须**只有一份**，否则又是一批漂移 |
//!
//! 所以本模块提供：
//! - [`LegacyCandidate`] + [`scan_legacy`]：扫出历史落点，把明文 JSON 形态
//!   的旧配置解出来（**只处理明文**，加密的交给各仓）；
//! - [`write_migrated`]：写进共享落点，并**抹掉旧文件里的明文密钥**。
//!
//! # 抹明文的语义
//!
//! 抹的是**密钥字段**，不是整个文件。理由：旧文件里可能还存着该仓要用的其它设置，
//! 整文件删掉会把无关设置一起弄丢。只抹密钥字段后，即便该仓还没完全切换，
//! 盘上也不再有一份能直接用的明文 Key。
//!
//! 抹除**必须发生在写入共享落点之后** —— 顺序反了就是「密钥丢了且没搬走」。

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::store::{
    atomic_write, config_path, load_config, save_config, shared_secret_store, LlmConfigFile,
    StoreError,
};

/// 扫出的一份历史配置。
#[derive(Debug, Clone)]
pub struct LegacyCandidate {
    /// 旧配置文件路径。
    pub path: PathBuf,
    /// 从哪个仓来的（用于诊断信息）。
    pub source: &'static str,
    /// 供应商标识（preset id 或 provider 名）。
    pub provider: String,
    /// 服务地址。
    pub base_url: String,
    /// 模型名。
    pub model: String,
    /// 明文密钥。**只在迁移瞬间存在于内存**。
    pub api_key: String,
    /// 温度（部分仓有）。
    pub temperature: Option<f32>,
    /// 超时毫秒（部分仓以秒存，需换算）。
    pub timeout_ms: Option<u64>,
}

/// 一处历史落点的描述。
struct LegacySite {
    /// 相对家目录的路径。
    rel: &'static str,
    /// 来源仓名。
    source: &'static str,
}

/// 全部**明文**历史落点。
///
/// 只列明文的：加密的那几处（terminal / mind / pet / aigen）由各仓自己读，
/// 本 crate 不重复实现 AES-GCM 与 XChaCha20 —— 见模块文档的分工说明。
fn legacy_sites() -> Vec<LegacySite> {
    vec![
        // file：app_config_dir()/com.zifang.z-biz-tool-file/llm.json
        LegacySite {
            rel: "Library/Application Support/com.zifang.z-biz-tool-file/llm.json",
            source: "file",
        },
        LegacySite {
            rel: ".config/com.zifang.z-biz-tool-file/llm.json",
            source: "file",
        },
        LegacySite {
            rel: "AppData/Roaming/com.zifang.z-biz-tool-file/llm.json",
            source: "file",
        },
        // db：**两处**。db 仓的 `get_data_dir()` 用的是
        // `dirs::data_local_dir()/z-biz-tool-db`（**没有 bundle id 前缀**），
        // 而 bundle id `com.zifang.z-biz-tool-db` 那条路是 Tauri 的
        // `app_data_dir()` —— 两者不是同一个目录，只登记一条就会漏扫。
        //
        // 漏扫的后果很具体：存量用户的明文 Key 既不会被搬走（功能退化），
        // 也不会被抹掉（**明文仍留在盘上**）—— 比不迁移更糟。
        LegacySite {
            rel: "Library/Application Support/com.zifang.z-biz-tool-db/ai_config.json",
            source: "db",
        },
        LegacySite {
            rel: ".local/share/com.zifang.z-biz-tool-db/ai_config.json",
            source: "db",
        },
        LegacySite {
            rel: "AppData/Roaming/com.zifang.z-biz-tool-db/ai_config.json",
            source: "db",
        },
        // db 真实落点：dirs::data_local_dir()/z-biz-tool-db/ai_config.json
        LegacySite {
            rel: "Library/Application Support/z-biz-tool-db/ai_config.json",
            source: "db",
        },
        LegacySite {
            rel: ".local/share/z-biz-tool-db/ai_config.json",
            source: "db",
        },
        LegacySite {
            rel: "AppData/Local/z-biz-tool-db/ai_config.json",
            source: "db",
        },
        // kb：app_data_dir()/com.zifang.z-biz-tool-kb/llm_config.json
        LegacySite {
            rel: "Library/Application Support/com.zifang.z-biz-tool-kb/llm_config.json",
            source: "kb",
        },
        LegacySite {
            rel: ".local/share/com.zifang.z-biz-tool-kb/llm_config.json",
            source: "kb",
        },
        LegacySite {
            rel: "AppData/Roaming/com.zifang.z-biz-tool-kb/llm_config.json",
            source: "kb",
        },
    ]
}

/// 扫出所有可识别的明文历史配置。
///
/// 只读，不改任何东西。返回的候选里 `api_key` 可能是空串
/// （用户填了地址和模型但没填 Key）—— 那种也是有效配置，要搬。
pub fn scan_legacy() -> Vec<LegacyCandidate> {
    let Some(home) = crate::store::home_dir() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for site in legacy_sites() {
        let path = home.join(site.rel);
        let Some(candidate) = read_legacy_file(&path, site.source) else {
            continue;
        };
        out.push(candidate);
    }
    out
}

/// 读一个旧文件并解出字段。文件不存在或内容不认识返回 `None`。
fn read_legacy_file(path: &Path, source: &'static str) -> Option<LegacyCandidate> {
    let text = fs::read_to_string(path).ok()?;
    let json: Value = serde_json::from_str(&text).ok()?;
    let obj = json.as_object()?;

    let provider = pick_str(obj, &["preset", "provider", "providerId"]).unwrap_or_default();
    let base_url = pick_str(
        obj,
        &["baseUrl", "base_url", "baseURL", "endpoint", "apiEndpoint"],
    )
    .unwrap_or_default();
    let model =
        pick_str(obj, &["model", "modelName", "model_name", "defaultModel"]).unwrap_or_default();
    let api_key = pick_str(obj, &["apiKey", "api_key", "key", "token"]).unwrap_or_default();

    // 什么都没配的文件不值得搬（那只是模板默认值）。
    if provider.is_empty() && base_url.is_empty() && model.is_empty() && api_key.is_empty() {
        return None;
    }

    let temperature = obj
        .get("temperature")
        .and_then(|v| v.as_f64())
        .map(|v| v as f32);
    let timeout_ms = obj
        .get("timeoutMs")
        .and_then(|v| v.as_u64())
        .or_else(|| {
            // file 仓存的是秒，换算成毫秒。
            obj.get("timeout_secs")
                .and_then(|v| v.as_u64())
                .map(|s| s.saturating_mul(1000))
        })
        .or_else(|| {
            obj.get("timeoutSecs")
                .and_then(|v| v.as_u64())
                .map(|s| s.saturating_mul(1000))
        });

    Some(LegacyCandidate {
        path: path.to_path_buf(),
        source,
        provider,
        base_url,
        model,
        api_key,
        temperature,
        timeout_ms,
    })
}

/// 从对象里取第一个存在的字符串字段（按候选名顺序）。
fn pick_str(obj: &serde_json::Map<String, Value>, names: &[&str]) -> Option<String> {
    for name in names {
        if let Some(v) = obj.get(*name).and_then(|v| v.as_str()) {
            let t = v.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// 迁移结果。
#[derive(Debug, Default)]
pub struct MigrationReport {
    /// 是否搬了东西。
    pub migrated: bool,
    /// 从哪搬的（仓名）。
    pub from: Option<String>,
    /// 旧文件里的明文密钥是否已抹掉。
    pub plaintext_scrubbed: bool,
    /// 过程中的说明（给人看的）。
    pub notes: Vec<String>,
}

/// 迁移历史配置到统一落点。
///
/// ## 幂等
///
/// 新落点**已有配置**时直接返回，不覆盖。这是刻意的：
/// 十个仓各自启动时都会调它，先到先得，后来者不能把先到的覆盖掉。
/// 否则用户先开 note 再开 terminal，terminal 的启动会拿自己那份（可能是默认值）
/// 把 note 刚配好的冲掉 —— 这种 bug 极难复现且极难解释。
///
/// ## 抹明文
///
/// 写入成功**之后**才抹旧文件里的密钥字段。抹不掉只记 note，不算失败 ——
/// 迁移本身已经成功，不该因为擦屁股失败而让用户看到「迁移失败」。
pub fn migrate_legacy_configs() -> Result<MigrationReport, StoreError> {
    let home = crate::store::home_dir().ok_or_else(|| {
        StoreError::Io("无法确定用户家目录（HOME / USERPROFILE 均未设置）".into())
    })?;
    migrate_legacy_in(&home)
}

/// [`migrate_legacy_configs`] 的可注入形态：扫描根由调用方给定。
///
/// **为什么要有它**：全量版按 `HOME` 扫旧落点，而 `HOME` 在 macOS 上
/// 同时决定 Security 框架的默认钥匙串位置 —— 测试里改 `HOME` 会让
/// 密钥库整个失效（"A default keychain could not be found"），
/// 隔离与密钥库**不能同时成立**。各仓要测「自己的旧文件会被搬走」时，
/// 用本函数把扫描根指到临时目录，`HOME` 保持不动。
///
/// 与全量版同一套幂等与抹明文语义。
pub fn migrate_legacy_in(home: &Path) -> Result<MigrationReport, StoreError> {
    let mut report = MigrationReport::default();

    // 新落点已有配置 ⇒ 不动。
    if let Some(existing) = load_config()? {
        report.notes.push(format!(
            "统一配置已存在（厂商 {}，模型 {}），跳过迁移。",
            if existing.preset.is_empty() {
                existing.provider.as_str()
            } else {
                existing.preset.as_str()
            },
            if existing.model.is_empty() {
                "未填"
            } else {
                existing.model.as_str()
            }
        ));
        return Ok(report);
    }

    let mut candidates = Vec::new();
    for site in legacy_sites() {
        let path = home.join(site.rel);
        if let Some(candidate) = read_legacy_file(&path, site.source) {
            candidates.push(candidate);
        }
    }
    let Some(first) = candidates
        .into_iter()
        .find(|c| !c.api_key.is_empty() || !c.model.is_empty())
    else {
        report.notes.push("未发现可迁移的历史配置。".into());
        return Ok(report);
    };

    let cfg = LlmConfigFile {
        preset: provider_of(&first),
        provider: provider_of(&first),
        base_url: first.base_url.clone(),
        model: first.model.clone(),
        temperature: first.temperature,
        max_tokens: None,
        timeout_ms: first.timeout_ms,
        updated_at_ms: now_ms(),
    };

    // 顺序要紧：**先写新落点，再抹旧明文**。
    save_config(&cfg)?;
    if !first.api_key.is_empty() {
        let store = shared_secret_store();
        store.set(&first.api_key)?;
    }

    report.migrated = true;
    report.from = Some(first.source.to_string());

    match scrub_plaintext_key(&first.path) {
        Ok(true) => report.plaintext_scrubbed = true,
        Ok(false) => report.notes.push(format!(
            "旧文件 {} 里没有明文密钥字段，无需抹除。",
            first.path.display()
        )),
        Err(e) => report.notes.push(format!(
            "旧文件 {} 的明文密钥抹除失败：{}（迁移本身已完成）",
            first.path.display(),
            e
        )),
    }

    report.notes.push(format!(
        "已从 {} 迁移到 {}。",
        first.source,
        config_path()?.display()
    ));

    Ok(report)
}

/// 迁移后写进统一落点的供应商标识。
///
/// **不能留空**：各仓的 `to_cap` 走 `ProviderKind::parse_with_preset`，
/// 那个函数对空串直接报错（"未选择 AI 提供商"）—— 迁移把配置搬过来了，
/// 却因为没有 provider 而让用户「明明配过却说没配」。
///
/// 旧文件里没写 provider 的（db 仓的 `ai_config.json` 就只有
/// base_url / api_key / model 三个字段），按地址猜：
/// - 命中某家厂商的官方地址 → 记那家的 preset id；
/// - 猜不出 → 记 `custom`（自定义 OpenAI 兼容地址），语义上最保守，
///   而且 `base_url` 原样保留，用户照样能用。
fn provider_of(candidate: &LegacyCandidate) -> String {
    let p = candidate.provider.trim();
    if !p.is_empty() {
        return p.to_string();
    }
    if let Some(preset) = presets_match(&candidate.base_url) {
        return preset;
    }
    "custom".to_string()
}

/// 按地址反查预设 id。命中不了返回 `None`。
fn presets_match(base_url: &str) -> Option<String> {
    let url = base_url.trim();
    if url.is_empty() {
        return None;
    }
    cap_llm::preset::PRESETS
        .iter()
        .find(|p| {
            !p.base_url.is_empty()
                && (url == p.base_url
                    || url.starts_with(&format!("{}/", p.base_url.trim_end_matches('/'))))
        })
        .map(|p| p.id.to_string())
}

/// 把旧文件里的密钥字段抹掉，**只抹密钥**，其余字段原样保留。
///
/// 返回 `Ok(true)` 表示确实抹掉了什么。
fn scrub_plaintext_key(path: &Path) -> Result<bool, StoreError> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(StoreError::Io(e.to_string())),
    };
    let Ok(mut json) = serde_json::from_str::<Value>(&text) else {
        return Ok(false);
    };
    let Some(obj) = json.as_object_mut() else {
        return Ok(false);
    };

    const KEY_FIELDS: &[&str] = &["apiKey", "api_key", "key", "token"];
    let mut scrubbed = false;
    for name in KEY_FIELDS {
        if let Some(v) = obj.get_mut(*name) {
            if v.as_str().map(|s| !s.trim().is_empty()).unwrap_or(false) {
                *v = Value::String(String::new());
                scrubbed = true;
            }
        }
    }
    if !scrubbed {
        return Ok(false);
    }

    let out = serde_json::to_string_pretty(&json)
        .map_err(|e| StoreError::Parse(format!("序列化失败：{}", e)))?;
    atomic_write(path, out.as_bytes())?;
    Ok(true)
}

/// 当前时刻（毫秒时间戳）。写失败不影响迁移 —— 时间戳只是诊断辅助。
fn now_ms() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// 各仓把「自己那份旧配置」写进统一落点的入口。
///
/// 加密仓（terminal / mind / pet / aigen）用这个：它们自己读自己的旧配置、
/// 自己解密，然后调用本函数搬过来。
///
/// ## 幂等
///
/// 统一落点已有配置时**不覆盖**，直接返回 `Ok(false)`（表示「没搬」）。
/// 与 [`migrate_legacy_configs`] 同一条规矩：先到先得。
pub fn write_migrated(
    provider: &str,
    base_url: &str,
    model: &str,
    api_key: &str,
    temperature: Option<f32>,
    timeout_ms: Option<u64>,
) -> Result<bool, StoreError> {
    if load_config()?.is_some() {
        return Ok(false);
    }
    let cfg = LlmConfigFile {
        preset: provider.to_string(),
        provider: provider.to_string(),
        base_url: base_url.to_string(),
        model: model.to_string(),
        temperature,
        max_tokens: None,
        timeout_ms,
        updated_at_ms: now_ms(),
    };
    save_config(&cfg)?;
    if !api_key.trim().is_empty() {
        shared_secret_store().set(api_key)?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_snake_case_legacy_file() {
        let dir = std::env::temp_dir().join(format!("cap-llm-config-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("llm.json");
        fs::write(
            &path,
            r#"{"provider":"openai","api_key":"sk-plain","base_url":"https://api.openai.com/v1","model":"gpt-4o","timeout_secs":30,"temperature":0.5}"#,
        )
        .unwrap();

        let c = read_legacy_file(&path, "file").expect("应能解出");
        assert_eq!(c.provider, "openai");
        assert_eq!(c.api_key, "sk-plain");
        assert_eq!(c.base_url, "https://api.openai.com/v1");
        assert_eq!(c.model, "gpt-4o");
        assert_eq!(c.temperature, Some(0.5));
        // timeout_secs 是秒，要换算成毫秒
        assert_eq!(c.timeout_ms, Some(30_000));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reads_camel_case_legacy_file() {
        let dir = std::env::temp_dir().join(format!("cap-llm-config-camel-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ai_config.json");
        fs::write(
            &path,
            r#"{"baseUrl":"https://x/v1","apiKey":"sk-camel","model":"m1"}"#,
        )
        .unwrap();

        let c = read_legacy_file(&path, "db").expect("应能解出");
        assert_eq!(c.base_url, "https://x/v1");
        assert_eq!(c.api_key, "sk-camel");
        assert_eq!(c.model, "m1");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_file_is_not_a_candidate() {
        let dir = std::env::temp_dir().join(format!("cap-llm-config-empty-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("llm.json");
        fs::write(
            &path,
            r#"{"provider":"","base_url":"","model":"","api_key":""}"#,
        )
        .unwrap();
        assert!(read_legacy_file(&path, "file").is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scrub_only_touches_the_key_field() {
        let dir = std::env::temp_dir().join(format!("cap-llm-config-scrub-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("llm.json");
        fs::write(
            &path,
            r#"{"provider":"openai","api_key":"sk-secret","base_url":"https://x/v1","model":"m"}"#,
        )
        .unwrap();

        assert!(scrub_plaintext_key(&path).expect("抹除应成功"));
        let after: Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).expect("应仍是合法 JSON");
        assert_eq!(after["api_key"].as_str(), Some(""));
        // 其余字段必须原样保留
        assert_eq!(after["provider"].as_str(), Some("openai"));
        assert_eq!(after["base_url"].as_str(), Some("https://x/v1"));
        assert_eq!(after["model"].as_str(), Some("m"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scrub_is_a_noop_without_plaintext_key() {
        let dir =
            std::env::temp_dir().join(format!("cap-llm-config-noscrub-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("llm.json");
        fs::write(&path, r#"{"provider":"openai","api_key":""}"#).unwrap();
        assert!(!scrub_plaintext_key(&path).expect("应成功"));
        fs::remove_dir_all(&dir).ok();
    }

    // ============ db 仓落点与 provider 反查 ============

    /// db 仓的旧落点是 `dirs::data_local_dir()/z-biz-tool-db/ai_config.json`
    /// —— **没有 bundle id 前缀**。迁移器只登记 `com.zifang.z-biz-tool-db`
    /// 那一族时，存量用户的明文 Key 既不会被搬走、也不会被抹掉。
    ///
    /// 这条测试钉住「真实路径也在扫描范围内」。
    #[test]
    fn db_real_data_local_dir_path_is_scanned() {
        let sites = legacy_sites();
        let rels: Vec<&str> = sites.iter().map(|s| s.rel).collect();
        for expect in [
            "Library/Application Support/z-biz-tool-db/ai_config.json",
            ".local/share/z-biz-tool-db/ai_config.json",
            "AppData/Local/z-biz-tool-db/ai_config.json",
        ] {
            assert!(
                rels.contains(&expect),
                "db 真实落点没登记，存量用户的明文 Key 不会被搬走也不会被抹掉：缺 {}",
                expect
            );
        }
        // bundle id 那一族也要留着 —— 早期版本可能落在那儿
        assert!(
            rels.contains(&"Library/Application Support/com.zifang.z-biz-tool-db/ai_config.json")
        );
    }

    /// db 旧配置只有 base_url / api_key / model，**没有 provider**。
    /// 迁移后 provider 若为空，各仓的 `to_cap` 会报「未选择 AI 提供商」——
    /// 用户明明配过却被说没配。
    #[test]
    fn provider_is_guessed_from_the_legacy_base_url() {
        let c = LegacyCandidate {
            path: PathBuf::from("/tmp/x"),
            source: "db",
            provider: String::new(),
            base_url: "https://api.deepseek.com/v1".into(),
            model: "deepseek-chat".into(),
            api_key: "sk-x".into(),
            temperature: None,
            timeout_ms: None,
        };
        assert_eq!(provider_of(&c), "deepseek", "官方地址应反查回厂商 id");

        // 猜不出就落到 custom，语义最保守，地址原样保留
        let c2 = LegacyCandidate {
            base_url: "https://my-proxy.internal/v1".into(),
            ..c.clone()
        };
        assert_eq!(provider_of(&c2), "custom");

        // 旧文件本来就写了 provider —— 以它为准
        let c3 = LegacyCandidate {
            provider: "openai".into(),
            base_url: "https://api.deepseek.com/v1".into(),
            ..c.clone()
        };
        assert_eq!(provider_of(&c3), "openai");
    }

    /// 反查要认「官方地址 + 斜杠后缀」（用户常把 /v1 之外的路径也带上）。
    #[test]
    fn preset_lookup_tolerates_path_suffixes() {
        assert_eq!(
            presets_match("https://api.deepseek.com/v1"),
            Some("deepseek".to_string())
        );
        assert_eq!(
            presets_match("https://api.deepseek.com/v1/chat/completions"),
            Some("deepseek".to_string())
        );
        assert_eq!(presets_match("https://my-proxy.internal/v1"), None);
        assert_eq!(presets_match(""), None);
    }

    /// 端到端：db 的旧文件被搬进统一落点，明文被抹掉。
    ///
    /// 用 `migrate_legacy_in` 而不是全量版 —— 改 `HOME` 在 macOS 上会
    /// 让 Security 框架找不到默认钥匙串，隔离与密钥库不能同时成立。
    #[test]
    fn db_legacy_file_is_migrated_and_scrubbed() {
        // 环境变量是进程级的：与 testing.rs 里的测试共用同一把 crate 级锁，
        // 否则并发时后构造的 IsolatedLlm 会抢走目录，让本测试读不到自己的配置。
        let _guard = crate::testing::lock_llm_env();
        let _iso = crate::testing::IsolatedLlm::new("db-migrate");

        let home =
            std::env::temp_dir().join(format!("cap-llm-config-dbhome-{}", std::process::id()));
        let legacy_dir = home.join("Library/Application Support/z-biz-tool-db");
        fs::create_dir_all(&legacy_dir).unwrap();
        let legacy = legacy_dir.join("ai_config.json");
        fs::write(
            &legacy,
            r#"{"base_url":"https://api.deepseek.com/v1","api_key":"sk-db-plaintext","model":"deepseek-chat"}"#,
        )
        .unwrap();

        let report = migrate_legacy_in(&home).expect("迁移应成功");
        assert!(report.migrated, "db 的旧文件应被搬走：{:?}", report);
        assert_eq!(report.from.as_deref(), Some("db"));
        assert!(report.plaintext_scrubbed, "旧文件里的明文 Key 应被抹掉");

        let cfg = load_config().expect("读配置").expect("统一落点应有配置");
        assert_eq!(cfg.preset, "deepseek", "provider 应按地址反查出来");
        assert_eq!(cfg.base_url, "https://api.deepseek.com/v1");
        assert_eq!(cfg.model, "deepseek-chat");

        // 旧文件里不该再有明文
        let after = fs::read_to_string(&legacy).unwrap();
        assert!(
            !after.contains("sk-db-plaintext"),
            "旧文件里仍有明文 Key：{}",
            after
        );

        fs::remove_dir_all(&home).ok();
    }
}
