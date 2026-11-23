//! The worker inbox (docs/specs/super-agent-s1.md 4.6): the person's line to an outcome worker.
//!
//! No socket and no DDL: the owner writes chained events in the control session
//! (`attempt.message` for a running attempt, `attempt.message.queued` otherwise), the worker polls
//! them through a [`Reader`] and acks with `owner_message.delivered` in its OWN session. A row is
//! believed only if its chain link and its sha256 hold, so a row written without the per-user key
//! (a forged INSERT) is dropped, never delivered and never laundered into a queued one.

use super::*;
use crate::store::event_link_ok;

/// Longest message, in bytes.
pub const MESSAGE_MAX_BYTES: usize = 4096;
/// At most this many per attempt in [`PER_ATTEMPT_SECS`], per outcome in [`PER_OUTCOME_SECS`], and
/// waiting for the next attempt of one work item.
const PER_ATTEMPT: i64 = 5;
const PER_ATTEMPT_SECS: i64 = 600;
const PER_OUTCOME: i64 = 20;
const PER_OUTCOME_SECS: i64 = 3600;
const QUEUED_PER_ITEM: usize = 10;
/// Rows looked at per read (a flood of forged rows cannot make a poll unbounded).
const SCAN: i64 = 100;

/// The union of the two hidden-character lists, plus the object-replacement and musical-format
/// characters neither names.
fn hidden(c: char) -> bool {
    synthesis_core::outcome::hidden_char(c)
        || synthesis_config::is_invisible_or_blank(c)
        || matches!(c, '\u{fffc}' | '\u{1d173}'..='\u{1d17a}')
}

/// Is `text` a message a person may send? Hidden, bidi and control characters (a newline and a tab
/// are allowed) are refused, never stripped: what the person typed must be what the worker reads.
pub fn check_message_text(text: &str) -> std::result::Result<(), &'static str> {
    if text.trim().is_empty() {
        Err("the message is empty")
    } else if text.len() > MESSAGE_MAX_BYTES {
        Err("the message is longer than 4096 bytes")
    } else if text.chars().any(|c| !matches!(c, '\n' | '\t') && hidden(c)) {
        Err("the message holds hidden, bidi or control characters")
    } else {
        Ok(())
    }
}

/// A message to send.
pub struct NewMessage<'a> {
    pub item: &'a WorkItemId,
    pub text: &'a str,
    /// `user:<surface>`, from the command's principal (never from its params).
    pub from: &'a str,
    /// `person` (typed in the worker's frame) or `card` (a conductor proposal the person clicked).
    /// Client-asserted, like `from`: a label for display only, and no decision may depend on it.
    pub via: &'a str,
    pub proposal_sha256: Option<&'a str>,
}

/// What became of a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sent {
    /// An attempt runs: its worker reads the event.
    Live {
        attempt: AttemptId,
        seq: u64,
        sha256: String,
    },
    /// No attempt runs: the next attempt's first prompt carries it.
    Queued { seq: u64, sha256: String },
}

impl Sent {
    /// `(seq, sha256)` of the stored (secret-masked) text.
    pub fn id(&self) -> (u64, &str) {
        match self {
            Sent::Live { seq, sha256, .. } | Sent::Queued { seq, sha256 } => (*seq, sha256),
        }
    }
}

/// A queued message, verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedMessage {
    pub seq: u64,
    pub text: String,
    pub sha256: String,
    pub from: String,
}

/// A message for one worker, verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxMessage {
    pub seq: u64,
    pub text: String,
    pub sha256: String,
}

/// What a poll found: the verified messages above the cursor and the highest row looked at.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Inbox {
    pub messages: Vec<InboxMessage>,
    pub scanned: u64,
}

/// An event row that passed [`believed`] and the chain.
struct Row {
    seq: i64,
    payload: Value,
    text: String,
    sha256: String,
    from: String,
}

/// `(text, sha256, from)` of a payload whose own `seq` is the row's and whose hash is its text's.
fn believed(payload: &Value, seq: i64) -> Option<(String, String, String)> {
    let text = payload.get("text")?.as_str()?;
    let sha = payload.get("sha256")?.as_str()?;
    (payload.get("seq")?.as_i64()? == seq && sha_hex(text.as_bytes()) == sha).then(|| {
        (
            text.to_string(),
            sha.to_string(),
            payload
                .get("from")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )
    })
}

