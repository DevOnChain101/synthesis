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
/// A cache file larger than this is ignored.
const MAX_CACHE_BYTES: u64 = 4 << 20;
/// A context window outside this range is treated as unknown (too small compacts constantly,
/// too large never compacts).
const WINDOW_RANGE: std::ops::RangeInclusive<u64> = 1_024..=100_000_000;
const MIN_OUTPUT: u64 = 16;

/// A model id safe to store and print: 1..=256 characters, no whitespace, control, bidi,
/// zero-width or other invisible characters.
pub fn valid_id(id: &str) -> bool {
    (1..=256).contains(&id.chars().count())
        && !id
            .chars()
            .any(|c| c.is_whitespace() || synthesis_config::is_invisible_or_blank(c))
}

/// Sanity bounds for numbers that come from outside: an implausible window is unknown; an
/// output cap below 16 is unknown and one above the window is clamped to it.
pub fn clamp(window: Option<u64>, output: Option<u64>) -> (Option<u64>, Option<u64>) {
    let window = window.filter(|w| WINDOW_RANGE.contains(w));
    let output = output
        .filter(|o| *o >= MIN_OUTPUT)
        .map(|o| o.min(window.unwrap_or(*WINDOW_RANGE.end())));
    (window, output)
}

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

/// `<synthesis_home>/cache/models/<stem>-<hash>.json`. The stem is the id with characters outside
/// `[A-Za-z0-9._-]` replaced by `_` (a Windows device name gets a `p-` prefix); the hash is the
/// first 8 hex digits of the sha256 of the raw id, so no two ids share a file. An id with no
/// usable character is the hash alone.
pub fn cache_path(synthesis_home: &Path, provider_id: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let stem: String = provider_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    let stem = stem.trim_matches(['.', '_']);
    let hash: String = Sha256::digest(provider_id.as_bytes())[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let lower = stem.to_ascii_lowercase();
    let device = matches!(lower.as_str(), "con" | "prn" | "aux" | "nul")
        || ["com", "lpt"].iter().any(|p| {
            lower
                .strip_prefix(p)
                .is_some_and(|n| matches!(n.as_bytes(), [b'1'..=b'9']))
        });
    let name = match (stem.is_empty(), device) {
        (true, _) => hash,
        (false, true) => format!("p-{stem}-{hash}"),
        (false, false) => format!("{stem}-{hash}"),
    };
    synthesis_home
        .join("cache")
        .join("models")
        .join(format!("{name}.json"))
}

/// The cache for this endpoint. A missing, corrupt, unknown-version or other-endpoint file is
/// `None` (never an error).
pub fn read_cache(path: &Path, base_url: &str, protocol: Protocol) -> Option<CacheFile> {
    if std::fs::metadata(path).ok()?.len() > MAX_CACHE_BYTES {
        return None;
    }
    let mut f: CacheFile = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    if f.schema_version != SCHEMA_VERSION
        || f.base_url != base_url
        || f.protocol != protocol_name(protocol)
    {
        return None;
    }
    // a hand-edited or poisoned file gets the same checks as an API answer
    f.models.retain(|id, _| valid_id(id));
    for m in f.models.values_mut() {
        (m.context_window, m.max_output) = clamp(m.context_window, m.max_output);
    }
    Some(f)
}

pub fn is_fresh(f: &CacheFile, now: u64) -> bool {
    // a timestamp from the future is not trusted
    now.checked_sub(f.fetched_at)
        .is_some_and(|age| age < TTL_SECS)
}

/// One model's `raw_capabilities` above this is dropped (the useful facts are separate fields).
const MAX_RAW_CAPS_BYTES: usize = 4 << 10;
/// What is written stays under this, below [`MAX_CACHE_BYTES`], so a valid cache is never ignored
/// on read.
const MAX_CACHE_WRITE_BYTES: usize = 3 << 20;

pub fn write_cache(path: &Path, f: &CacheFile) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = f.clone();
    for m in f.models.values_mut() {
        let big = m
            .raw_capabilities
            .as_ref()
            .is_some_and(|r| serde_json::to_vec(r).map_or(0, |v| v.len()) > MAX_RAW_CAPS_BYTES);
        if big {
            m.raw_capabilities = None;
        }
    }
    let mut bytes = serde_json::to_vec_pretty(&f)?;
    // still too big (a huge list): keep the first models that fit, not a file nobody can read
    while bytes.len() > MAX_CACHE_WRITE_BYTES && !f.models.is_empty() {
        let keep = f.models.len() * MAX_CACHE_WRITE_BYTES / bytes.len() * 9 / 10;
        f.models = std::mem::take(&mut f.models)
            .into_iter()
            .take(keep)
            .collect();
        bytes = serde_json::to_vec_pretty(&f)?;
    }
    synthesis_config::write_private_atomic(path, &bytes)
}

