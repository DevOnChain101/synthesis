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
    images_kept: usize,
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

    /// Image blocks sent although secrets are registered (`synt.browser.screenshots` is `allow`): pixels are not
    /// masked, so they may show on-screen secrets.
    pub fn images_kept_with_secrets(&self) -> usize {
        self.images_kept
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
    /// The person chose to send images even when secrets are registered (explicit exception, see docs/SECURITY.md).
    allow_images: bool,
}

impl Default for SecretGate {
    fn default() -> Self {
        Self::new(Redactor::new([]))
    }
}

impl SecretGate {
    /// `redactor` is the session's (clones share what it learns).
    pub fn new(redactor: Redactor) -> Self {
        Self {
            redactor,
            allow_images: false,
        }
    }

    /// Keep image blocks while secrets are registered (`synt.browser.screenshots` = `allow`). Text is masked as always.
    pub fn allow_images(mut self, allow: bool) -> Self {
        self.allow_images = allow;
        self
    }

    /// Masks every content string of the request (system, messages, tool results, tool schemas,
    /// tool input), drops images when any secret value is registered, then re-checks the result: a
    /// secret that survives, or any failure here, is `Err` and the request must not be sent.
    /// The structure of the request (roles, block types, ids, tool names, model) is never rewritten.
    pub fn seal(&self, req: &Request) -> Result<SealedRequest, Blocked> {
        let internal = || Blocked {
            class: "internal".into(),
        };
        let r = &self.redactor;
        // values that could not be registered, or an automaton that did not build: not safe to send
        if let Some(class @ ("capacity" | "internal")) = r.leak_class("") {
            return Err(Blocked {
                class: class.into(),
            });
        }
        // cloned only to strip images (which happens only when a value is registered)
        let stripped;
        let images_kept = if r.has_secrets() && self.allow_images {
            // shortcut: counts on a throwaway clone, only when allow and secrets meet; a counting walk if it shows up in profiles
            let mut c = req.clone();
            c.messages
                .iter_mut()
                .map(|m| m.content.strip_images())
                .sum()
        } else {
            0
        };
        let (src, images_dropped) = if r.has_secrets() && !self.allow_images {
            let mut c = req.clone();
            let n = c
                .messages
                .iter_mut()
                .map(|m| m.content.strip_images())
                .sum();
            stripped = c;
            (&stripped, n)
        } else {
            (req, 0)
        };
        let mut v = serde_json::to_value(src).map_err(|_| internal())?;
        let before = markers(&mut v);
        // adjacent text blocks are masked as one text: a value split over them is still found
        merge_text_runs(r, &mut v);
        visit_request(&mut v, &mut |s| *s = r.redact_str(s));
        let mut found = None;
        visit_request(&mut v, &mut |s| found = found.or_else(|| r.leak_class(s)));
        // the survivor check also reads each message's text as one string (across a tool_use block
        // or between tool results): a value split over them must block the send
        // names and ids are never rewritten, but they are checked: a secret there blocks the send
        found = found.or_else(|| structural_strings(&v).iter().find_map(|t| r.leak_class(t)));
        found = found.or_else(|| message_texts(&v).iter().find_map(|t| r.leak_class(t)));
        if let Some(class) = found {
            return Err(Blocked {
                class: class.into(),
            });
        }
        let mut redactions = markers(&mut v);
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
            images_kept,
        })
    }
}

type Visit<'a> = &'a mut dyn FnMut(&mut String);

const B64: fn(&u8) -> bool =
    |b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'\n');

/// The exact shape of an image a tool returned: `{type:image, source:{type:base64, media_type,
/// data}}` with base64 data and nothing else. Its data is pixels, not text (any other shape,
/// lookalikes included, is ordinary data).
fn is_plain_image(o: &serde_json::Map<String, Value>) -> bool {
    let keys = |m: &serde_json::Map<String, Value>, k: &[&str]| {
        m.len() == k.len() && k.iter().all(|k| m.contains_key(*k))
    };
    let Some(Value::Object(src)) = o.get("source") else {
        return false;
    };
    keys(o, &["type", "source"])
        && o["type"] == "image"
        && keys(src, &["type", "media_type", "data"])
        && src["type"] == "base64"
        && src["media_type"]
            .as_str()
            .is_some_and(|m| m.starts_with("image/"))
        && src["data"]
            .as_str()
            .is_some_and(|d| d.bytes().all(|b| B64(&b)))
}

