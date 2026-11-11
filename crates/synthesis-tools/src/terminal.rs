//! Bash, BashOutput, KillShell and the terminal_* tools on top of `synthesis_terminal`.
//! Commands always run under the runtime's fixed shell with `Env::child_env` plus the explicit
//! per-call env; `bash_id` == `process_id`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use regex::Regex;
use serde_json::{Value, json};
use synthesis_core::ToolContext;
use synthesis_terminal::{
    Error, ExecSpec, ProcessHandle, ProcessStatus, Readiness, Signal, SpawnSpec,
};
use tokio::time::sleep;

use crate::native::{Shared, lock};
use crate::util::str_field;
use crate::{BoxFuture, ToolOutput};

const MAX_OUTPUT: usize = 30_000;
const DEFAULT_TIMEOUT_MS: f64 = 120_000.0;
const MAX_TIMEOUT_MS: f64 = 600_000.0;
const MAX_LOG_READ: u64 = 1_000_000;

tokio::task_local! {
    /// Live output of the Bash call running in this task: set by the harness around
    /// `Tool::execute`, so a tool call stays `(input, ctx) -> output` for everyone else.
    pub static LIVE_OUTPUT: synthesis_terminal::OutputSink;
    /// Run the Bash call unconfined. Set only by the harness for a command the person typed
    /// (`!cmd`) when their settings say so; the model's input can never reach this.
    pub static UNCONFINED: bool;
}

fn kind(sh: &Shared) -> &'static str {
    if sh.rt.shell().is_posix() {
        "posix"
    } else {
        "cmd"
    }
}

fn state(st: &ProcessStatus) -> String {
    let mut s = match (st.running, st.exit_code, st.signal) {
        (true, ..) => "running".to_string(),
        (_, Some(c), _) => format!("exited (exit code {c})"),
        (_, _, Some(n)) => format!("killed by signal {n}"),
        _ => "exited".to_string(),
    };
    if st.output_limit_exceeded {
        s += ", output limit exceeded";
    }
    if st.max_runtime_exceeded {
        s += ", max runtime exceeded";
    }
    s
}

fn status_json(st: &ProcessStatus) -> Value {
    json!({
        "process_id": st.process_id, "name": st.name, "pid": st.pid,
        "status": if st.running { "running" } else { "exited" },
        "exit_code": st.exit_code, "signal": st.signal,
        "output_limit_exceeded": st.output_limit_exceeded,
        "max_runtime_exceeded": st.max_runtime_exceeded,
    })
}

/// `meta.process` for the harness: the `process.exited` payload once the process is gone.
fn proc_meta(st: &ProcessStatus) -> Value {
    json!({"process": st.to_event_info(), "running": st.running})
}

fn unknown(tool: &str, id: &str, e: &Error) -> ToolOutput {
    match e {
        Error::UnknownProcess(_) => ToolOutput::error(format!("{tool}: no process \"{id}\".")),
        e => ToolOutput::error(format!("{tool}: {e}")),
    }
}

fn str_arg<'a>(input: &'a Value, tool: &str, key: &str) -> Result<&'a str, ToolOutput> {
    str_field(input, key)
        .ok_or_else(|| ToolOutput::error(format!("{tool}: \"{key}\" is required.")))
}

async fn start(
    sh: &Shared,
    ctx: &ToolContext,
    tool: &str,
    spec: SpawnSpec,
) -> Result<(ProcessHandle, ProcessStatus), ToolOutput> {
    sh.session(ctx);
    let h = tokio::select! {
        r = sh.rt.spawn(&ctx.session_id, spec) => r.map_err(|e| ToolOutput::error(format!("{tool}: {e}")))?,
        () = ctx.cancel.cancelled() => return Err(ToolOutput::error(format!("{tool}: aborted."))),
    };
    let st = sh
        .rt
        .get_status(&ctx.session_id, &h.process_id)
        .map_err(|e| unknown(tool, &h.process_id, &e))?;
    Ok((h, st))
}

