//! `--resume <id>` and the session list, over `synthesis-session` (read side only: the UI never
//! writes the store; the host reopens the writer for the resumed session).

use std::path::Path;

use synthesis_events::{Envelope, Record};
use synthesis_providers::{Content, Message, Role};
use synthesis_session::{Reader, SessionRecord};

/// A saved session ready to continue: its events fill the screen, its messages are the history
/// of the next task.
#[derive(Debug)]
pub struct Resume {
    pub record: SessionRecord,
    pub events: Vec<Envelope>,
    pub history: Vec<Message>,
}

/// `id` is a full id or a unique prefix. Unreadable events or messages are an error, not a
/// silently shorter conversation.
pub fn load_resume(synthesis_home: &Path, id: &str) -> Result<Resume, String> {
    if id.trim().is_empty() {
        return Err("empty session id".into()); // an empty prefix would match everything
    }
    let r = Reader::open(synthesis_home).map_err(|e| e.to_string())?;
    let all = r.list_sessions(None).map_err(|e| e.to_string())?;
    let matches: Vec<_> = all
        .iter()
        .filter(|s| s.id == id || s.id.starts_with(id))
        .collect();
    let record = match matches.as_slice() {
        [one] => (*one).clone(),
        [] => return Err(format!("no saved session matches \"{id}\"")),
        many => {
            let exact = many.iter().find(|s| s.id == id);
            match exact {
                Some(s) => (*s).clone(),
                None => {
                    return Err(format!(
                        "\"{id}\" matches {} sessions; use a longer id",
                        many.len()
                    ));
                }
            }
        }
    };
    let mut events = Vec::new();
    for rec in r
        .read_events(&record.id, 0, usize::MAX >> 1)
        .map_err(|e| e.to_string())?
    {
        // events of a newer schema are skipped on purpose (spec: consumers ignore unknown types)
        if let Record::Known(e) = rec {
            events.push(e);
        }
    }
    let mut history = Vec::new();
    for m in r.read_messages(&record.id).map_err(|e| e.to_string())? {
        if m.compacted {
            continue;
        }
        let role = match m.role.as_str() {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => continue,
        };
        let content: Content = serde_json::from_value(m.content)
            .map_err(|e| format!("message {} of {}: {e}", m.idx, record.id))?;
        history.push(Message { role, content });
    }
    Ok(Resume {
        record,
        events,
        history,
    })
}