/// Free-form data (tool input, tool results, schemas): every string, object keys included (maps
/// are rebuilt with the visited keys), except the data of a plain image.
fn visit(v: &mut Value, f: Visit) {
    match v {
        Value::String(s) => f(s),
        Value::Array(a) => a.iter_mut().for_each(|x| visit(x, f)),
        Value::Object(o) if is_plain_image(o) => {}
        Value::Object(o) => {
            for (mut k, mut val) in std::mem::take(o) {
                f(&mut k);
                visit(&mut val, f);
                o.insert(k, val);
            }
        }
        _ => {}
    }
}

/// Values of these keys name the request's own structure or are provider-issued: never rewritten.
fn structural(key: &str) -> bool {
    matches!(
        key,
        "type"
            | "role"
            | "id"
            | "name"
            | "model"
            | "tool_use_id"
            | "media_type"
            | "is_error"
            | "signature"
            | "data"
    )
}

/// An object whose KEYS are the request's structure: only the values of content keys are visited.
fn visit_struct(o: &mut serde_json::Map<String, Value>, f: Visit) {
    let kind = o
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    for (k, val) in o.iter_mut() {
        match (kind.as_str(), k.as_str()) {
            // `thinking` of a thinking block is text; the request's own `thinking` is a config
            ("thinking", "signature") | ("redacted_thinking", "data") => {}
            (_, k) if structural(k) => {}
            ("image", "source") => {
                if let Value::Object(src) = val {
                    visit_struct(src, f);
                }
            }
            _ => visit(val, f),
        }
    }
}

/// Walks the serialized `Request`: its envelope, messages and blocks keep their keys and tags,
/// only content is visited.
fn visit_request(v: &mut Value, f: Visit) {
    let Value::Object(root) = v else {
        return visit(v, f);
    };
    for (k, val) in root.iter_mut() {
        match (k.as_str(), val) {
            ("model" | "thinking" | "reasoning_effort", _) => {}
            ("messages", Value::Array(msgs)) => {
                for m in msgs {
                    let Value::Object(mo) = m else {
                        visit(m, f);
                        continue;
                    };
                    for (mk, mv) in mo.iter_mut() {
                        match (mk.as_str(), mv) {
                            ("role", _) => {}
                            ("content", Value::Array(bs)) => bs.iter_mut().for_each(|b| match b {
                                Value::Object(bo) => visit_block(bo, f),
                                other => visit(other, f),
                            }),
                            (_, mv) => visit(mv, f),
                        }
                    }
                }
            }
            ("system", Value::Array(bs)) => bs.iter_mut().for_each(|b| match b {
                Value::Object(bo) => visit_struct(bo, f),
                other => visit(other, f),
            }),
            ("tools", Value::Array(ts)) => ts.iter_mut().for_each(|t| match t {
                Value::Object(to) => visit_struct(to, f),
                other => visit(other, f),
            }),
            (_, val) => visit(val, f),
        }
    }
}

/// One content block. Its keys are fixed; the free-form parts (`input` of a tool call, `content` of
/// a tool result) are data like any other, keys included.
fn visit_block(b: &mut serde_json::Map<String, Value>, f: Visit) {
    visit_struct(b, f)
}

/// Adjacent plain text blocks (of a message or of a tool result's content) become one block when
/// masking the joined text differs from masking each block: a value split over them is masked
/// instead of surviving, and a word that merely ends one block and starts the next is not hit.
fn merge_text_runs(r: &Redactor, v: &mut Value) {
    fn text_of(b: &Value) -> Option<&str> {
        let o = b.as_object()?;
        (o.len() == 2 && o.get("type").is_some_and(|t| t == "text"))
            .then(|| o.get("text")?.as_str())
            .flatten()
    }
    fn blocks(r: &Redactor, bs: &mut Vec<Value>) {
        let mut i = 0;
        while i < bs.len() {
            let mut j = i;
            while j < bs.len() && text_of(&bs[j]).is_some() {
                j += 1;
            }
            if j - i > 1 {
                let parts: Vec<&str> = bs[i..j].iter().filter_map(text_of).collect();
                let whole = r.redact_str(&parts.concat());
                if whole != parts.iter().map(|p| r.redact_str(p)).collect::<String>() {
                    bs[i]["text"] = Value::String(whole);
                    bs.drain(i + 1..j);
                    i += 1;
                } else {
                    i = j;
                }
                continue;
            }
            if let Some(inner) = bs[i].get_mut("content").and_then(Value::as_array_mut) {
                blocks(r, inner);
            }
            i = j.max(i + 1);
        }
    }
    for m in v["messages"].as_array_mut().into_iter().flatten() {
        if let Some(bs) = m.get_mut("content").and_then(Value::as_array_mut) {
            blocks(r, bs);
        }
    }
}