/// Precise note for a sandbox denial in `output` (first absolute path on an EPERM/EACCES line).
/// Paths inside the workspace are plain permission errors and get no note.
fn sandbox_hint(output: &str, workspace: &std::path::Path) -> Option<String> {
    static PATH: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = PATH.get_or_init(|| Regex::new(r"(/[\w.@+~%-]+(?:/[\w.@+~%-]+)*)").unwrap());
    let line = output.lines().find(|l| {
        let l = l.to_lowercase();
        l.contains("operation not permitted") || l.contains("permission denied")
    })?;
    let path = re
        .find_iter(line)
        .map(|m| m.as_str())
        .find(|p| p.len() > 1)?;
    if std::path::Path::new(path).starts_with(workspace) {
        return None;
    }
    Some(format!(
        "[blocked by Synthesis's sandbox, not by an OS permission: access to {path} is outside what commands may use. \
Reads outside the workspace are limited to system and toolchain directories; writes to the workspace and a private temp dir. \
If {path} must be READ, tell the user to restart synthesis with `--sandbox-read {path}`. \
Do NOT suggest Full Disk Access, System Settings, sudo or chmod.]"
    ))
}

pub(crate) fn bash<'a>(
    sh: &'a Shared,
    input: &'a Value,
    ctx: &'a ToolContext,
) -> BoxFuture<'a, ToolOutput> {
    Box::pin(async move {
        let command = match str_arg(input, "Bash", "command") {
            Ok(c) => c,
            Err(e) => return e,
        };
        if ctx.cancel.is_cancelled() {
            return ToolOutput::error("Bash: aborted before start.");
        }
        sh.session(ctx);
        let shell_kind = kind(sh);
        if input["run_in_background"].as_bool() == Some(true) {
            let mut spec = SpawnSpec::new(command);
            spec.name = str_field(input, "description").map(Into::into);
            return match start(sh, ctx, "Bash", spec).await {
                Ok((h, _)) => ToolOutput::text(format!(
                    "Started background command {id}: {command}\nRead its output with BashOutput (bash_id: \"{id}\"); stop it with KillShell (shell_id: \"{id}\").",
                    id = h.process_id
                ))
                .with_meta(json!({"bash_id": h.process_id, "pid": h.pid, "artifact_path": h.log_path, "shell_kind": shell_kind})),
                Err(e) => e,
            };
        }
        let timeout_ms = input["timeout"]
            .as_f64()
            .filter(|t| t.is_finite())
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1.0, MAX_TIMEOUT_MS) as u64;
        let mut spec = ExecSpec::new(command);
        spec.timeout = Duration::from_millis(timeout_ms);
        spec.max_output_bytes = MAX_OUTPUT;
        spec.on_output = LIVE_OUTPUT.try_with(Clone::clone).ok();
        spec.unconfined = UNCONFINED.try_with(|u| *u).unwrap_or(false);
        spec.background_on_timeout = sh
            .background_on_timeout
            .load(std::sync::atomic::Ordering::Relaxed);
        let r = match sh.rt.exec(&ctx.session_id, spec, &ctx.cancel).await {
            Ok(r) => r,
            Err(e) => return ToolOutput::error(format!("Bash: {e}")),
        };
        let mut notes = vec![];
        if r.timed_out {
            notes.push(format!("[timed out after {timeout_ms}ms]"));
        }
        if let Some(id) = &r.background_id {
            notes.push(format!(
                "[timed out after {timeout_ms}ms; the command was NOT killed, it now runs in the background as {id}. Read its output with BashOutput (bash_id: \"{id}\"); stop it with KillShell (shell_id: \"{id}\")]"
            ));
        }
        if r.cancelled {
            notes.push("[aborted]".into());
        }
        if r.output_limit_exceeded {
            notes.push("[output limit exceeded, process killed]".into());
        }
        let mut text = r.output.clone();
        if !notes.is_empty() {
            text += &format!(
                "{}{}",
                if text.is_empty() { "" } else { "\n\n" },
                notes.join(" ")
            );
        }
        let hint = (r.sandbox.is_some() && r.exit_code.is_some_and(|c| c != 0))
            .then(|| sandbox_hint(&r.output, &ctx.workspace.root))
            .flatten();
        if let Some(h) = hint {
            text += &format!("\n\n{h}");
        }
        match (r.exit_code, r.signal) {
            (Some(0), _) => {}
            (Some(c), _) => {
                text += &format!(
                    "{}[exit code {c}]",
                    if text.is_empty() { "" } else { "\n\n" }
                )
            }
            (None, Some(s)) if notes.is_empty() => {
                text += &format!(
                    "{}[terminated by signal {s}]",
                    if text.is_empty() { "" } else { "\n\n" }
                )
            }
            _ => {}
        }
        let failed = r.background_id.is_none()
            && (r.timed_out || r.cancelled || r.output_limit_exceeded || r.exit_code != Some(0));
        let mut out = ToolOutput::text(text).with_meta(json!({
            "exit_code": r.exit_code, "signal": r.signal, "timed_out": r.timed_out,
            "truncated": r.truncated, "total_bytes": r.total_bytes,
            "artifact_path": r.log_path, "shell_kind": shell_kind,
            "signal_name": r.signal.map(synthesis_terminal::signal_name),
        }));
        out.is_error = failed;
        if let Some(id) = &r.background_id {
            out.meta["bash_id"] = json!(id);
        }
        out
    })
}