fn supported(v: Option<&Value>) -> Option<bool> {
    v?.get("supported")?.as_bool()
}

/// Id and facts of one listed model. Anthropic reports `capabilities.image_input.supported` and
/// `capabilities.effort.<tier>.supported`.
pub fn facts_of(m: &ModelInfo) -> Option<(String, ModelFacts)> {
    let id = m.id.as_str().filter(|i| valid_id(i))?.to_string();
    let (context_window, max_output) = clamp(m.context_window, m.max_output);
    let effort = m.capabilities.get("effort");
    let effort_tiers = ["low", "medium", "high", "xhigh", "max"]
        .into_iter()
        .filter(|t| supported(effort.and_then(|e| e.get(*t))) == Some(true))
        .map(String::from)
        .collect();
    let facts = ModelFacts {
        context_window,
        max_output,
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

    #[test]
    fn a_written_cache_stays_readable_whatever_the_provider_sent() {
        let dir = std::env::temp_dir().join(format!("synt-mi-written-{}", std::process::id()));
        let path = dir.join("c.json");
        let mut f = file("http://x", 1);
        let big = |n: usize| json!({"blob": "x".repeat(n)}).as_object().cloned();
        f.models.get_mut("m").unwrap().raw_capabilities = big(10 << 10);
        f.models.insert(
            "small".into(),
            ModelFacts {
                raw_capabilities: big(100),
                ..Default::default()
            },
        );
        write_cache(&path, &f).unwrap();
        let r = read_cache(&path, "http://x", Protocol::Anthropic).unwrap();
        assert!(
            r.models["m"].raw_capabilities.is_none(),
            "over 4 KiB: dropped"
        );
        assert!(r.models["small"].raw_capabilities.is_some());
        // 40k models with 3 KiB each: over the limit as a whole, still written and readable
        for i in 0..40_000 {
            f.models.insert(
                format!("model-{i:05}"),
                ModelFacts {
                    raw_capabilities: big(3 << 10),
                    ..Default::default()
                },
            );
        }
        write_cache(&path, &f).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() <= MAX_CACHE_BYTES);
        let r = read_cache(&path, "http://x", Protocol::Anthropic).expect("not ignored");
        assert!(!r.models.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

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
        let dir = std::env::temp_dir().join(format!("synt-mi-trip-{}", std::process::id()));
        let p = cache_path(&dir, "we/ird:id");
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("we_ird_id-") && name.ends_with(".json"),
            "{name}"
        );
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

    #[test]
    fn ids_with_escapes_or_invisible_characters_are_dropped() {
        for bad in [
            "x\u{1b}]0;pwn\u{7}",
            "a\u{1b}[31mred",
            "a\u{9b}31m",
            "a\u{7f}",
            "a\u{202e}b",
            "a\u{200b}b",
            "a b",
            "",
        ] {
            assert!(!valid_id(bad), "{bad:?}");
            let m = crate::provider::normalize_model(&json!({"id": bad, "context_length": 5000}));
            assert!(facts_of(&m).is_none(), "{bad:?}");
        }
        assert!(valid_id("claude-opus-5/x:y_1.2"));
        // a poisoned cache is cleaned on read
        let dir = std::env::temp_dir().join(format!("synt-mi-poison-{}", std::process::id()));
        let p = dir.join("c.json");
        let mut f = file("https://a", unix_now());
        f.models
            .insert("evil\u{1b}[2J".into(), ModelFacts::default());
        write_cache(&p, &f).unwrap();
        let g = read_cache(&p, "https://a", Protocol::Anthropic).unwrap();
        assert_eq!(g.models.keys().collect::<Vec<_>>(), ["m"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn numbers_from_outside_are_clamped() {
        let w = |n| clamp(Some(n), None).0;
        assert_eq!(
            (
                w(0),
                w(1),
                w(1023),
                w(1024),
                w(100_000_000),
                w(100_000_001),
                w(u64::MAX)
            ),
            (None, None, None, Some(1024), Some(100_000_000), None, None)
        );
        assert_eq!(clamp(Some(8000), Some(15)).1, None, "below 16");
        assert_eq!(clamp(Some(8000), Some(16)).1, Some(16));
        assert_eq!(
            clamp(Some(8000), Some(9000)).1,
            Some(8000),
            "clamped to the window"
        );
        assert_eq!(clamp(None, Some(u64::MAX)).1, Some(100_000_000));
        // through the parser: fractional, negative, huge
        let f = |v: Value| {
            let m = crate::provider::normalize_model(&json!({"id":"m","context_length":v}));
            facts_of(&m).unwrap().1.context_window
        };
        assert_eq!(f(json!(2048.9)), Some(2048));
        assert_eq!(f(json!(-5)), None);
        assert_eq!(f(json!(1.0e30)), None);
        assert_eq!(f(json!("big")), None);
        // a poisoned cache cannot force constant compaction
        let dir = std::env::temp_dir().join(format!("synt-mi-clamp-{}", std::process::id()));
        let p = dir.join("c.json");
        let mut c = file("https://a", unix_now());
        c.models.get_mut("m").unwrap().context_window = Some(1);
        write_cache(&p, &c).unwrap();
        let g = read_cache(&p, "https://a", Protocol::Anthropic).unwrap();
        assert_eq!(g.models["m"].context_window, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_file_names_never_collide_or_name_a_device() {
        let home = Path::new("/h");
        let name = |id: &str| {
            cache_path(home, id)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        let ids = [
            "a/b", "a_b", "A", "a", "con", "CON", "nul", "com1", "lpt9", "..", "", "///",
        ];
        let names: Vec<_> = ids.iter().map(|i| name(i)).collect();
        let mut uniq = names.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), ids.len(), "{names:?}");
        for (id, n) in ids.iter().zip(&names) {
            let stem = n.split('.').next().unwrap();
            assert!(stem.contains('-') || stem.len() == 8, "{id}: {n}"); // never a bare device name
        }
        assert!(name("con").starts_with("p-con-"));
        assert!(name("com1").starts_with("p-com1-"));
        assert_eq!(name("").len(), "xxxxxxxx.json".len(), "hash only");
        assert!(name("a/b").starts_with("a_b-") && name("a_b").starts_with("a_b-"));
    }

    #[test]
    fn an_oversized_cache_file_is_ignored() {
        let dir = std::env::temp_dir().join(format!("synt-mi-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("c.json");
        let mut pad = serde_json::to_string(&file("https://a", unix_now())).unwrap();
        pad.truncate(pad.len() - 1);
        pad.push_str(&format!(",\"pad\":\"{}\"}}", "x".repeat(5 << 20)));
        std::fs::write(&p, pad).unwrap();
        assert!(read_cache(&p, "https://a", Protocol::Anthropic).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