/// `model`, tool names and block ids/names: the strings the gate leaves alone.
fn structural_strings(v: &Value) -> Vec<&str> {
    let mut out: Vec<&str> = v["model"].as_str().into_iter().collect();
    let keys = ["id", "name", "tool_use_id"];
    for t in v["tools"].as_array().into_iter().flatten() {
        out.extend(t["name"].as_str());
    }
    for m in v["messages"].as_array().into_iter().flatten() {
        for b in m["content"].as_array().into_iter().flatten() {
            out.extend(keys.iter().filter_map(|k| b[*k].as_str()));
        }
    }
    out
}

/// Per message: all its text (text blocks and the text parts of tool results) in order, joined.
/// Only messages with more than one piece are returned (a single piece was checked already).
fn message_texts(v: &Value) -> Vec<String> {
    fn collect(b: &Value, out: &mut Vec<String>) {
        if let Some(t) = b.get("text").and_then(Value::as_str) {
            out.push(t.to_string());
        }
        match b.get("content") {
            Some(Value::String(s)) => out.push(s.clone()),
            Some(Value::Array(a)) => a.iter().for_each(|x| collect(x, out)),
            _ => {}
        }
    }
    v["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let mut parts = vec![];
            m["content"]
                .as_array()?
                .iter()
                .for_each(|b| collect(b, &mut parts));
            (parts.len() > 1).then(|| parts.concat())
        })
        .collect()
}

