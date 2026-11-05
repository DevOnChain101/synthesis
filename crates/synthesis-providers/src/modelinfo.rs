//! Model facts (context window, max output, vision, effort tiers) taken from a provider's model
//! list, cached under `$SYNT_HOME/cache/models/<provider-id>.json` and shared in place with the
//! provider that serves them. Holds no credentials.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use synthesis_config::Protocol;

use crate::provider::ModelInfo;

pub const SCHEMA_VERSION: u32 = 1;
/// A cache younger than this is used without asking the network.
pub const TTL_SECS: u64 = 24 * 3600;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelFacts {
    pub context_window: Option<u64>,
    pub max_output: Option<u64>,
    pub vision: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effort_tiers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_capabilities: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheFile {
    pub schema_version: u32,
    pub base_url: String,
    pub protocol: String,
    /// Unix seconds.
    pub fetched_at: u64,
    pub models: BTreeMap<String, ModelFacts>,
}

pub fn protocol_name(p: Protocol) -> &'static str {
    match p {
        Protocol::Anthropic => "anthropic",
        Protocol::OpenAi => "openai",
    }
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `<synthesis_home>/cache/models/<id>.json`; characters outside `[A-Za-z0-9._-]` become `_`.
pub fn cache_path(synthesis_home: &Path, provider_id: &str) -> PathBuf {
    let safe: String = provider_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    synthesis_home
        .join("cache")
        .join("models")
        .join(format!("{}.json", safe.trim_start_matches('.')))
}

/// The cache for this endpoint. A missing, corrupt, unknown-version or other-endpoint file is
/// `None` (never an error).
pub fn read_cache(path: &Path, base_url: &str, protocol: Protocol) -> Option<CacheFile> {
    let f: CacheFile = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (f.schema_version == SCHEMA_VERSION
        && f.base_url == base_url
        && f.protocol == protocol_name(protocol))
    .then_some(f)
}

pub fn is_fresh(f: &CacheFile, now: u64) -> bool {
    // a timestamp from the future is not trusted
    now.checked_sub(f.fetched_at)
        .is_some_and(|age| age < TTL_SECS)
}

pub fn write_cache(path: &Path, f: &CacheFile) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    synthesis_config::write_private_atomic(path, &serde_json::to_vec_pretty(f)?)
}

fn supported(v: Option<&Value>) -> Option<bool> {
    v?.get("supported")?.as_bool()
}

/// Id and facts of one listed model. Anthropic reports `capabilities.image_input.supported` and
/// `capabilities.effort.<tier>.supported`.
pub fn facts_of(m: &ModelInfo) -> Option<(String, ModelFacts)> {
    let id = m.id.as_str()?.to_string();
    let effort = m.capabilities.get("effort");
    let effort_tiers = ["low", "medium", "high", "xhigh", "max"]
        .into_iter()
        .filter(|t| supported(effort.and_then(|e| e.get(*t))) == Some(true))
        .map(String::from)
        .collect();
    let facts = ModelFacts {
        // 0 means unknown
        context_window: m.context_window.filter(|n| *n > 0),
        max_output: m.max_output.filter(|n| *n > 0),
        vision: supported(m.capabilities.get("image_input")),
        effort_tiers,
        raw_capabilities: (!m.capabilities.is_empty()).then(|| m.capabilities.clone()),
    };
    Some((id, facts))
}

/// Documented limits for models whose API does not report them (OpenAI's `/v1/models` has no
/// context length). Only used for `api.openai.com`. Prefix match, first row wins. Sources:
/// developers.openai.com/api/docs/models/{gpt-4.1,gpt-4o,gpt-6-luna}. Add a row only from a
/// documented number.
const OPENAI_KNOWN: &[(&str, u64, u64)] = &[
    ("gpt-6", 1_050_000, 128_000),
    ("gpt-4.1", 1_047_576, 32_768),
    ("gpt-4o", 128_000, 16_384),
];

