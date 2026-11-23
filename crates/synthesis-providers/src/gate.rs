//! The secret gate: the only way to build the request a provider sends. Secrets must never reach
//! a model, so this is deterministic code that fails closed (no classifier, no "approve and send").
//! [`SealedRequest`] has a private constructor; `ModelProvider::stream` accepts nothing else.

use std::collections::BTreeMap;
use std::ops::Deref;

use serde_json::Value;
use synthesis_events::Redactor;

use crate::types::Request;

/// A request that passed [`SecretGate::seal`].
#[derive(Debug, Clone, PartialEq)]
pub struct SealedRequest {
    req: Request,
    redactions: BTreeMap<String, usize>,
    images_dropped: usize,
}

impl Deref for SealedRequest {
    type Target = Request;
    fn deref(&self) -> &Request {
        &self.req
    }
}

impl SealedRequest {
    /// `class -> count` of the values masked by this seal (never the values).
    pub fn redactions(&self) -> &BTreeMap<String, usize> {
        &self.redactions
    }

    /// Image blocks replaced by `[image]` because secrets are registered.
    pub fn images_dropped(&self) -> usize {
        self.images_dropped
    }

    /// The same sealed content for another model (failover); the content is not touched.
    pub(crate) fn with_model(&self, model: &str) -> Self {
        let mut s = self.clone();
        s.req.model = model.to_string();
        s
    }
}

/// Why nothing was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    /// Class of what survived redaction (`secret`, `github`, ...); `internal` when the gate itself failed.
    pub class: String,
}

impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "request not sent: a secret ({}) could not be removed from it",
            self.class
        )
    }
}

impl std::error::Error for Blocked {}

#[derive(Clone)]
pub struct SecretGate {
    redactor: Redactor,
}

impl Default for SecretGate {
    fn default() -> Self {
        Self::new(Redactor::new([]))
    }
}

impl SecretGate {
    /// `redactor` is the session's (clones share what it learns).
    pub fn new(redactor: Redactor) -> Self {
        Self { redactor }
    }

    /// Masks every string of the request (system, messages, tool results, tool schemas, tool
    /// input), drops images when any secret value is registered, then re-checks the result: a
    /// secret that survives, or any failure here, is `Err` and the request must not be sent.
    pub fn seal(&self, req: &Request) -> Result<SealedRequest, Blocked> {
        let internal = || Blocked {
            class: "internal".into(),
        };
        let mut req = req.clone();
        let images_dropped = if self.redactor.has_secrets() {
            req.messages
                .iter_mut()
                .map(|m| m.content.strip_images())
                .sum()
        } else {
            0
        };
        let mut v = serde_json::to_value(&req).map_err(|_| internal())?;
        let before = markers(&v);
        scrub(&self.redactor, &mut v);
        if let Some(class) = leak(&self.redactor, &v) {
            return Err(Blocked {
                class: class.into(),
            });
        }
        let mut redactions = markers(&v);
        for (k, n) in before {
            if let Some(m) = redactions.get_mut(&k) {
                *m = m.saturating_sub(n);
            }
        }
        redactions.retain(|_, n| *n > 0);
        let req = serde_json::from_value(v).map_err(|_| internal())?;
        Ok(SealedRequest {
            req,
            redactions,
            images_dropped,
        })
    }
}

type Visit<'a> = &'a mut dyn FnMut(&mut String);

/// Visits every string, object keys included (maps are rebuilt with the visited keys).
fn visit(v: &mut Value, f: Visit) {
    match v {
        Value::String(s) => f(s),
        Value::Array(a) => a.iter_mut().for_each(|x| visit(x, f)),
        Value::Object(o) => visit_map(o, f, &|_| false),
        _ => {}
    }
}

/// `exempt(key)`: that value is opaque base64 and is not visited (its key still is).
fn visit_map(o: &mut serde_json::Map<String, Value>, f: Visit, exempt: &dyn Fn(&str) -> bool) {
    for (mut k, mut v) in std::mem::take(o) {
        f(&mut k);
        if !exempt(&k) {
            visit(&mut v, f);
        }
        o.insert(k, v);
    }
}