/// Output after the stored cursor (BashOutput) or an explicit one (terminal_logs).
fn read(
    sh: &Shared,
    ctx: &ToolContext,
    tool: &str,
    id: &str,
    cursor: Option<&str>,
    filter: Option<&str>,
    max: usize,
) -> Result<(String, String, ProcessStatus, bool), ToolOutput> {
    let re = filter
        .map(Regex::new)
        .transpose()
        .map_err(|e| ToolOutput::error(format!("{tool}: invalid filter regex: {e}")))?;
    let chunk = sh
        .rt
        .read_logs(&ctx.session_id, id, cursor, max)
        .map_err(|e| unknown(tool, id, &e))?;
    let st = sh
        .rt
        .get_status(&ctx.session_id, id)
        .map_err(|e| unknown(tool, id, &e))?;
    let data = match re {
        Some(re) => chunk
            .data
            .split('\n')
            .filter(|l| re.is_match(l))
            .collect::<Vec<_>>()
            .join("\n"),
        None => chunk.data,
    };
    Ok((data, chunk.cursor, st, chunk.eof))
}

fn log_text(data: &str, id: &str, st: &ProcessStatus, tail: &str) -> String {
    let nl = if data.is_empty() || data.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    format!("{data}{nl}[{id}: {}{tail}]", state(st))
}

pub(crate) fn bash_output<'a>(
    sh: &'a Shared,
    input: &'a Value,
    ctx: &'a ToolContext,
) -> BoxFuture<'a, ToolOutput> {
    Box::pin(async move {
        let id = match str_arg(input, "BashOutput", "bash_id") {
            Ok(i) => i,
            Err(e) => return e,
        };
        let s = sh.session(ctx);
        let cur = lock(&s.cursors).get(id).cloned();
        match read(
            sh,
            ctx,
            "BashOutput",
            id,
            cur.as_deref(),
            str_field(input, "filter"),
            MAX_OUTPUT,
        ) {
            Ok((data, cursor, st, eof)) => {
                // Filtered-out lines are consumed, like Node.
                let more = if !st.running && !eof {
                    "; more output pending"
                } else {
                    ""
                };
                lock(&s.cursors).insert(id.into(), cursor);
                ToolOutput::text(log_text(&data, id, &st, more)).with_meta(proc_meta(&st))
            }
            Err(e) => e,
        }
    })
}

async fn kill(
    sh: &Shared,
    ctx: &ToolContext,
    tool: &str,
    id: &str,
    sig: Signal,
) -> Result<ProcessStatus, ToolOutput> {
    sh.session(ctx);
    sh.rt
        .signal(&ctx.session_id, id, sig)
        .map_err(|e| unknown(tool, id, &e))?;
    for _ in 0..40 {
        let st = sh
            .rt
            .get_status(&ctx.session_id, id)
            .map_err(|e| unknown(tool, id, &e))?;
        if !st.running || sig != Signal::Kill {
            return Ok(st);
        }
        sleep(Duration::from_millis(50)).await;
    }
    sh.rt
        .get_status(&ctx.session_id, id)
        .map_err(|e| unknown(tool, id, &e))
}

pub(crate) fn kill_shell<'a>(
    sh: &'a Shared,
    input: &'a Value,
    ctx: &'a ToolContext,
) -> BoxFuture<'a, ToolOutput> {
    Box::pin(async move {
        let id = match str_arg(input, "KillShell", "shell_id") {
            Ok(i) => i,
            Err(e) => return e,
        };
        sh.session(ctx);
        match sh.rt.get_status(&ctx.session_id, id) {
            Err(e) => return unknown("KillShell", id, &e),
            Ok(st) if !st.running => {
                return ToolOutput::text(format!("{id} had already exited ({}).", state(&st)))
                    .with_meta(proc_meta(&st));
            }
            Ok(_) => {}
        }
        match kill(sh, ctx, "KillShell", id, Signal::Kill).await {
            Ok(st) if st.running => {
                ToolOutput::error(format!("KillShell: {id} is still running after SIGKILL."))
            }
            Ok(st) => ToolOutput::text(format!("Killed {id}.")).with_meta(proc_meta(&st)),
            Err(e) => e,
        }
    })
}

