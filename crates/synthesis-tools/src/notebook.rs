//! NotebookEdit (port of notebook.js). The file is opened through the cap-std workspace root
//! like Write/Edit, parsed with key order preserved, and written back with its own indent.

use serde::Serialize;
use serde_json::{Map, Value, json, ser::PrettyFormatter};
use synthesis_core::ToolContext;

use crate::ToolOutput;
use crate::util::*;

fn find_cell(cells: &[Value], id: &str) -> Option<usize> {
    cells
        .iter()
        .position(|c| c["id"].as_str() == Some(id))
        .or_else(|| {
            let n: usize = id.strip_prefix("cell-")?.parse().ok()?;
            (n < cells.len()).then_some(n)
        })
}

/// Keeps the file's own indentation (Jupyter writes 1 space).
fn indent_of(raw: &str) -> usize {
    raw.strip_prefix("{\n")
        .map(|r| r.bytes().take_while(|b| *b == b' ').count())
        .filter(|n| *n > 0 && raw[2 + n..].starts_with('"'))
        .unwrap_or(1)
}

fn new_id(cells: &[Value]) -> String {
    loop {
        let id = format!("{:08x}", fastrand::u32(..));
        if !cells.iter().any(|c| c["id"].as_str() == Some(&id)) {
            return id;
        }
    }
}

fn set_code_fields(cell: &mut Map<String, Value>) {
    cell.insert("outputs".into(), json!([]));
    cell.insert("execution_count".into(), Value::Null);
}