/// `[REDACTED:<class>]` markers by class in all strings.
fn markers(v: &mut Value) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    visit_request(v, &mut |s| {
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
    fn allow_images_keeps_the_image_but_text_is_still_masked() {
        let secret = format!("{}-{}", "hunter", "42424242");
        let r = Redactor::new([]);
        r.add_secret(&secret);
        let sealed = SecretGate::new(r)
            .allow_images(true)
            .seal(&req_with(format!("a {secret} b")))
            .unwrap();
        let out = serde_json::to_string(&*sealed).unwrap();
        assert!(!out.contains(&secret) && out.contains("AAAA") && !out.contains("[image]"));
        assert_eq!(
            (sealed.images_dropped(), sealed.images_kept_with_secrets()),
            (0, 1)
        );
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

    fn user_blocks(bs: Vec<Block>) -> Request {
        let mut req = req_with("x".into());
        req.messages.push(Message {
            role: Role::User,
            content: Content::Blocks(bs),
        });
        req
    }

    fn text(t: &str) -> Block {
        Block::Text { text: t.into() }
    }

    #[test]
    fn a_value_split_across_adjacent_blocks_is_masked_as_one_text() {
        let secret = format!("{}-{}", "hunter", "42424242");
        let (a, b) = secret.split_at(7);
        let g = SecretGate::new(Redactor::new([secret.clone()]));
        let sealed = g
            .seal(&user_blocks(vec![
                text(&format!("one {a}")),
                text(&format!("{b} two")),
            ]))
            .unwrap();
        let out = serde_json::to_string(&*sealed).unwrap();
        assert!(out.contains("one [REDACTED:secret] two"), "{out}");
        // inside one tool result
        let tr = Block::ToolResult {
            tool_use_id: "t".into(),
            content: Some(json!([{"type": "text", "text": a}, {"type": "text", "text": b}])),
            is_error: None,
        };
        let out = serde_json::to_string(&*g.seal(&user_blocks(vec![tr])).unwrap()).unwrap();
        assert!(
            !out.contains(a) && out.contains("[REDACTED:secret]"),
            "{out}"
        );
        // a split pattern is masked whole, not only its head
        let tok = format!("ghp_{}", "Q".repeat(36));
        let (a, b) = tok.split_at(10);
        let sealed = SecretGate::default()
            .seal(&user_blocks(vec![text(a), text(b)]))
            .unwrap();
        let out = serde_json::to_string(&*sealed).unwrap();
        assert!(!out.contains("QQQQ"), "{out}");
    }

    #[test]
    fn a_value_split_over_a_gap_blocks_the_send() {
        let secret = format!("{}-{}", "hunter", "42424242");
        let (a, b) = secret.split_at(7);
        let g = SecretGate::new(Redactor::new([secret.clone()]));
        let call = Block::ToolUse {
            id: "u".into(),
            name: "n".into(),
            input: None,
        };
        let e = g
            .seal(&user_blocks(vec![text(a), call, text(b)]))
            .unwrap_err();
        assert_eq!(e.class, "secret");
        let tr = |t: &str| Block::ToolResult {
            tool_use_id: "t".into(),
            content: Some(json!([{"type": "text", "text": t}])),
            is_error: None,
        };
        let e = g.seal(&user_blocks(vec![tr(a), tr(b)])).unwrap_err();
        assert_eq!(e.class, "secret");
        assert!(g.seal(&user_blocks(vec![text("one"), text("two")])).is_ok());
    }

    #[test]
    fn a_short_value_ending_one_block_and_starting_a_word_in_the_next_does_not_block() {
        let r = Redactor::new([]);
        r.add_secret_min("pass", 4).unwrap();
        let g = SecretGate::new(r);
        let sealed = g
            .seal(&user_blocks(vec![text("my pass"), text("word is long")]))
            .unwrap();
        let out = serde_json::to_string(&*sealed).unwrap();
        assert!(out.contains("my password is long"), "{out}");
    }

    #[test]
    fn a_value_inside_another_value_is_masked_whole_in_a_request() {
        let (a, b) = ("abcd-1234".to_string(), "abcd-1234-tail-9876".to_string());
        let g = SecretGate::new(Redactor::new([a.clone(), b.clone()]));
        let out =
            serde_json::to_string(&*g.seal(&req_with(format!("x {b} y {a}"))).unwrap()).unwrap();
        assert!(!out.contains("tail") && !out.contains("abcd"), "{out}");
    }

    #[test]
    fn a_registered_structural_word_cannot_rewrite_the_request() {
        // refused at registration ...
        let r = Redactor::new([]);
        for w in ["user", "text", "tool_use", "assistant", "type"] {
            assert!(r.add_secret_min(w, 4).is_err(), "{w}");
        }
        // ... and names are never rewritten: a registered value that is a tool name blocks the
        // send (checked, not scrubbed) instead of corrupting the request
        let r = Redactor::new(["Probe".to_string()]);
        let mut req = req_with("x".into());
        req.tools[0].name = "Probe".into();
        assert_eq!(SecretGate::new(r).seal(&req).unwrap_err().class, "secret");
    }

    #[test]
    fn a_full_redactor_blocks_every_send() {
        let r = Redactor::new([]);
        let g = SecretGate::new(r.clone());
        assert!(g.seal(&req_with("x".into())).is_ok());
        let mut i = 0;
        while !r.add_secret(&format!("filler-value-{i:07}")) {
            i += 1;
        }
        assert_eq!(g.seal(&req_with("x".into())).unwrap_err().class, "capacity");
    }

    #[test]
    fn nested_plain_images_are_not_scanned_but_lookalikes_are() {
        let mut r = req_with("x".into());
        let data = format!("AKIA{}", "B".repeat(16)); // base64 that happens to look like a key
        r.messages.push(Message {
            role: Role::User,
            content: Content::Blocks(vec![Block::ToolResult {
                tool_use_id: "t".into(),
                content: Some(json!([{"type": "image", "source":
                    {"type": "base64", "media_type": "image/png", "data": data}}])),
                is_error: None,
            }]),
        });
        let out = serde_json::to_string(&*SecretGate::default().seal(&r).unwrap()).unwrap();
        assert!(out.contains(&data), "{out}");
    }

    #[test]
    fn many_values_and_a_large_history_stay_fast() {
        let vals: Vec<String> = (0..1000).map(|i| format!("secret-value-{i:06}")).collect();
        let g = SecretGate::new(Redactor::new(vals.clone()));
        let big = "lorem ipsum dolor sit amet ".repeat(40_000); // ~1 MB
        let mut req = req_with(big);
        req.messages.push(Message {
            role: Role::User,
            content: Content::Text(format!("and {}", vals[999])),
        });
        let t = std::time::Instant::now();
        let sealed = g.seal(&req).unwrap();
        assert!(
            !serde_json::to_string(&*sealed)
                .unwrap()
                .contains(&vals[999])
        );
        // one automaton pass per string: generous bound, a per-value scan took minutes
        assert!(
            t.elapsed() < std::time::Duration::from_secs(20),
            "{:?}",
            t.elapsed()
        );
    }

    #[test]
    fn an_encoded_registered_value_never_survives() {
        let secret = format!("{}-{}", "hunter", "42424242");
        let enc: String = secret.bytes().map(|b| format!("{b:02x}")).collect();
        let r = Redactor::new([secret]);
        let sealed = SecretGate::new(r)
            .seal(&req_with(format!("hex {enc}")))
            .unwrap();
        assert!(!serde_json::to_string(&*sealed).unwrap().contains(&enc));
    }

    #[test]
    fn a_marker_lookalike_value_cannot_block_every_request() {
        let r = Redactor::new([]);
        let g = SecretGate::new(r.clone());
        assert!(r.add_secret_min("[REDACTED:secret]", 4).is_err());
        assert!(r.add_secret_min("REDACTED", 4).is_err());
        let ghp = format!("ghp_{}", "Q".repeat(36));
        assert!(g.seal(&req_with(ghp)).is_ok());
    }
}