pub fn builtin(openai_native: bool, model: &str) -> Option<(u64, u64)> {
    openai_native
        .then(|| OPENAI_KNOWN.iter().find(|r| model.starts_with(r.0)))
        .flatten()
        .map(|r| (r.1, r.2))
}

/// Facts by model id; replaced as a whole when the cache loads or a fetch completes.
#[derive(Default)]
pub struct ModelInfoStore(RwLock<(HashMap<String, ModelFacts>, bool)>);

impl ModelInfoStore {
    pub fn get(&self, model: &str) -> Option<ModelFacts> {
        let g = self.0.read().unwrap_or_else(|e| e.into_inner());
        g.0.get(model).cloned()
    }

    /// True when the facts are younger than the TTL (no fetch needed).
    pub fn fresh(&self) -> bool {
        self.0.read().unwrap_or_else(|e| e.into_inner()).1
    }

    pub fn set(&self, models: impl IntoIterator<Item = (String, ModelFacts)>, fresh: bool) {
        let mut g = self.0.write().unwrap_or_else(|e| e.into_inner());
        *g = (models.into_iter().collect(), fresh);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn file(base: &str, at: u64) -> CacheFile {
        CacheFile {
            schema_version: SCHEMA_VERSION,
            base_url: base.into(),
            protocol: "anthropic".into(),
            fetched_at: at,
            models: [(
                "m".to_string(),
                ModelFacts {
                    context_window: Some(1_000_000),
                    ..Default::default()
                },
            )]
            .into(),
        }
    }

    #[test]
    fn cache_round_trip_ttl_invalidation_and_corruption() {
        let dir = std::env::temp_dir().join(format!("synt-mi-{}", std::process::id()));
        let p = cache_path(&dir, "we/ird:id");
        assert!(p.ends_with("cache/models/we_ird_id.json"));
        write_cache(&p, &file("https://a", 1000)).unwrap();
        #[cfg(unix)]
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&p).unwrap().permissions())
                & 0o777,
            0o600
        );
        let f = read_cache(&p, "https://a", Protocol::Anthropic).unwrap();
        assert_eq!(f.models["m"].context_window, Some(1_000_000));
        assert!(is_fresh(&f, 1000 + TTL_SECS - 1));
        assert!(!is_fresh(&f, 1000 + TTL_SECS));
        assert!(!is_fresh(&f, 999), "future timestamp");
        // other endpoint or protocol: invalidated
        assert!(read_cache(&p, "https://b", Protocol::Anthropic).is_none());
        assert!(read_cache(&p, "https://a", Protocol::OpenAi).is_none());
        // unknown version and garbage: ignored
        let mut g = file("https://a", 1000);
        g.schema_version = 99;
        write_cache(&p, &g).unwrap();
        assert!(read_cache(&p, "https://a", Protocol::Anthropic).is_none());
        std::fs::write(&p, b"{not json").unwrap();
        assert!(read_cache(&p, "https://a", Protocol::Anthropic).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn anthropic_capabilities_become_facts() {
        let m = crate::provider::normalize_model(&json!({
            "id": "claude-x", "max_input_tokens": 1_000_000, "max_tokens": 64_000,
            "capabilities": {
                "image_input": {"supported": true},
                "effort": {"supported": true, "low": {"supported": true}, "high": {"supported": true},
                           "max": {"supported": false}, "xhigh": null}
            }
        }));
        let (id, f) = facts_of(&m).unwrap();
        assert_eq!(id, "claude-x");
        assert_eq!(
            (f.context_window, f.max_output, f.vision),
            (Some(1_000_000), Some(64_000), Some(true))
        );
        assert_eq!(f.effort_tiers, ["low", "high"]);
    }

    #[test]
    fn builtin_table_is_openai_native_only() {
        assert_eq!(builtin(true, "gpt-4.1-mini"), Some((1_047_576, 32_768)));
        assert_eq!(builtin(false, "gpt-4.1-mini"), None);
        assert_eq!(builtin(true, "mystery"), None);
    }
}
