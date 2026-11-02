//! One place that turns an agent's model into what the user sees (port of `modelname.js`).
//! Shortcut: the `parseModel` fallback of `native/config` (provider lookup for a bare id) is not
//! ported; a bare id without a configured owner stays provider-less.

use std::collections::BTreeMap;

#[derive(Clone, Debug, Default)]
pub struct ModelConfig {
    pub aliases: BTreeMap<String, String>,
    /// provider id -> its model ids
    pub providers: Option<BTreeMap<String, Vec<String>>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelParts {
    pub provider: Option<String>,
    pub id: String,
    pub alias: Option<String>,
    pub text: String,
}

pub fn model_parts(
    model: &str,
    active_model: &str,
    config: Option<&ModelConfig>,
) -> Option<ModelParts> {
    let raw = if model.is_empty() {
        active_model
    } else {
        model
    };
    if raw.is_empty() {
        return None;
    }
    let mut alias = None;
    let mut r = raw.to_string();
    if !raw.contains('/')
        && let Some(a) = config
            .and_then(|c| c.aliases.get(raw))
            .filter(|a| !a.is_empty())
    {
        alias = Some(raw.to_string());
        r = a.clone();
    }
    let (mut provider, mut id) = match r.find('/').filter(|i| *i > 0) {
        Some(i) => (Some(r[..i].to_string()), r[i + 1..].to_string()),
        None => (None, r.clone()),
    };
    if let (Some(p), Some(provs)) = (provider.clone(), config.and_then(|c| c.providers.as_ref()))
        && !provs.contains_key(&p)
    {
        provider = provs
            .iter()
            .find(|(_, models)| models.contains(&r))
            .map(|(k, _)| k.clone());
        id = r.clone();
    }
    let base = provider
        .as_ref()
        .map_or(id.clone(), |p| format!("{p}/{id}"));
    let text = alias
        .as_ref()
        .map_or(base.clone(), |a| format!("{base} (alias {a})"));
    Some(ModelParts {
        provider,
        id,
        alias,
        text,
    })
}

pub fn model_label(
    model: &str,
    active_model: &str,
    config: Option<&ModelConfig>,
) -> Option<String> {
    model_parts(model, active_model, config).map(|p| p.text)
}
