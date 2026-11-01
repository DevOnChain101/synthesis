//! `--resume <id>` and the session list, over `synthesis-session` (read side only: the UI never
//! writes the store; the host reopens the writer for the resumed session).

use std::path::Path;

use synthesis_events::{Envelope, Record};
use synthesis_providers::{Content, Message, Role};
use synthesis_session::{Reader, SessionRecord};

use crate::SavedSession;

/// A saved session ready to continue: its events fill the screen, its messages are the history
/// of the next task.
#[derive(Debug)]
pub struct Resume {
    pub record: SessionRecord,
    pub events: Vec<Envelope>,
    pub history: Vec<Message>,
}

fn saved(r: &SessionRecord) -> SavedSession {
    let ms = synthesis_tui_render::model::parse_ts(&r.updated_at).unwrap_or(0);
    SavedSession {
        id: r.id.clone(),
        cwd: r.cwd.clone(),
        title: r.title.clone().unwrap_or_default(),
        updated_at: r.updated_at.clone(),
        mtime: ms,
    }
}

/// Newest first; `cwd` limits it to one folder.
pub fn list_saved(synthesis_home: &Path, cwd: Option<&str>) -> Result<Vec<SavedSession>, String> {
    let r = Reader::open(synthesis_home).map_err(|e| e.to_string())?;
    Ok(r.list_sessions(cwd)
        .map_err(|e| e.to_string())?
        .iter()
        .enumerate()
        .map(|(i, rec)| {
            let mut s = saved(rec);
            // sessions have no stored title: the first prompt names them (newest 100 only)
            if s.title.trim().is_empty() && i < 100 {
                s.title = first_prompt(&r, &rec.id).unwrap_or_default();
            }
            s
        })
        .collect())
}

/// First thing the person typed in a session, one line.
fn first_prompt(r: &Reader, id: &str) -> Option<String> {
    for rec in r.read_events(id, 0, 40).ok()? {
        if let Record::Known(e) = rec
            && let synthesis_events::Event::UserMessage {
                text, display_text, ..
            } = e.event
        {
            let t = display_text.filter(|d| !d.is_empty()).unwrap_or(text);
            if !t.trim().is_empty() && !t.starts_with("[Synthesis") {
                return Some(t.split_whitespace().collect::<Vec<_>>().join(" "));
            }
        }
    }
    None
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