/// One content block. The ONLY exemptions are the exact shapes the provider needs verbatim:
/// `source.data` of an image block, `signature` of a thinking block, `data` of a redacted_thinking
/// block. The same keys anywhere else (tool results, tool input) are ordinary text.
fn visit_block(b: &mut Value, f: Visit) {
    let Value::Object(o) = b else {
        return visit(b, f);
    };
    let kind = o
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    for (mut k, mut v) in std::mem::take(o) {
        let name = k.clone(); // the structural key itself is not data
        f(&mut k);
        match (kind.as_str(), name.as_str(), &mut v) {
            ("thinking", "signature", _) | ("redacted_thinking", "data", _) => {}
            ("image", "source", Value::Object(src))
                if src.get("type").and_then(Value::as_str) == Some("base64") =>
            {
                visit_map(src, f, &|key| key == "data")
            }
            _ => visit(&mut v, f),
        }
        o.insert(k, v);
    }
}

/// Walks the serialized `Request`: blocks at `messages[*].content[*]` get the exact exemptions
/// of [`visit_block`], everything else is visited whole.
fn visit_request(v: &mut Value, f: Visit) {
    let Value::Object(root) = v else {
        return visit(v, f);
    };
    for (mut k, mut val) in std::mem::take(root) {
        f(&mut k);
        match (k.as_str(), &mut val) {
            ("messages", Value::Array(msgs)) => {
                for m in msgs {
                    let Value::Object(mo) = m else {
                        visit(m, f);
                        continue;
                    };
                    for (mut mk, mut mv) in std::mem::take(mo) {
                        f(&mut mk);
                        match (mk.as_str(), &mut mv) {
                            ("content", Value::Array(bs)) => {
                                bs.iter_mut().for_each(|b| visit_block(b, f))
                            }
                            _ => visit(&mut mv, f),
                        }
                        mo.insert(mk, mv);
                    }
                }
            }
            _ => visit(&mut val, f),
        }
        root.insert(k, val);
    }
}

fn scrub(r: &Redactor, v: &mut Value) {
    visit_request(v, &mut |s| *s = r.redact_str(s));
}

fn leak(r: &Redactor, v: &Value) -> Option<&'static str> {
    let mut found = None;
    visit_request(&mut v.clone(), &mut |s| {
        found = found.or_else(|| r.leak_class(s));
    });
    found
}

