use base64::Engine;
use serde_json::Value;
use synthesis_core::ToolContext;

use crate::util::*;
use crate::{Block, Content, ImageSource, ToolOutput};

const DEFAULT_LIMIT: i64 = 2000;
const MAX_LINE_LENGTH: usize = 2000;
/// A whole-file read (no offset/limit) of a text file over this many bytes is refused.
const WHOLE_FILE_MAX_BYTES: usize = 256 * 1024;
/// Most estimated tokens (4 chars each) one call may return.
const MAX_RESULT_TOKENS: usize = 25_000;

fn image_type(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

/// JS `String.length` counts UTF-16 units; Node truncates on that.
fn truncate_line(line: &str) -> String {
    if line.encode_utf16().count() <= MAX_LINE_LENGTH {
        return line.to_string();
    }
    let units: Vec<u16> = line.encode_utf16().take(MAX_LINE_LENGTH).collect();
    format!("{} … [line truncated]", String::from_utf16_lossy(&units))
}

fn cell_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn render_notebook(nb: &Value) -> String {
    let mut out: Vec<String> = Vec::new();
    for (i, cell) in nb
        .get("cells")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let id = cell
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map_or_else(|| format!("cell-{i}"), str::to_string);
        let ty = cell
            .get("cell_type")
            .and_then(Value::as_str)
            .unwrap_or("undefined");
        out.push(format!("<cell id=\"{id}\" type=\"{ty}\">"));
        out.push(cell_text(cell.get("source")));
        for o in cell
            .get("outputs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let s = |k: &str| o.get(k).and_then(Value::as_str).unwrap_or("undefined");
            match o.get("output_type").and_then(Value::as_str) {
                Some("stream") => out.push(format!("[output]\n{}", cell_text(o.get("text")))),
                Some("error") => out.push(format!("[error] {}: {}", s("ename"), s("evalue"))),
                _ => match o.get("data") {
                    Some(d) if d.get("text/plain").is_some_and(|t| !t.is_null()) => {
                        out.push(format!("[output]\n{}", cell_text(d.get("text/plain"))));
                    }
                    Some(Value::Object(d)) => {
                        let keys: Vec<&str> = d.keys().map(String::as_str).collect();
                        out.push(format!("[output: {}]", keys.join(", ")));
                    }
                    _ => {}
                },
            }
        }
        out.push("</cell>".to_string());
    }
    out.join("\n")
}