const ENV_EXACT: [&str; 14] = [
    "NODE_ENV",
    "PORT",
    "HOST",
    "DEBUG",
    "LOG_LEVEL",
    "RUST_LOG",
    "RUST_BACKTRACE",
    "CI",
    "FORCE_COLOR",
    "NO_COLOR",
    "TZ",
    "LANG",
    "LANGUAGE",
    "COLUMNS",
];
const ENV_PREFIX: [&str; 4] = ["LC_", "NEXT_PUBLIC_", "VITE_", "REACT_APP_"];
const ENV_ALLOWED_DOC: &str = "NODE_ENV, PORT, HOST, DEBUG, LOG_LEVEL, RUST_LOG, RUST_BACKTRACE, CI, FORCE_COLOR, NO_COLOR, TZ, LANG, LANGUAGE, COLUMNS, LC_*, NEXT_PUBLIC_*, VITE_*, REACT_APP_*";

/// Why a model-supplied env name is refused, `None` if allowed. Exact, case-sensitive match
/// against a short list (so PATH, GIT_*, NODE_OPTIONS, LD_*, *_PROXY, `Path`, ... all fail);
/// the prefixes cover public build-time config and must not look like a secret.
fn env_refusal(name: &str) -> Option<&'static str> {
    if ENV_EXACT.contains(&name) {
        return None;
    }
    if let Some(rest) = ENV_PREFIX.iter().find_map(|p| name.strip_prefix(p)) {
        let ok = !rest.is_empty()
            && rest
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
        if !ok {
            return Some("name must be upper-case letters, digits and _");
        }
        let secret = [
            "KEY",
            "TOKEN",
            "SECRET",
            "PASSWORD",
            "PASSWD",
            "CREDENTIAL",
            "AUTH",
        ];
        if secret.iter().any(|w| rest.contains(w)) {
            return Some("looks like a secret");
        }
        return None;
    }
    Some("not on the allowlist")
}

pub(crate) fn spawn<'a>(
    sh: &'a Shared,
    input: &'a Value,
    ctx: &'a ToolContext,
) -> BoxFuture<'a, ToolOutput> {
    Box::pin(async move {
        let command = match str_arg(input, "terminal_spawn", "command") {
            Ok(c) => c,
            Err(e) => return e,
        };
        if input["tty"].as_bool() == Some(true) {
            return ToolOutput::error("terminal_spawn: tty is not supported yet.");
        }
        let mut spec = SpawnSpec::new(command);
        spec.name = str_field(input, "name").map(Into::into);
        spec.cwd = str_field(input, "cwd").map(PathBuf::from);
        let env: BTreeMap<String, String> = input["env"]
            .as_object()
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        // Model-supplied: allowlist only (a variable can inject code or redirect the toolchain).
        if let Some((k, why)) = env.iter().find_map(|(k, v)| {
            env_refusal(k)
                .or_else(|| v.contains('\0').then_some("value contains NUL"))
                .map(|w| (k, w))
        }) {
            return ToolOutput::error(format!(
                "terminal_spawn: env variable {k:?} is not allowed ({why}). Allowed: {}.",
                ENV_ALLOWED_DOC
            ));
        }
        spec.env = env;
        if let Some(r) = input.get("ready") {
            spec.ready = Some(match r["kind"].as_str() {
                Some("port") => match r["port"]
                    .as_i64()
                    .and_then(|p| u16::try_from(p).ok())
                    .filter(|p| *p > 0)
                {
                    Some(p) => Readiness::Port(p),
                    None => {
                        return ToolOutput::error("terminal_spawn: ready.port must be 1-65535.");
                    }
                },
                Some("http") => match str_field(r, "url").filter(|u| u.starts_with("http://")) {
                    Some(u) => {
                        if let Err(why) = synthesis_terminal::validate_probe_url(u) {
                            return ToolOutput::error(format!("terminal_spawn: ready.url {why}."));
                        }
                        Readiness::Http(u.into())
                    }
                    None => {
                        return ToolOutput::error(
                            "terminal_spawn: ready.url must be a plain http:// URL.",
                        );
                    }
                },
                _ => match str_field(r, "pattern") {
                    Some(p) => Readiness::LogPattern(p.into()),
                    None => {
                        return ToolOutput::error(
                            "terminal_spawn: ready.pattern is required for kind \"log\".",
                        );
                    }
                },
            });
            if let Some(t) = r["timeout_ms"].as_u64() {
                spec.ready_timeout = Duration::from_millis(t.clamp(1, MAX_TIMEOUT_MS as u64));
            }
        }
        match start(sh, ctx, "terminal_spawn", spec).await {
            Ok((h, st)) => ToolOutput::text(
                json!({"process_id": h.process_id, "pid": h.pid, "status": status_json(&st)["status"], "ready": h.ready, "log_cursor": h.log_cursor}).to_string(),
            )
            .with_meta(json!({"process_id": h.process_id, "pid": h.pid, "artifact_path": h.log_path, "shell_kind": kind(sh)})),
            Err(e) => e,
        }
    })
}