/// The rows `(seq, payload)` of `sql` that pass [`believed`] and the chain.
fn verified(
    conn: &Connection,
    key: &[u8; 32],
    control: &str,
    sql: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<Row>> {
    let mut st = conn.prepare(sql)?;
    let rows = st
        .query_map(args, |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = vec![];
    for (seq, body) in rows {
        let Ok(payload) = serde_json::from_str::<Value>(&body) else {
            continue;
        };
        if let Some((text, sha256, from)) = believed(&payload, seq)
            && event_link_ok(conn, key, control, seq)?
        {
            out.push(Row {
                seq,
                payload,
                text,
                sha256,
                from,
            });
        }
    }
    Ok(out)
}

/// The queued messages of `item` no attempt has taken yet, in seq order.
fn pending_queued(
    conn: &Connection,
    key: &[u8; 32],
    control: &str,
    item: &WorkItemId,
) -> Result<Vec<QueuedMessage>> {
    Ok(verified(
        conn,
        key,
        control,
        "SELECT q.seq, q.payload FROM events q WHERE q.session_id=?1 AND q.type=?2 \
         AND json_extract(q.payload,'$.work_item')=?3 \
         AND NOT EXISTS (SELECT 1 FROM events c, json_each(c.payload,'$.seqs') j \
            WHERE c.session_id=?1 AND c.type=?4 AND j.value=q.seq) \
         ORDER BY q.seq LIMIT ?5",
        &[
            &control,
            &ev::ATTEMPT_MESSAGE_QUEUED,
            &item.as_str(),
            &ev::ATTEMPT_MESSAGE_CONSUMED,
            &SCAN,
        ],
    )?
    .into_iter()
    .map(|r| QueuedMessage {
        seq: r.seq as u64,
        text: r.text,
        sha256: r.sha256,
        from: r.from,
    })
    .collect())
}

impl Reader {
    /// The queued messages of a work item that no attempt has taken (verified; a forged row is absent).
    pub fn queued_messages(
        &self,
        outcome: &OutcomeId,
        item: &WorkItemId,
    ) -> Result<Vec<QueuedMessage>> {
        let Some(key) = crate::trust::existing_chain_key(&self.home) else {
            return Ok(vec![]);
        };
        let control = self.get_outcome(outcome)?.control_session_id;
        pending_queued(&self.conn, &key, &control, item)
    }

    /// The `seq`s a worker acked in its own session.
    pub fn owner_deliveries(&self, session_id: &str) -> Result<Vec<u64>> {
        let mut st = self.conn.prepare(
            "SELECT json_extract(payload,'$.seq') FROM events WHERE session_id=?1 AND type=?2 \
             AND json_extract(payload,'$.seq') IS NOT NULL ORDER BY seq",
        )?;
        let rows = st.query_map(params![session_id, ev::OWNER_MESSAGE_DELIVERED], |r| {
            r.get::<_, i64>(0)
        })?;
        Ok(rows
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter_map(|n| u64::try_from(n).ok())
            .collect())
    }

    /// What the worker whose session is `session_id` has to deliver, above `after`. The attempt and
    /// the control session come from the store row of that session, never from the caller. A row
    /// that fails its chain link or its sha256 is skipped (and counted in `scanned`).
    pub fn inbox_after(&self, session_id: &str, after: u64) -> Result<Inbox> {
        let Some((attempt, control)) = self
            .conn
            .query_row(
                "SELECT a.id, o.control_session_id FROM attempts a \
                 JOIN work_items w ON w.id=a.work_item_id JOIN outcomes o ON o.id=w.outcome_id \
                 WHERE a.session_id=?1",
                [session_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
        else {
            return Ok(Inbox::default());
        };
        let Some(key) = crate::trust::existing_chain_key(&self.home) else {
            return Ok(Inbox::default());
        };
        let rows = verified(
            &self.conn,
            &key,
            &control,
            "SELECT seq, payload FROM events WHERE session_id=?1 AND type=?2 AND seq>?3 \
             AND json_extract(payload,'$.attempt')=?4 ORDER BY seq LIMIT ?5",
            &[
                &control,
                &ev::ATTEMPT_MESSAGE,
                &(after as i64),
                &attempt,
                &SCAN,
            ],
        )?;
        let scanned: Option<i64> = self.conn.query_row(
            "SELECT MAX(seq) FROM (SELECT seq FROM events WHERE session_id=?1 AND type=?2 AND seq>?3 \
             AND json_extract(payload,'$.attempt')=?4 ORDER BY seq LIMIT ?5)",
            params![control, ev::ATTEMPT_MESSAGE, after as i64, attempt, SCAN],
            |r| r.get(0),
        )?;
        Ok(Inbox {
            messages: rows
                .into_iter()
                .map(|r| InboxMessage {
                    seq: r.seq as u64,
                    text: r.text,
                    sha256: r.sha256,
                })
                .collect(),
            scanned: scanned.map_or(after, |n| n as u64),
        })
    }
}

impl OutcomeTx<'_, '_> {
    fn count(&self, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Result<i64> {
        Ok(self.et.tx.query_row(sql, args, |r| r.get(0))?)
    }

    /// One message, applied in the caller's transaction: the limits, then the item's state. A refusal
    /// is [`Error::Invalid`] with a fixed sentence (no row text). `Live` when the newest attempt of
    /// the item runs, `Queued` otherwise; a finished item refuses.
    pub fn send_worker_message(&mut self, m: &NewMessage<'_>) -> Result<Sent> {
        check_message_text(m.text).map_err(refuse)?;
        let o = self.outcome()?;
        let item_state: WorkItemState = self
            .et
            .tx
            .query_row(
                "SELECT state FROM work_items WHERE id=?1 AND outcome_id=?2",
                params![m.item.as_str(), self.id.as_str()],
                |r| col(r, 0),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("work item {}", m.item)))?;
        let newest: Option<(String, AttemptState)> = self
            .et
            .tx
            .query_row(
                "SELECT id, state FROM attempts WHERE work_item_id=?1 ORDER BY fence DESC LIMIT 1",
                [m.item.as_str()],
                |r| Ok((r.get(0)?, col(r, 1)?)),
            )
            .optional()?;
        let running = newest.filter(|(_, s)| *s == AttemptState::Running);
        let control = o.control_session_id.as_str();
        let since = |secs: i64| ts(self.now - time::Duration::seconds(secs));
        if let Some((a, _)) = &running
            && self.count(
                "SELECT COUNT(*) FROM events WHERE session_id=?1 AND type=?2 AND ts>=?3 \
                 AND json_extract(payload,'$.attempt')=?4",
                &[&control, &ev::ATTEMPT_MESSAGE, &since(PER_ATTEMPT_SECS), a],
            )? >= PER_ATTEMPT
        {
            return Err(refuse(
                "at most 5 messages to one attempt in 10 minutes; wait, or the worker is flooded",
            ));
        }
        if self.count(
            "SELECT COUNT(*) FROM events WHERE session_id=?1 AND type IN (?2,?3) AND ts>=?4 \
             AND json_extract(payload,'$.requeued_from') IS NULL",
            &[
                &control,
                &ev::ATTEMPT_MESSAGE,
                &ev::ATTEMPT_MESSAGE_QUEUED,
                &since(PER_OUTCOME_SECS),
            ],
        )? >= PER_OUTCOME
        {
            return Err(refuse("at most 20 messages to one outcome in an hour"));
        }
        if o.state.is_terminal()
            || matches!(
                item_state,
                WorkItemState::Succeeded
                    | WorkItemState::TerminalFailure
                    | WorkItemState::Cancelled
            )
        {
            return Err(refuse("this work is finished; create a new goal"));
        }
        if running.is_none() {
            let queued = pending_queued(self.et.tx, self.et.key(), control, m.item)?;
            if queued.len() >= QUEUED_PER_ITEM {
                return Err(refuse(
                    "10 messages already wait for the next attempt of this work item",
                ));
            }
        }
        let text = self.et.redact_str(m.text);
        let seq = self.count(
            "SELECT event_cursor + 1 FROM sessions WHERE id=?1",
            &[&control],
        )? as u64;
        let sha256 = sha_hex(text.as_bytes());
        let mut p = json!({"seq": seq, "sha256": sha256, "text": text,
                           "from": m.from, "via": m.via});
        if let Some(h) = m.proposal_sha256 {
            p["proposal_sha256"] = json!(h);
        }
        let sent = match &running {
            Some((a, _)) => {
                p["attempt"] = json!(a);
                Sent::Live {
                    attempt: AttemptId::parse(a).map_err(refuse)?,
                    seq,
                    sha256,
                }
            }
            None => {
                p["work_item"] = json!(m.item);
                Sent::Queued { seq, sha256 }
            }
        };
        let kind = if running.is_some() {
            ev::ATTEMPT_MESSAGE
        } else {
            ev::ATTEMPT_MESSAGE_QUEUED
        };
        self.et.append(kind, p)?;
        Ok(sent)
    }

    /// Re-queues the live messages of the item's ended attempts that their worker never acked
    /// (the task ended first, the worker was killed, or its binary is older). Each becomes an
    /// `attempt.message.queued` with `requeued_from`; a row that fails its chain link or sha256 is
    /// left alone (it is not laundered into a good one). Safe to call again. Returns how many.
    pub fn requeue_unacked(&mut self, item: &WorkItemId) -> Result<usize> {
        let control = self.outcome()?.control_session_id;
        let ended: Vec<(String, String)> = {
            let mut st = self.et.tx.prepare(
                "SELECT id, session_id FROM attempts WHERE work_item_id=?1 \
                 AND state IN ('finished','lost','cancelled') ORDER BY ordinal",
            )?;
            st.query_map([item.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut n = 0;
        for (attempt, session) in ended {
            let rows = verified(
                self.et.tx,
                self.et.key(),
                &control,
                "SELECT e.seq, e.payload FROM events e WHERE e.session_id=?1 AND e.type=?2 \
                 AND json_extract(e.payload,'$.attempt')=?3 \
                 AND NOT EXISTS (SELECT 1 FROM events x WHERE x.session_id=?1 AND x.type=?4 \
                    AND json_extract(x.payload,'$.requeued_from')=e.seq) \
                 AND NOT EXISTS (SELECT 1 FROM events d WHERE d.session_id=?5 AND d.type=?6 \
                    AND json_extract(d.payload,'$.seq')=e.seq) \
                 ORDER BY e.seq LIMIT ?7",
                &[
                    &control,
                    &ev::ATTEMPT_MESSAGE,
                    &attempt,
                    &ev::ATTEMPT_MESSAGE_QUEUED,
                    &session,
                    &ev::OWNER_MESSAGE_DELIVERED,
                    &SCAN,
                ],
            )?;
            for Row {
                seq: old,
                payload,
                text,
                from,
                ..
            } in rows
            {
                let seq = self.count(
                    "SELECT event_cursor + 1 FROM sessions WHERE id=?1",
                    &[&control],
                )? as u64;
                let mut p = json!({"seq": seq, "sha256": sha_hex(text.as_bytes()), "text": text,
                                   "from": from, "work_item": item, "requeued_from": old,
                                   "reason": "not delivered"});
                if let Some(v) = payload.get("via") {
                    p["via"] = v.clone();
                }
                self.et.append(ev::ATTEMPT_MESSAGE_QUEUED, p)?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// `brief_change`: the pending decision is closed and asked again with a new brief hash, through
    /// the same machinery as a `request_evidence` answer (same stored action and binding; the brief
    /// gains the message's `seq` and hash, so the person sees that it was re-asked). Never extends the expiry.
    pub fn rebrief_for_message(
        &mut self,
        id: &DecisionId,
        expected_revision: i64,
        current_binding: &Value,
        message: (u64, &str),
        action: Option<Value>,
    ) -> Result<DecisionRecord> {
        let note = json!({"owner_message": {"seq": message.0, "sha256": message.1}});
        self.rebrief_decision(id, expected_revision, current_binding, &note, action, true)
    }

    /// The next attempt took these queued messages into its first prompt.
    pub fn mark_messages_consumed(
        &mut self,
        item: &WorkItemId,
        attempt: &AttemptId,
        seqs: &[u64],
    ) -> Result<()> {
        if !seqs.is_empty() {
            self.et.append(
                ev::ATTEMPT_MESSAGE_CONSUMED,
                json!({"work_item": item, "attempt": attempt, "seqs": seqs}),
            )?;
        }
        Ok(())
    }
}

impl OutcomeWriter {
    pub fn send_worker_message(&mut self, m: &NewMessage<'_>) -> Result<Sent> {
        self.atomically(|tx| tx.send_worker_message(m))
    }

    pub fn requeue_unacked(&mut self, item: &WorkItemId) -> Result<usize> {
        self.atomically(|tx| tx.requeue_unacked(item))
    }

    pub fn mark_messages_consumed(
        &mut self,
        item: &WorkItemId,
        attempt: &AttemptId,
        seqs: &[u64],
    ) -> Result<()> {
        self.atomically(|tx| tx.mark_messages_consumed(item, attempt, seqs))
    }

    /// The verified queued messages of `item` the next attempt carries.
    pub fn queued_messages(&self, item: &WorkItemId) -> Result<Vec<QueuedMessage>> {
        Reader::queued_messages(self, self.id(), item)
    }
}