pub(crate) fn run(input: &Value, ctx: &ToolContext) -> ToolOutput {
    let e = |m: &str| ToolOutput::error(format!("NotebookEdit: {m}"));
    let Some(path) = str_field(input, "notebook_path") else {
        return e("\"notebook_path\" is required.");
    };
    let abs = match resolve(ctx, "NotebookEdit", path) {
        Ok(p) => p,
        Err(m) => return ToolOutput::error(m),
    };
    if !abs
        .to_string_lossy()
        .to_ascii_lowercase()
        .ends_with(".ipynb")
    {
        return e(&format!("{} is not a .ipynb file.", abs.display()));
    }
    let mode = str_field(input, "edit_mode").unwrap_or("replace");
    if !["replace", "insert", "delete"].contains(&mode) {
        return e(&format!("unknown edit_mode \"{mode}\"."));
    }
    let new_source = input.get("new_source").and_then(Value::as_str);
    if mode != "delete" && new_source.is_none() {
        return e("\"new_source\" is required.");
    }
    let cell_type = str_field(input, "cell_type");
    if mode == "insert" && !matches!(cell_type, Some("code" | "markdown")) {
        return e("\"cell_type\" (code or markdown) is required for insert.");
    }

    let root = match open_root(ctx, "NotebookEdit", &abs) {
        Ok(r) => r,
        Err(o) => return o,
    };
    let md = match root.metadata(&abs) {
        Ok(m) => m,
        Err(x) => {
            let msg = format!(
                "NotebookEdit: cannot access {}: {}",
                abs.display(),
                io_msg(&x, &abs)
            );
            return if x.kind() == std::io::ErrorKind::NotFound {
                ToolOutput::soft_error(msg)
            } else {
                ToolOutput::error(msg)
            };
        }
    };
    if !is_fresh(ctx, &abs, mtime(&md)) {
        return ToolOutput::soft_error(format!(
            "NotebookEdit: {} has not been read in this conversation (or changed since). Read it first.",
            abs.display()
        ));
    }
    let raw = match root
        .read_capped(&abs, MAX_FILE_BYTES)
        .map_err(|x| x.to_string())
        .and_then(|b| String::from_utf8(b).map_err(|x| x.to_string()))
    {
        Ok(r) => r,
        Err(x) => return e(&format!("cannot read {}: {x}", abs.display())),
    };
    let mut nb: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(x) => {
            return e(&format!(
                "{} is not valid notebook JSON: {x}",
                abs.display()
            ));
        }
    };
    // Only nbformat 4 is understood; anything else (3, 5, missing) may use another cell layout.
    if nb["nbformat"].as_u64() != Some(4) {
        return e(&format!(
            "{} has nbformat {}, only nbformat 4 is supported.",
            abs.display(),
            nb["nbformat"]
        ));
    }
    let with_ids = nb["nbformat_minor"].as_u64().unwrap_or(0) >= 5;
    let Some(cells) = nb["cells"].as_array_mut() else {
        return e(&format!("{} has no \"cells\" array.", abs.display()));
    };
    if cells.iter().any(|c| !c.is_object()) {
        return e(&format!(
            "{} has a cell that is not an object.",
            abs.display()
        ));
    }

    let cell_id = str_field(input, "cell_id");
    let at = cell_id.and_then(|id| find_cell(cells, id));
    if let (Some(id), None) = (cell_id, at) {
        return ToolOutput::soft_error(format!(
            "NotebookEdit: no cell \"{id}\" in {}.",
            abs.display()
        ));
    }
    if mode != "insert" && at.is_none() {
        return ToolOutput::soft_error(format!(
            "NotebookEdit: \"cell_id\" is required for {mode}."
        ));
    }

    // cell source before the edit (`source` is a string or an array of lines)
    let src_of = |c: &Value| match &c["source"] {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    };
    let old_src = at.map(|i| src_of(&cells[i])).unwrap_or_default();
    let message = match (mode, at) {
        ("delete", Some(i)) => {
            cells.remove(i);
            format!("Deleted cell {}.", cell_id.unwrap_or_default())
        }
        ("replace", Some(i)) => {
            let cell = cells[i].as_object_mut().expect("checked");
            cell.insert("source".into(), json!(new_source.unwrap_or_default()));
            let old = cell
                .get("cell_type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            match cell_type {
                Some(t) if t != old => {
                    cell.insert("cell_type".into(), json!(t));
                    if t == "code" {
                        set_code_fields(cell);
                    } else {
                        cell.shift_remove("outputs");
                        cell.shift_remove("execution_count");
                    }
                }
                // the old outputs belong to the old source
                _ if old == "code" => set_code_fields(cell),
                _ => {}
            }
            format!(
                "Replaced the source of cell {}.",
                cell_id.unwrap_or_default()
            )
        }
        _ => {
            let t = cell_type.unwrap_or("code");
            let mut cell = Map::new();
            cell.insert("cell_type".into(), json!(t));
            cell.insert("metadata".into(), json!({}));
            cell.insert("source".into(), json!(new_source.unwrap_or_default()));
            if t == "code" {
                set_code_fields(&mut cell);
            }
            let mut id_note = String::new();
            if with_ids {
                let id = new_id(cells);
                id_note = format!(" (id {id})");
                cell.insert("id".into(), json!(id));
            }
            cells.insert(at.map_or(0, |i| i + 1), Value::Object(cell));
            let place = match cell_id {
                Some(id) if at.is_some() => format!("after {id}"),
                _ => "at the top".to_string(),
            };
            format!("Inserted a {t} cell {place}{id_note}.")
        }
    };
    let n = cells.len();

    let pad = vec![b' '; indent_of(&raw)];
    let mut out = Vec::new();
    let mut ser =
        serde_json::Serializer::with_formatter(&mut out, PrettyFormatter::with_indent(&pad));
    if let Err(x) = nb.serialize(&mut ser) {
        return e(&format!("cannot serialize notebook: {x}"));
    }
    out.push(b'\n');
    let text = String::from_utf8(out).expect("serde_json emits UTF-8");
    if let Err(x) = root.write(&abs, &text) {
        return e(&format!("cannot write {}: {x}", abs.display()));
    }
    match root.metadata(&abs) {
        Ok(m) => ledger_set(ctx, &abs, mtime(&m)),
        Err(x) => return e(&format!("cannot access {}: {x}", abs.display())),
    }
    // line numbers are relative to the cell source
    let (kind, before) = match mode {
        "delete" => ("delete", old_src.as_str()),
        "insert" => ("create", ""),
        _ => ("update", old_src.as_str()),
    };
    let after = if mode == "delete" {
        ""
    } else {
        new_source.unwrap_or_default()
    };
    ToolOutput::text(format!("{message} ({n} cells)"))
        .with_meta(json!({"cells": n, "diff": crate::diff::for_file(&ctx.workspace.secrets, &ctx.workspace.root, path, kind, before, after)}))
}