pub(crate) fn run(input: &Value, ctx: &ToolContext) -> ToolOutput {
    let Some(file_path) = str_field(input, "file_path") else {
        return ToolOutput::error("Read: \"file_path\" is required.");
    };
    let abs = match resolve(ctx, "Read", file_path) {
        Ok(p) => p,
        Err(m) => return ToolOutput::error(m),
    };
    let root = match open_root(ctx, "Read", &abs) {
        Ok(r) => r,
        Err(o) => return o,
    };
    let md = match root.metadata(&abs) {
        Ok(m) => m,
        Err(e) => {
            let msg = format!(
                "Read: cannot access {}: {}",
                abs.display(),
                io_msg(&e, &abs)
            );
            return if e.kind() == std::io::ErrorKind::NotFound {
                ToolOutput::soft_error(msg)
            } else {
                ToolOutput::error(msg)
            };
        }
    };
    if md.is_dir() {
        return ToolOutput::soft_error(format!(
            "Read: {} is a directory. Use Glob or Bash `ls` to list its contents.",
            abs.display()
        ));
    }
    if !md.is_file() {
        return ToolOutput::error(format!(
            "Read: {} is not a regular file (pipe, device or socket).",
            abs.display()
        ));
    }
    let ext = abs
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    // Divergence from Node (no cap): files over 10 MiB are refused instead of loaded.
    // shortcut: no windowed read for larger files; use Grep or Bash `sed -n` for a slice.
    let buf = match root.read_capped(&abs, MAX_FILE_BYTES) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::FileTooLarge => {
            return ToolOutput::error(format!(
                "Read: {} is larger than {} MiB ({} bytes); use Grep to search it or Bash (`sed -n 'A,Bp'`) to read a slice.",
                abs.display(),
                MAX_FILE_BYTES >> 20,
                md.len()
            ));
        }
        Err(e) => {
            return ToolOutput::error(format!("Read: cannot read {}: {e}", abs.display()));
        }
    };
    if let Some(media_type) = image_type(&ext) {
        ledger_set(ctx, &abs, mtime(&md));
        let data = base64::engine::general_purpose::STANDARD.encode(&buf);
        return ToolOutput {
            content: Content::Blocks(vec![Block::Image {
                source: ImageSource::Base64 {
                    media_type: media_type.to_string(),
                    data,
                },
            }]),
            ..ToolOutput::text("")
        };
    }
    if ext == "ipynb" && !buf.is_empty() {
        // Invalid JSON (or `null`) falls through and shows the raw text, as in Node.
        if let Ok(nb) = serde_json::from_slice::<Value>(&buf).map(|v| (!v.is_null()).then_some(v))
            && let Some(nb) = nb
        {
            ledger_set(ctx, &abs, mtime(&md));
            let text = render_notebook(&nb);
            return ToolOutput::text(if text.is_empty() {
                "Note: this notebook has no cells."
            } else {
                &text
            });
        }
    }
    if buf.is_empty() {
        ledger_set(ctx, &abs, mtime(&md));
        return ToolOutput::text("Note: this file exists but has empty contents.");
    }
    let buf = if ext == "pdf" {
        match crate::pdf::extract(&buf, input["pages"].as_str()) {
            Ok(t) => t.into_bytes(),
            Err(m) => return ToolOutput::error(format!("Read: {}: {m}", abs.display())),
        }
    } else {
        buf
    };
    if is_binary(&buf) {
        return ToolOutput::error(format!(
            "Read: {} appears to be a binary file and cannot be displayed as text.",
            abs.display()
        ));
    }
    let text = String::from_utf8_lossy(&buf);
    let text = text.strip_suffix('\n').unwrap_or(&text);

    let windowed = int_field(input, "offset").is_some() || int_field(input, "limit").is_some();
    if !windowed && text.len() > WHOLE_FILE_MAX_BYTES {
        return ToolOutput::error(format!(
            "Read: {} is {} KB, over the {} KB whole-file limit. Read it in parts with offset and limit (e.g. offset=1, limit=500) or search it with Grep.",
            abs.display(),
            text.len().div_ceil(1024),
            WHOLE_FILE_MAX_BYTES >> 10
        ));
    }
    let offset = int_field(input, "offset").filter(|&n| n > 0).unwrap_or(1);
    let limit = int_field(input, "limit")
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_LIMIT);
    let start = usize::try_from(offset - 1).unwrap_or(usize::MAX);
    let out = text
        .split('\n')
        .enumerate()
        .skip(start)
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
        .map(|(i, line)| format!("{:>6}\t{}", i + 1, truncate_line(line)))
        .collect::<Vec<_>>()
        .join("\n");

    if out.is_empty() {
        ledger_set(ctx, &abs, mtime(&md));
        return ToolOutput::text(format!(
            "Note: offset {offset} is past the end of the file ({} lines).",
            text.split('\n').count()
        ));
    }
    let tokens = out.chars().count().div_ceil(4);
    if tokens > MAX_RESULT_TOKENS {
        return ToolOutput::error(format!(
            "Read: this window of {} is about {tokens} tokens, over the {MAX_RESULT_TOKENS}-token limit. Use a smaller limit (fewer lines) with offset, or search it with Grep.",
            abs.display()
        ));
    }
    ledger_set(ctx, &abs, mtime(&md));
    ToolOutput::text(out)
}