/// `[REDACTED:<class>]` markers by class in all strings.
fn markers(v: &Value) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    visit_request(&mut v.clone(), &mut |s| {
        for part in s.split("[REDACTED:").skip(1) {
            if let Some((c, _)) = part.split_once(']') {
                *m.entry(c.to_string()).or_default() += 1;
            }
        }
    });
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Block, Content, ImageSource, Message, Role, System, ToolDef};
    use serde_json::json;

    fn req_with(text: String) -> Request {
        Request {
            model: "m".into(),
            max_tokens: 10,
            system: Some(System::Text(format!("sys {text}"))),
            messages: vec![
                Message {
                    role: Role::User,
                    content: Content::Text(text.clone()),
                },
                Message {
                    role: Role::User,
                    content: Content::Blocks(vec![
                        Block::ToolResult {
                            tool_use_id: "t1".into(),
                            content: Some(json!([{"type": "text", "text": text}])),
                            is_error: None,
                        },
                        Block::Image {
                            source: ImageSource::Base64 {
                                media_type: "image/png".into(),
                                data: "AAAA".into(),
                            },
                        },
                    ]),
                },
            ],
            tools: vec![ToolDef {
                name: "mcp".into(),
                description: Some(text),
                input_schema: json!({"type": "object"}),
            }],
            temperature: None,
            thinking: None,
            reasoning_effort: None,
        }
    }

    // fake values are assembled at runtime so no secret-looking literal is committed
    fn fakes() -> Vec<(&'static str, String)> {
        let a = "A".repeat(20);
        vec![
            ("github", format!("ghp_{a}{a}")),
            ("aws", format!("AK{}{}", "IA", "B".repeat(16))),
            ("sk", format!("{}-{}", "sk", "c".repeat(30))),
            ("slack", format!("{}-{}", "xoxb", "1234567890-abcdefgh")),
            (
                "url-userinfo",
                format!("postgres://bob:{}@db.local/x", "p4ssw0rd9"),
            ),
            (
                "jwt",
                format!(
                    "eyJ{}.eyJ{}.{}",
                    "h".repeat(8),
                    "p".repeat(8),
                    "s".repeat(8)
                ),
            ),
            (
                "private-key",
                format!(
                    "-----BEGIN {} KEY-----\n{}\n-----END {} KEY-----",
                    "RSA PRIVATE",
                    "Zm9v".repeat(10),
                    "RSA PRIVATE"
                ),
            ),
            ("credential", format!("DB_PASSWORD={}", "hunter2hunter2")),
            (
                "credential",
                format!("\"api_key\": \"{}\"", "k3y-value-123456"),
            ),
            ("credential", format!("private_key: {}", "q9w8e7r6t5y4")),
        ]
    }

    #[test]
    fn every_class_is_masked_everywhere_in_the_request() {
        let g = SecretGate::default();
        for (class, v) in fakes() {
            let sealed = g.seal(&req_with(format!("x {v} y"))).expect(class);
            let out = serde_json::to_string(&*sealed).unwrap();
            let needle = v
                .split(['=', ':'])
                .next_back()
                .unwrap()
                .trim()
                .trim_matches('"');
            assert!(
                !out.contains(needle.split('\n').next().unwrap()),
                "{class} leaked"
            );
            assert!(out.contains("[REDACTED:"), "{class}");
            assert!(
                sealed.redactions().values().sum::<usize>() >= 4,
                "{class}: counts"
            );
        }
    }

    #[test]
    fn registered_value_is_masked_and_images_are_dropped() {
        let secret = format!("{}-{}", "hunter", "42424242");
        let r = Redactor::new([]);
        let g = SecretGate::new(r.clone());
        let plain = g.seal(&req_with("see".into())).unwrap();
        assert_eq!(plain.images_dropped(), 0);
        assert!(plain.redactions().is_empty());
        assert_eq!(*plain, req_with("see".into()), "clean request is unchanged");
        r.add_secret(&secret); // learned later, through a clone
        let sealed = g.seal(&req_with(format!("a {secret} b"))).unwrap();
        assert!(!serde_json::to_string(&*sealed).unwrap().contains(&secret));
        assert_eq!(sealed.images_dropped(), 1);
        assert_eq!(sealed.redactions().get("secret"), Some(&4));
    }

    #[test]
    fn lookalike_image_and_signature_shapes_in_tool_data_are_still_masked() {
        let tok = format!("ghp_{}", "Q".repeat(36));
        let mut r = req_with("x".into());
        r.messages.push(Message {
            role: Role::User,
            content: Content::Blocks(vec![
                Block::ToolResult {
                    tool_use_id: "t2".into(),
                    content: Some(json!({"type": "image", "note": tok, "signature": tok,
                        "source": {"type": "base64", "data": tok}})),
                    is_error: None,
                },
                Block::ToolUse {
                    id: "u".into(),
                    name: "n".into(),
                    input: Some(json!({"signature": tok, tok.clone(): 1})),
                },
            ]),
        });
        let sealed = SecretGate::default().seal(&r).unwrap();
        let out = serde_json::to_string(&*sealed).unwrap();
        assert!(!out.contains(&tok), "{out}");
        assert!(
            sealed.redactions().get("github") >= Some(&5),
            "{:?}",
            sealed.redactions()
        );
    }

    #[test]
    fn real_image_data_and_thinking_signatures_stay_verbatim() {
        let data = format!("AKIA{}", "B".repeat(16)); // base64 that happens to look like a key
        let mut r = req_with("x".into());
        r.messages.push(Message {
            role: Role::Assistant,
            content: Content::Blocks(vec![
                Block::Thinking {
                    thinking: "t".into(),
                    signature: Some(data.clone()),
                },
                Block::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: data.clone(),
                    },
                },
            ]),
        });
        let sealed = SecretGate::default().seal(&r).unwrap();
        let out = serde_json::to_string(&*sealed).unwrap();
        assert_eq!(out.matches(&data).count(), 2, "{out}");
    }

    #[test]
    fn a_surviving_secret_blocks_the_send() {
        // masking the first value forms the second (registered earlier, so already passed over)
        let r = Redactor::new([]);
        let g = SecretGate::new(r.clone());
        r.add_secret("[REDACTED:secret]");
        r.add_secret("12345678");
        let e = g.seal(&req_with("12345678".into())).unwrap_err();
        assert_eq!(e.class, "secret");
    }
}
