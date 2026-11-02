//! The `/peers` and `/agents --all` table: `WORKSPACE | SESSION | AGENT | KIND | STATUS | TASK`
//! (messaging.md B9), built from the rows the harness lists.

use serde_json::Value;

fn s<'a>(v: &'a Value, path: &[&str]) -> &'a str {
    path.iter()
        .try_fold(v, |v, k| v.get(k))
        .and_then(Value::as_str)
        .unwrap_or("")
}

pub(crate) fn table(rows: &[Value]) -> Vec<String> {
    let cells: Vec<[String; 6]> = rows
        .iter()
        .map(|r| {
            let sess = match s(r, &["session", "name"]) {
                "" => s(r, &["session", "short_id"]).to_string(),
                n => format!("{} · {n}", s(r, &["session", "short_id"])),
            };
            [
                format!(
                    "{} · {}",
                    s(r, &["workspace", "label"]),
                    s(r, &["workspace", "root"])
                ),
                sess,
                s(r, &["agent", "instance"]).into(),
                s(r, &["agent", "kind"]).into(),
                s(r, &["agent", "status"]).into(),
                s(r, &["task_summary"]).into(),
            ]
        })
        .collect();
    let head = ["WORKSPACE", "SESSION", "AGENT", "KIND", "STATUS", "TASK"].map(String::from);
    let mut w = [0; 6];
    for row in std::iter::once(&head).chain(&cells) {
        for (i, c) in row.iter().enumerate() {
            w[i] = w[i].max(c.chars().count());
        }
    }
    let line = |row: &[String; 6]| {
        let mut l = String::from("  ");
        for (i, c) in row.iter().enumerate() {
            l.push_str(&format!(
                "{c:<width$}  ",
                width = if i == 5 { 0 } else { w[i] }
            ));
        }
        l.trim_end().to_string()
    };
    std::iter::once(&head).chain(&cells).map(line).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rows_become_aligned_columns() {
        let rows = [json!({"workspace": {"label": "api", "root": "/w/api"},
            "session": {"short_id": "3c9e01aa", "name": "backend"},
            "agent": {"instance": "main", "kind": "main", "status": "idle"}, "task_summary": "fix it"})];
        let t = table(&rows);
        assert_eq!(t.len(), 2);
        assert!(t[0].starts_with("  WORKSPACE") && t[0].ends_with("TASK"));
        assert!(
            t[1].contains("api · /w/api")
                && t[1].contains("3c9e01aa · backend")
                && t[1].ends_with("fix it")
        );
    }
}
