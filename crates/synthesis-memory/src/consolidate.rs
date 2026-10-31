//! Consolidation (memory.md §7): near-duplicate notes are merged, so repeated reflections do not
//! bloat the store. Never touches `owner` records, slotted records or pinned ones.

use rusqlite::{TransactionBehavior, params};

use crate::reflect::jaccard;
use crate::store::now_iso;
use crate::{Filter, Memory, Result, Store, Trust};

/// Tasks (episodes) between two consolidation passes.
pub const EVERY_N_TASKS: i64 = 20;

impl Store {
    /// True when the workspace has just written its 20th, 40th, ... episode.
    pub fn consolidation_due(&self, workspace_key: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT count(*) FROM memories WHERE kind='episode' AND scope='workspace' AND scope_key=?1
             AND json_extract(source, '$.origin') = 'task_end'",
            [workspace_key],
            |r| r.get(0),
        )?;
        Ok(n > 0 && n % EVERY_N_TASKS == 0)
    }

    /// [`Store::consolidate`] for every workspace (`synth memory prune`).
    pub fn consolidate_all(&mut self) -> Result<usize> {
        let keys: Vec<String> = {
            let mut st = self
                .conn
                .prepare("SELECT DISTINCT scope_key FROM memories WHERE scope='workspace'")?;
            let rows = st.query_map([], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut n = 0;
        for k in keys {
            n += self.consolidate(&k)?;
        }
        Ok(n)
    }

    /// Merges active fact/preference/lesson notes of one scope whose titles are near copies
    /// (Jaccard >= 0.8): the stronger one (trust, then use count, then newer) stays, takes the
    /// other's counters and the other becomes `superseded`. Returns how many were merged.
    pub fn consolidate(&mut self, workspace_key: &str) -> Result<usize> {
        let mut merged = 0;
        for kind in ["fact", "preference", "lesson"] {
            let mut rows: Vec<Memory> = self
                .list(&Filter {
                    workspace: Some(workspace_key.into()),
                    kind: Some(kind.into()),
                    status: Some("active".into()),
                    limit: 5000,
                })?
                .into_iter()
                .filter(|m| m.trust != Trust::Owner && m.slot.is_none() && !m.pinned)
                .collect();
            // strongest first, so a record is only ever merged into one that outranks it
            rows.sort_by(|a, b| {
                b.trust
                    .cmp(&a.trust)
                    .then(b.use_count.cmp(&a.use_count))
                    .then(b.updated_at.cmp(&a.updated_at))
            });
            let mut gone: Vec<String> = vec![];
            for i in 0..rows.len() {
                if gone.contains(&rows[i].id) {
                    continue;
                }
                for j in i + 1..rows.len() {
                    let (w, l) = (&rows[i], &rows[j]);
                    if gone.contains(&l.id) || w.scope != l.scope || w.scope_key != l.scope_key {
                        continue;
                    }
                    if jaccard(&w.title, &l.title) < 0.8 {
                        continue;
                    }
                    let tx = self
                        .conn
                        .transaction_with_behavior(TransactionBehavior::Immediate)?;
                    let now = now_iso();
                    tx.execute(
                        "UPDATE memories SET status='superseded', updated_at=?2 WHERE id=?1",
                        params![l.id, now],
                    )?;
                    tx.execute(
                        "UPDATE memories SET use_count=use_count+?2, success_count=success_count+?3,
                           failure_count=failure_count+?4, supersedes=coalesce(supersedes, ?5), updated_at=?6
                         WHERE id=?1",
                        params![w.id, l.use_count, l.success_count, l.failure_count, l.id, now],
                    )?;
                    tx.commit()?;
                    gone.push(l.id.clone());
                    merged += 1;
                }
            }
        }
        Ok(merged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewMemory;

    fn add(s: &mut Store, title: &str, body: &str, trust: Trust) -> String {
        s.put(NewMemory::new("workspace", "w", "fact", title, body, trust))
            .unwrap()
            .id
    }

    #[test]
    fn near_duplicate_titles_merge_into_the_stronger_and_owner_notes_stay() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::open(d.path(), vec![]).unwrap();
        let a = add(
            &mut s,
            "Run the parser tests with cargo test",
            "body one",
            Trust::Verified,
        );
        let b = add(
            &mut s,
            "Run the parser tests with cargo test -p",
            "body two",
            Trust::Agent,
        );
        let other = add(
            &mut s,
            "Deploys go through make release",
            "body three",
            Trust::Agent,
        );
        let o1 = add(
            &mut s,
            "Run the parser tests with cargo test now",
            "owner body",
            Trust::Owner,
        );
        s.record_uses(std::slice::from_ref(&b), "s", None, "injected")
            .unwrap();
        assert_eq!(s.consolidate("w").unwrap(), 1);
        let status = |id: &str| s.get(id).unwrap().unwrap();
        assert_eq!(status(&a).status, "active");
        assert_eq!(status(&b).status, "superseded");
        assert_eq!(status(&other).status, "active");
        assert_eq!(status(&o1).status, "active", "owner notes are never merged");
        assert_eq!(
            status(&a).use_count,
            1,
            "the loser's counters move to the winner"
        );
        assert_eq!(status(&a).supersedes.as_deref(), Some(b.as_str()));
        assert_eq!(s.consolidate("w").unwrap(), 0, "idempotent");
    }

    #[test]
    fn due_every_twentieth_task() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::open(d.path(), vec![]).unwrap();
        for i in 0..20 {
            assert!(!s.consolidation_due("w").unwrap(), "{i}");
            let mut n = NewMemory::new(
                "workspace",
                "w",
                "episode",
                &format!("t{i}"),
                &format!("b{i}"),
                Trust::Agent,
            );
            n.source = serde_json::json!({"origin": "task_end"});
            s.put(n).unwrap();
        }
        assert!(s.consolidation_due("w").unwrap());
    }
}