pub(crate) fn logs<'a>(
    sh: &'a Shared,
    input: &'a Value,
    ctx: &'a ToolContext,
) -> BoxFuture<'a, ToolOutput> {
    Box::pin(async move {
        let id = match str_arg(input, "terminal_logs", "process_id") {
            Ok(i) => i,
            Err(e) => return e,
        };
        sh.session(ctx);
        let max = input["max_bytes"]
            .as_u64()
            .unwrap_or(MAX_OUTPUT as u64)
            .clamp(1, MAX_LOG_READ) as usize;
        match read(
            sh,
            ctx,
            "terminal_logs",
            id,
            str_field(input, "cursor"),
            str_field(input, "filter"),
            max,
        ) {
            Ok((data, cursor, st, _)) => {
                ToolOutput::text(log_text(&data, id, &st, &format!(", cursor {cursor}"))).with_meta(
                    json!({"cursor": cursor, "process": st.to_event_info(), "running": st.running}),
                )
            }
            Err(e) => e,
        }
    })
}

pub(crate) fn signal<'a>(
    sh: &'a Shared,
    input: &'a Value,
    ctx: &'a ToolContext,
) -> BoxFuture<'a, ToolOutput> {
    Box::pin(async move {
        let id = match str_arg(input, "terminal_signal", "process_id") {
            Ok(i) => i,
            Err(e) => return e,
        };
        let sig = match input["signal"].as_str() {
            Some("int") => Signal::Int,
            Some("term") => Signal::Term,
            _ => Signal::Kill,
        };
        match kill(sh, ctx, "terminal_signal", id, sig).await {
            Ok(st) => ToolOutput::text(status_json(&st).to_string()).with_meta(proc_meta(&st)),
            Err(e) => e,
        }
    })
}

pub(crate) fn status<'a>(
    sh: &'a Shared,
    input: &'a Value,
    ctx: &'a ToolContext,
) -> BoxFuture<'a, ToolOutput> {
    Box::pin(async move {
        sh.session(ctx);
        let r = match str_field(input, "process_id") {
            Some(id) => sh
                .rt
                .get_status(&ctx.session_id, id)
                .map(|s| status_json(&s))
                .map_err(|e| unknown("terminal_status", id, &e)),
            None => sh
                .rt
                .list(&ctx.session_id)
                .map(|l| Value::Array(l.iter().map(status_json).collect()))
                .map_err(|e| ToolOutput::error(format!("terminal_status: {e}"))),
        };
        match r {
            Ok(v) => ToolOutput::text(v.to_string()),
            Err(e) => e,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::sandbox_hint;
    use std::path::Path;

    #[test]
    fn sandbox_hint_names_path_and_flag() {
        let o = "error: could not read settings file: '/Users/u/.rustup/settings.toml': Operation not permitted (os error 1)";
        let h = sandbox_hint(o, Path::new("/Users/u/proj")).unwrap();
        assert!(
            h.contains("--sandbox-read /Users/u/.rustup/settings.toml"),
            "{h}"
        );
        assert!(h.contains("Do NOT suggest Full Disk Access"));
        assert!(
            sandbox_hint(
                "cat: /Users/u/proj/x: Permission denied",
                Path::new("/Users/u/proj")
            )
            .is_none()
        );
        assert!(sandbox_hint("all fine", Path::new("/w")).is_none());
    }
}
