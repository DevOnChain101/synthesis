//! Saved sessions of the store, for the sidebar and the resume picker (read side only).

use std::path::Path;

use synthesis_events::Record;
use synthesis_session::{Reader, SessionRecord};

/// A session in the store, for the sidebar and the resume picker.
#[derive(Clone, Debug, Default)]
pub struct SavedSession {
    pub id: String,
    pub cwd: String,
    pub title: String,
    pub updated_at: String,
    /// Epoch ms of `updated_at` (0 when unknown).
    pub mtime: i64,
}

fn saved(r: &SessionRecord) -> SavedSession {
    let ms = crate::model::parse_ts(&r.updated_at).unwrap_or(0);
    SavedSession {
        id: r.id.clone(),
        cwd: r.cwd.clone(),
        title: r.title.clone().unwrap_or_default(),
        updated_at: r.updated_at.clone(),
        mtime: ms,
    }
}

/// Newest first; `cwd` limits it to one folder. Sessions without a message or task are left out.
pub fn list_saved(synthesis_home: &Path, cwd: Option<&str>) -> Result<Vec<SavedSession>, String> {
    let r = Reader::open(synthesis_home).map_err(|e| e.to_string())?;
    Ok(r.list_used_sessions(cwd)
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
pub(crate) fn first_prompt(r: &Reader, id: &str) -> Option<String> {
    for rec in r.read_events(id, 0, 40).ok()? {
        if let Record::Known(e) = rec
            && let synthesis_events::Event::UserMessage {
                text, display_text, ..
            } = e.event
        {
            if crate::text::is_harness_note(display_text.as_deref()) {
                continue;
            }
            let t = display_text.unwrap_or(text);
            if !t.trim().is_empty() && !t.starts_with("[Synthesis") {
                return Some(t.split_whitespace().collect::<Vec<_>>().join(" "));
            }
        }
    }
    None
}
