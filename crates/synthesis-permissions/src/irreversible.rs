//! Code-level table of irreversible EXTERNAL actions (publish, push, deploy, remote shells,
//! cloud writes, network writes, download-and-run). A hit asks (headless: denied) in every mode,
//! bypassPermissions and the auto judge included, unless a USER-level allow rule names the action
//! (the hit's `key` must appear in the rule). Force pushes are denied outright unless a user rule
//! names `--force`. Found with the shell parser: compound commands, env prefixes, `sudo`,
//! `xargs`, `command`, `npx`/`dlx`/`setsid`/`flock`..., `bash -c`, `$(...)`, quoting, a shell fed
//! from stdin, and the recipes of `make`/`just`/`npm run`.
//! shortcut: the table is a list of known tools, not a proof; an unknown tool, or a program that
//! pushes by itself (`python deploy.py`), is for the sandbox (network denied) and `text_hits` on
//! agent-written files; upgrade by adding the tool here.

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::bash::{self, Words};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// Text a user rule must contain to lift the hit (`git push`, `npm publish`, `--force`).
    pub key: &'static str,
    pub reason: &'static str,
    /// Denied, not asked: only a user rule naming `--force` lifts it.
    pub force: bool,
}

const fn hit(key: &'static str, reason: &'static str) -> Hit {
    Hit {
        key,
        reason,
        force: false,
    }
}

fn base(w: &str) -> &str {
    w.rsplit(['/', '\\']).next().unwrap_or(w)
}

fn has(args: &[String], any: &[&str]) -> bool {
    args.iter().any(|a| any.contains(&a.as_str()))
}

/// First non-flag word of `args` and what follows it.
fn sub(args: &[String]) -> Option<(&str, &[String])> {
    let i = args.iter().position(|a| !a.starts_with('-'))?;
    Some((args[i].as_str(), &args[i + 1..]))
}

/// git config keys whose value is run as a program.
const EXEC_KEYS: &[&str] = &[
    "core.fsmonitor",
    "core.sshcommand",
    "core.hookspath",
    "credential.helper",
    "core.pager",
    "core.editor",
    "diff.external",
    "gpg.program",
    "core.askpass",
    "sequence.editor",
];

fn git(args: &[String], worktree: bool, out: &mut Vec<Hit>) {
    // global options before the subcommand; `-C`/`--git-dir`/`--work-tree` leave the session worktree
    let (mut i, mut elsewhere) = (0, false);
    while let Some(a) = args.get(i) {
        match a.as_str() {
            "-C" | "--git-dir" | "--work-tree" => {
                elsewhere = true;
                i += 2;
            }
            "-c" => {
                // an alias can be a push in disguise (`-c alias.p=push p`, `-c alias.x=!git push`)
                if let Some(v) = args.get(i + 1) {
                    let k = v.split('=').next().unwrap_or("").to_lowercase();
                    if k.starts_with("alias.") {
                        out.push(hit(
                            "git alias",
                            "git -c alias.* can hide a push or any command",
                        ));
                    } else if EXEC_KEYS.contains(&k.as_str()) {
                        out.push(hit("git config exec", "this git config key runs a program"));
                    }
                }
                i += 2;
            }
            "--namespace" | "--exec-path" => i += 2,
            x if x.starts_with("--git-dir=") || x.starts_with("--work-tree=") => {
                elsewhere = true;
                i += 1;
            }
            x if x.starts_with('-') => i += 1,
            _ => break,
        }
    }
    let Some(s) = args.get(i) else { return };
    let rest = &args[i + 1..];
    let flag = |f: &dyn Fn(&str) -> bool| rest.iter().any(|a| f(a));
    match s.as_str() {
        "push" => {
            let forced = flag(&|a| {
                a == "--force"
                    || a.starts_with("--force-")
                    || a == "--mirror"
                    || a == "--delete"
                    || a == "-d"
                    || (a.starts_with('-') && !a.starts_with("--") && a[1..].contains('f'))
                    || (a.starts_with('+') && a.len() > 1)
                    || a.starts_with(':')
                    || a.contains(":+")
            });
            out.push(if forced {
                Hit {
                    key: "--force",
                    reason: "git push --force / forced refspec / --mirror / --delete rewrites or deletes remote history",
                    force: true,
                }
            } else {
                hit(
                    "git push",
                    "git push publishes commits to a remote (pushes to main/master/the default branch included)",
                )
            });
        }
        "send-pack" | "http-push" => out.push(hit("git push", "sends objects to a remote")),
        "lfs" if has(rest, &["push"]) => {
            out.push(hit("git push", "git lfs push uploads to a remote"))
        }
        "svn" if has(rest, &["dcommit", "commit"]) => {
            out.push(hit("svn commit", "commits to a remote svn repository"));
        }
        "config" if rest.iter().any(|a| a.starts_with("alias.")) => {
            out.push(hit("git alias", "an alias can hide a push or any command"));
        }
        "config"
            if rest
                .iter()
                .any(|a| EXEC_KEYS.contains(&a.to_lowercase().as_str())) =>
        {
            out.push(hit("git config exec", "this git config key runs a program"));
        }
        "reset" if flag(&|a| a == "--hard") && (!worktree || elsewhere) => out.push(hit(
            "git reset --hard",
            "git reset --hard outside the session worktree discards work",
        )),
        "clean"
            if (!worktree || elsewhere)
                && flag(&|a| {
                    a == "--force"
                        || (a.starts_with('-') && !a.starts_with("--") && a.contains('f'))
                }) =>
        {
            out.push(hit(
                "git clean",
                "git clean -f deletes untracked files outside the session worktree",
            ));
        }
        _ => {}
    }
}

/// `(program, key, words any of which makes the call a hit)`; any word, not the first, so
/// `npm --registry x publish`, `kubectl -n x delete` and `docker --context c push` are seen.
const WORDS: &[(&str, &str, &str, &[&str])] = &[
    (
        "npm",
        "npm publish",
        "publishes a package",
        &[
            "publish",
            "unpublish",
            "deprecate",
            "dist-tag",
            "access",
            "owner",
            "token",
        ],
    ),
    (
        "yarn",
        "yarn publish",
        "publishes a package",
        &["publish", "npm"],
    ),
    ("pnpm", "pnpm publish", "publishes a package", &["publish"]),
    ("bun", "bun publish", "publishes a package", &["publish"]),
    (
        "cargo",
        "cargo publish",
        "publishes or yanks a crate",
        &["publish", "yank", "owner"],
    ),
    ("twine", "twine upload", "uploads a package", &["upload"]),
    ("uv", "uv publish", "publishes a package", &["publish"]),
    (
        "poetry",
        "poetry publish",
        "publishes a package",
        &["publish"],
    ),
    ("gem", "gem push", "publishes a gem", &["push", "yank"]),
    (
        "svn",
        "svn commit",
        "commits to a remote svn repository",
        &[
            "dcommit", "commit", "import", "delete", "mkdir", "copy", "move",
        ],
    ),
    ("mvn", "mvn deploy", "deploys artifacts", &["deploy"]),
    ("mvnw", "mvn deploy", "deploys artifacts", &["deploy"]),
    (
        "gradle",
        "gradle publish",
        "publishes artifacts",
        &["publish", "publishToMavenRepository"],
    ),
    (
        "gradlew",
        "gradle publish",
        "publishes artifacts",
        &["publish", "publishToMavenRepository"],
    ),
    (
        "docker",
        "docker push",
        "pushes an image",
        &["push", "--push"],
    ),
    (
        "podman",
        "docker push",
        "pushes an image",
        &["push", "--push"],
    ),
    (
        "terraform",
        "terraform",
        "changes real infrastructure",
        &[
            "apply",
            "destroy",
            "import",
            "state",
            "taint",
            "untaint",
            "force-unlock",
            "-auto-approve",
        ],
    ),
    (
        "tofu",
        "terraform",
        "changes real infrastructure",
        &[
            "apply",
            "destroy",
            "import",
            "state",
            "taint",
            "untaint",
            "force-unlock",
            "-auto-approve",
        ],
    ),
    (
        "kubectl",
        "kubectl",
        "changes or enters a live cluster",
        &[
            "apply",
            "delete",
            "replace",
            "patch",
            "create",
            "scale",
            "drain",
            "cordon",
            "taint",
            "edit",
            "exec",
            "run",
            "rollout",
            "label",
            "annotate",
            "cp",
            "set",
            "port-forward",
            "expose",
            "autoscale",
            "debug",
            "attach",
        ],
    ),
    (
        "oc",
        "kubectl",
        "changes or enters a live cluster",
        &[
            "apply",
            "delete",
            "replace",
            "patch",
            "create",
            "scale",
            "exec",
            "run",
            "rollout",
            "label",
            "annotate",
            "cp",
            "set",
            "port-forward",
        ],
    ),
    (
        "helm",
        "helm",
        "changes a live cluster or registry",
        &[
            "install",
            "upgrade",
            "uninstall",
            "rollback",
            "delete",
            "push",
        ],
    ),
    (
        "cdk",
        "cdk",
        "deploys cloud infrastructure",
        &["deploy", "destroy", "bootstrap"],
    ),
    (
        "sam",
        "sam",
        "deploys cloud infrastructure",
        &["deploy", "publish", "delete", "sync"],
    ),
    (
        "firebase",
        "firebase deploy",
        "deploys to Firebase",
        &["deploy"],
    ),
    (
        "fly",
        "fly",
        "changes a Fly.io app",
        &[
            "deploy", "launch", "destroy", "secrets", "scale", "machine", "volumes", "ips", "certs",
        ],
    ),
    (
        "flyctl",
        "fly",
        "changes a Fly.io app",
        &[
            "deploy", "launch", "destroy", "secrets", "scale", "machine", "volumes", "ips", "certs",
        ],
    ),
];

/// Programs that act on the outside unless called with a read-only word only.
const UNLESS_READ: &[(&str, &[&str])] = &[
    (
        "pulumi",
        &["preview", "version", "whoami", "about", "--help", "-h"],
    ),
    (
        "vercel",
        &[
            "ls",
            "list",
            "whoami",
            "inspect",
            "logs",
            "help",
            "--help",
            "-h",
            "--version",
            "-v",
        ],
    ),
    ("netlify", &["status", "help", "--help", "-h", "--version"]),
    (
        "wrangler",
        &[
            "whoami",
            "dev",
            "tail",
            "types",
            "--help",
            "-h",
            "--version",
            "-v",
        ],
    ),
    (
        "ansible-playbook",
        &[
            "--check",
            "-C",
            "--syntax-check",
            "--list-tasks",
            "--list-hosts",
            "--help",
            "-h",
        ],
    ),
];

/// `aws`/`gcloud`/`az`: a call is a read only when a read verb is among its command words, no
/// word is a mutating verb, and no operand is a remote URL it could write to. Unknown asks.
const READ_VERBS: &[&str] = &[
    "describe",
    "list",
    "ls",
    "get",
    "head",
    "lookup",
    "search",
    "show",
    "validate",
    "wait",
    "help",
    "version",
    "whoami",
    "info",
    "batch-get",
    "export-info",
];
const MUT_VERBS: &[&str] = &[
    "cp",
    "mv",
    "rm",
    "rb",
    "mb",
    "sync",
    "put",
    "delete",
    "create",
    "update",
    "add",
    "remove",
    "set",
    "patch",
    "deploy",
    "apply",
    "start",
    "stop",
    "restart",
    "reset",
    "import",
    "upload",
    "publish",
    "send",
    "terminate",
    "run",
    "invoke",
    "attach",
    "detach",
    "modify",
    "register",
    "deregister",
    "tag",
    "untag",
    "enable",
    "disable",
    "revoke",
    "authorize",
    "associate",
    "disassociate",
    "release",
    "allocate",
    "cancel",
    "copy",
    "restore",
    "execute",
    "push",
    "write",
    "insert",
    "drop",
    "truncate",
    "grant",
    "reboot",
    "resize",
    "scale",
    "move",
    "rename",
    "login",
    "configure",
    "assume-role",
    "ssh",
    "scp",
    "submit",
    "trigger",
    "write-log",
];

fn verb_is(tok: &str, set: &[&str]) -> bool {
    set.iter()
        .any(|v| tok == *v || tok.strip_prefix(v).is_some_and(|r| r.starts_with('-')))
}

static TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9-]*$").unwrap());
const AWS_BOOL: &[&str] = &[
    "--debug",
    "--no-paginate",
    "--no-sign-request",
    "--no-verify-ssl",
    "--no-cli-pager",
    "--no-cli-auto-prompt",
];

fn cloud(name: &'static str, args: &[String], out: &mut Vec<Hit>) {
    let ask = |out: &mut Vec<Hit>| {
        out.push(hit(
            name,
            "cloud CLI call that is not a recognised read-only operation",
        ));
    };
    if args
        .iter()
        .any(|a| matches!(a.as_str(), "--version" | "--help" | "-h" | "-v"))
    {
        return;
    }
    // a URL anywhere (`--endpoint-url https://evil`) sends the call somewhere else
    if args
        .iter()
        .any(|a| a.contains("http://") || a.contains("https://"))
    {
        return ask(out);
    }
    // leading global flags (`--region x`, `--profile=y`)
    let mut a = args;
    while let Some(f) = a.first().filter(|f| f.starts_with('-')) {
        let skip = if f.contains('=') || AWS_BOOL.contains(&f.as_str()) {
            1
        } else {
            2
        };
        a = &a[skip.min(a.len())..];
    }
    // the command words: the leading run of command-shaped tokens (`storage cp`, not `scan.json`)
    let run: Vec<&str> = a
        .iter()
        .take_while(|w| TOKEN.is_match(w))
        .map(String::as_str)
        .collect();
    let operands: Vec<&String> = a.iter().filter(|w| !w.starts_with('-')).collect();
    let remote = |w: &str| {
        ["s3://", "gs://", "ftp://", "abfs://", "wasbs://", "az://"]
            .iter()
            .any(|p| w.starts_with(p))
    };
    // aws: `service operation`; a download (remote source, local destination) is not a write
    if name == "aws" && run.first() == Some(&"s3") && matches!(run.get(1), Some(&"cp" | &"sync")) {
        let ops: Vec<&&String> = operands.iter().skip(2).collect();
        if let [src, dst] = ops.as_slice()
            && remote(src)
            && !dst.contains("://")
        {
            return;
        }
        return ask(out);
    }
    let any_remote_operand = a.iter().any(|w| remote(w));
    let mutating = run.iter().any(|t| verb_is(t, MUT_VERBS));
    let read = run.iter().skip(1).any(|t| verb_is(t, READ_VERBS))
        || (run.len() == 1 && verb_is(run[0], &["help", "version", "whoami", "info"]));
    // `s3 ls` is a read: `ls` is a read verb, `cp`/`mv`/`sync`/`rm` are mutating; remote operands
    // matter only to a call that is not already mutating
    if read && !mutating && !(any_remote_operand && !run.contains(&"ls")) {
        return;
    }
    ask(out);
}

static URL_ARG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[a-z][a-z0-9+.-]*://|git\+|git@|github:|gitlab:|bitbucket:)|^[^/\s]+@[^/\s]+:")
        .unwrap()
});
static REMOTE_OPERAND: LazyLock<Regex> = LazyLock::new(|| {
    // `host:path`, `user@host:path`, `rsync://`; not a Windows drive (`C:\x`) or a local path
    Regex::new(r"^(?:[a-z][a-z0-9+.-]*://|[^/\\\s:]{2,}:)").unwrap()
});

fn is_shell(name: &str) -> bool {
    bash::SHELLS.contains(&name) || name == "fish"
}

fn is_interpreter(name: &str) -> bool {
    let n = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    is_shell(n)
        || matches!(
            n,
            "python" | "perl" | "ruby" | "node" | "php" | "pypy" | "deno" | "bun"
        )
}

/// `localhost`, `127.x`, `::1` (also in a URL or `host:port`).
fn local_host(w: &str) -> bool {
    use std::net::{Ipv4Addr, Ipv6Addr};
    let rest = w.split("://").last().unwrap_or(w);
    let auth = rest.split(['/', '?', '#']).next().unwrap_or("");
    // the host is after the LAST `@` (anything before it is userinfo)
    let auth = auth.rsplit('@').next().unwrap_or(auth);
    let host = if let Some(v6) = auth.strip_prefix('[') {
        // `[::1]:80`; text after `]` must be empty or a port
        match v6.split_once(']') {
            Some((h, tail))
                if tail.is_empty()
                    || tail
                        .strip_prefix(':')
                        .is_some_and(|p| p.bytes().all(|b| b.is_ascii_digit())) =>
            {
                h
            }
            _ => return false,
        }
    } else if auth.matches(':').count() > 1 {
        auth // bare IPv6 (`::1`)
    } else {
        auth.split(':').next().unwrap_or("")
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<Ipv4Addr>()
            .is_ok_and(|a| a.is_loopback() || a.is_unspecified())
        || host
            .parse::<Ipv6Addr>()
            .is_ok_and(|a| a.is_loopback() || a.is_unspecified())
}

/// The URLs / hosts among curl-like arguments, all local (and at least one).
fn only_local_targets(args: &[String]) -> bool {
    let targets: Vec<&String> = args
        .iter()
        .filter(|a| {
            !a.starts_with('-')
                && (a.contains("://")
                    || a.contains('.')
                    || a.contains(':')
                    || a.as_str() == "localhost")
        })
        .collect();
    !targets.is_empty() && targets.iter().all(|t| local_host(t))
}

fn curl_writes(args: &[String]) -> bool {
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-X" | "--request" => {
                if it
                    .peek()
                    .is_some_and(|m| !matches!(m.to_uppercase().as_str(), "GET" | "HEAD"))
                {
                    return true;
                }
            }
            "-d" | "--data" | "--data-binary" | "--data-raw" | "--data-urlencode"
            | "--data-ascii" | "--json" | "-F" | "--form" | "--form-string" | "-T"
            | "--upload-file" => return true,
            x if x.starts_with("--request=") => {
                if !matches!(x[10..].to_uppercase().as_str(), "GET" | "HEAD") {
                    return true;
                }
            }
            x if x.starts_with("--data")
                || x.starts_with("--form")
                || x.starts_with("--upload-file=")
                || x.starts_with("--json=") =>
            {
                return true;
            }
            x if x.starts_with("-X") && x.len() > 2 => {
                if !matches!(x[2..].to_uppercase().as_str(), "GET" | "HEAD") {
                    return true;
                }
            }
            // a short cluster: `-sXPOST`, `-sd`, `-sT`
            x if x.starts_with('-') && !x.starts_with("--") && x.len() > 2 => {
                let letters = &x[1..];
                if let Some(i) = letters.find(['X', 'd', 'F', 'T']) {
                    let c = letters.as_bytes()[i];
                    if c != b'X' {
                        return true;
                    }
                    let attached = &letters[i + 1..];
                    let m = if attached.is_empty() {
                        it.peek().map(|s| s.as_str()).unwrap_or("")
                    } else {
                        attached
                    };
                    if !matches!(m.to_uppercase().as_str(), "GET" | "HEAD") {
                        return true;
                    }
                }
            }
            x if x.starts_with("-d") && x.len() > 2 => return true,
            _ => {}
        }
    }
    false
}

fn gh_api(rest: &[String], out: &mut Vec<Hit>) {
    let mut method: Option<String> = None;
    let mut implicit_post = false;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-X" | "--method" => method = it.next().map(|m| m.to_uppercase()),
            "-f" | "-F" | "--field" | "--raw-field" | "--input" => implicit_post = true,
            x if x.starts_with("--method=") => method = Some(x[9..].to_uppercase()),
            x if x.starts_with("-X") && x.len() > 2 => {
                method = Some(x[2..].trim_start_matches('=').to_uppercase())
            }
            x if x.starts_with("--field=")
                || x.starts_with("--raw-field=")
                || x.starts_with("--input=") =>
            {
                implicit_post = true
            }
            x if x.to_lowercase().contains("mutation") => implicit_post = true,
            _ => {}
        }
    }
    let write = match method.as_deref() {
        Some("GET" | "HEAD") => false,
        Some(_) => true,
        None => implicit_post,
    };
    if write {
        out.push(hit(
            "gh api",
            "gh api with a write method, a field, an input or a mutation writes to GitHub",
        ));
    }
}

/// Programs that send data to a host with no URL syntax.
const RAW_NET: &[&str] = &["nc", "ncat", "netcat", "socat", "telnet"];

/// Wrappers to look through before matching (`name`, number of leading positional words it eats
/// before the wrapped command; flags are skipped). `None` = not a wrapper.
fn unwrap_more(u: &Words) -> Option<Words> {
    let first = base(u.first()?).to_string();
    let rest = &u[1..];
    let skip_flags = |r: &[String], valued: &[&str]| -> usize {
        let mut i = 0;
        while let Some(a) = r
            .get(i)
            .filter(|a| a.starts_with('-') && a.as_str() != "--")
        {
            i += if valued.contains(&a.as_str()) { 2 } else { 1 };
        }
        i + usize::from(r.get(i).is_some_and(|a| a == "--"))
    };
    let take = |n: usize| (n <= rest.len() && n < rest.len()).then(|| rest[n..].to_vec());
    match first.as_str() {
        "npx" | "pnpx" | "bunx" | "uvx" | "setsid" | "time" => {
            let n = skip_flags(rest, &["-p", "--package", "--from"]);
            take(n)
        }
        "ionice" => {
            let n = skip_flags(rest, &["-c", "-n", "-p", "--class", "--classdata"]);
            take(n)
        }
        "chrt" | "taskset" => {
            let n = skip_flags(rest, &[]);
            take(n + 1)
        }
        "flock" => {
            if let Some(i) = rest.iter().position(|a| a == "-c") {
                return rest
                    .get(i + 1)
                    .map(|c| vec!["sh".into(), "-c".into(), c.clone()]);
            }
            let n = skip_flags(rest, &["-w", "-E", "--timeout", "--conflict-exit-code"]);
            take(n + 1)
        }
        "npm" | "pnpm" | "yarn" | "bun" => {
            let (i, s) = rest.iter().enumerate().find(|(_, a)| !a.starts_with('-'))?;
            (matches!(s.as_str(), "exec" | "dlx") || (first == "bun" && s == "x")).then(|| {
                let r = &rest[i + 1..];
                let n = skip_flags(r, &["-p", "--package"]);
                r[n.min(r.len())..].to_vec()
            })
        }
        _ => None,
    }
    .filter(|w| !w.is_empty())
}

/// Total recipe / script lines one scan reads (a bigger one cannot be inspected: it asks).
const MAX_LINES: usize = 5000;

/// What one `scan` has already read: a (file, target) is scanned once, however many ways reach it.
#[derive(Default)]
struct Ctx {
    seen: std::collections::HashSet<String>,
    lines: usize,
}

struct Recipe {
    names: Vec<String>,
    prereqs: Vec<String>,
    body: Vec<String>,
}

/// Targets of a Makefile / justfile: backslash continuations joined, `target: ; cmd` inline
/// recipes, `target: a b` prerequisites. Variables and `.PHONY`-style lines have no recipe.
fn parse_recipes(text: &str, just: bool) -> Vec<Recipe> {
    static HEAD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(@?[A-Za-z0-9_./%$()@-][^:=#]*?)\s*:(?:[^=]|$)(.*)$").unwrap()
    });
    let text = text.replace("\\\r\n", " ").replace("\\\n", " ");
    let mut out: Vec<Recipe> = vec![];
    for l in text.lines() {
        let indented = l.starts_with(['\t']) || (just && l.starts_with(' '));
        if indented {
            if let Some(r) = out.last_mut() {
                let t = l.trim().trim_start_matches(['@', '-', '+']).trim();
                if !t.is_empty() && !t.starts_with('#') {
                    r.body.push(t.to_string());
                }
            }
        } else if let Some(c) = HEAD.captures(l) {
            let head = c[1].trim();
            let names: Vec<String> = if just {
                head.split_whitespace()
                    .take(1)
                    .map(|n| n.trim_start_matches('@').to_string())
                    .collect()
            } else {
                head.split_whitespace().map(String::from).collect()
            };
            let rhs = c.get(2).map_or("", |m| m.as_str());
            let (deps, inline) = rhs.split_once(';').unwrap_or((rhs, ""));
            let mut body = vec![];
            if !inline.trim().is_empty() {
                body.push(
                    inline
                        .trim()
                        .trim_start_matches(['@', '-', '+'])
                        .to_string(),
                );
            }
            out.push(Recipe {
                names,
                prereqs: deps
                    .split_whitespace()
                    .filter(|d| *d != "|")
                    .map(String::from)
                    .collect(),
                body,
            });
        } else if !l.trim().is_empty() && !l.starts_with('#') {
            // a non-recipe line ends the current recipe
            out.push(Recipe {
                names: vec![],
                prereqs: vec![],
                body: vec![],
            });
        }
    }
    out
}

/// Value of the first of `flags` in `args` (`-f x`, `--file=x`, `-fx` for a one-letter flag).
fn flag_value<'a>(args: &'a [String], flags: &[&str]) -> Option<&'a str> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        for f in flags {
            if a == f {
                return it.next().map(String::as_str);
            }
            if let Some(v) = a.strip_prefix(&format!("{f}=")) {
                return Some(v);
            }
            if f.len() == 2 && !f.starts_with("--") && a.len() > 2 && a.starts_with(f) {
                return Some(&a[2..]);
            }
        }
    }
    None
}

/// The commands to scan for `make`/`just`/`npm run NAME`, with the directory they run in. Only
/// the named targets and their prerequisites (make: the first target when none is named).
/// `Err(())` = too much to read.
fn project_scripts(
    first: &str,
    args: &[String],
    cwd: &Path,
    ctx: &mut Ctx,
) -> Result<Vec<(String, std::path::PathBuf)>, ()> {
    let read = |p: &Path| {
        std::fs::metadata(p)
            .ok()
            .filter(|m| m.len() <= 256 * 1024)
            .and_then(|_| std::fs::read_to_string(p).ok())
    };
    let mut out = vec![];
    match first {
        "make" | "gmake" | "just" => {
            let just = first == "just";
            let dir = flag_value(
                args,
                if just {
                    &["--working-directory", "-d"]
                } else {
                    &["-C", "--directory"]
                },
            )
            .map_or_else(|| cwd.to_path_buf(), |d| cwd.join(d));
            let file = flag_value(
                args,
                if just {
                    &["--justfile", "-f"]
                } else {
                    &["-f", "--file", "--makefile"]
                },
            );
            let path = match file {
                Some(f) => dir.join(f),
                None => {
                    let names: &[&str] = if just {
                        &["justfile", "Justfile", ".justfile"]
                    } else {
                        &["GNUmakefile", "Makefile", "makefile"]
                    };
                    match names.iter().map(|n| dir.join(n)).find(|p| p.is_file()) {
                        Some(p) => p,
                        None => return Ok(out),
                    }
                }
            };
            let Some(text) = read(&path) else {
                return Ok(out);
            };
            let recipes = parse_recipes(&text, just);
            // words that are not flags, flag values or VAR=value
            let mut wanted: Vec<String> = vec![];
            let mut skip = false;
            for a in args {
                if skip {
                    skip = false;
                } else if matches!(
                    a.as_str(),
                    "-f" | "-C"
                        | "-d"
                        | "--file"
                        | "--directory"
                        | "--justfile"
                        | "--working-directory"
                        | "--makefile"
                ) {
                    skip = true;
                } else if !a.starts_with('-') && !a.contains('=') {
                    wanted.push(a.clone());
                }
            }
            if wanted.is_empty()
                && let Some(n) = recipes
                    .iter()
                    .flat_map(|r| r.names.iter())
                    .find(|n| !n.starts_with('.'))
            {
                wanted.push(n.clone());
            }
            let mut stack = wanted;
            while let Some(t) = stack.pop() {
                if !ctx.seen.insert(format!("{}\0{t}", path.display())) {
                    continue;
                }
                for r in recipes.iter().filter(|r| r.names.contains(&t)) {
                    ctx.lines += r.body.len() + 1;
                    if ctx.lines > MAX_LINES {
                        return Err(());
                    }
                    stack.extend(r.prereqs.iter().cloned());
                    out.extend(r.body.iter().map(|l| {
                        (
                            l.replace("$(MAKE)", "make").replace("${MAKE}", "make"),
                            dir.clone(),
                        )
                    }));
                }
            }
        }
        "npm" | "pnpm" | "yarn" | "bun" => {
            let dir = flag_value(args, &["--prefix", "--cwd", "-C"])
                .map_or_else(|| cwd.to_path_buf(), |d| cwd.join(d));
            // positional words, with the value of --prefix / --cwd / -C left out
            let mut skip = false;
            let words: Vec<&String> = args
                .iter()
                .filter(|a| {
                    if std::mem::take(&mut skip) {
                        return false;
                    }
                    if matches!(a.as_str(), "--prefix" | "--cwd" | "-C") {
                        skip = true;
                        return false;
                    }
                    !a.starts_with('-')
                })
                .collect();
            let name = match words.as_slice() {
                [r, n, ..] if matches!(r.as_str(), "run" | "run-script" | "r") => n.as_str(),
                [n, ..]
                    if first == "yarn"
                        && !matches!(n.as_str(), "run" | "install" | "add" | "publish") =>
                {
                    n.as_str()
                }
                [n, ..] if matches!(n.as_str(), "test" | "start" | "stop" | "restart") => {
                    n.as_str()
                }
                _ => return Ok(out),
            };
            let pj = dir.join("package.json");
            let Some(pkg) =
                read(&pj).and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            else {
                return Ok(out);
            };
            if ctx.seen.insert(format!("{}\0{name}", pj.display())) {
                for p in ["pre", "", "post"] {
                    if let Some(v) = pkg
                        .pointer(&format!("/scripts/{p}{name}"))
                        .and_then(|v| v.as_str())
                    {
                        ctx.lines += 1;
                        if ctx.lines > MAX_LINES {
                            return Err(());
                        }
                        out.push((v.to_string(), dir.clone()));
                    }
                }
            }
        }
        _ => {}
    }
    Ok(out)
}

/// Phrases in code that names a table action (inline `python -c`, agent-written files). Matched
/// on the text, so `['git','push']`, `"npm publish"` and `requests.post(` are all seen.
pub fn text_hits(text: &str) -> bool {
    static R: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"(?i)\b(?:git\W+(?:push|send-pack)",
            r"|(?:npm|yarn|pnpm|bun|cargo|twine|gem|poetry|uv)\W+(?:publish|push|upload|yank)",
            r"|docker\W+(?:buildx\W+)?(?:push|build\W+.*--push)",
            r"|(?:kubectl|oc)\W+(?:apply|delete|exec|run|patch|replace|create)",
            r"|(?:terraform|tofu)\W+(?:apply|destroy)",
            r"|helm\W+(?:install|upgrade|uninstall|push)",
            r"|gh\W+(?:release|pr\W+merge|repo\W+delete|api)",
            r"|(?:ssh|scp|sftp|rsync|nc|ncat|netcat|socat|telnet|mosh)\b",
            r"|curl\W+.*-(?:X|d|F|T)\b",
            r"|curl\W+.*--(?:data|form|upload|request)",
            r"|aws\b|gcloud\b",
            r"|requests\.(?:post|put|patch|delete)|urllib\.request|urlopen|http\.client|smtplib|ftplib|paramiko|socket\.socket\s*\(",
            r"|(?:pulumi|cdk|sam|firebase|vercel|wrangler|fly|flyctl)\W+(?:up|deploy|destroy|publish))",
        ))
        .unwrap()
    });
    R.is_match(text)
}

/// Every hit in `cmd`. `worktree`: the session runs in its own git worktree. `cwd`: where
/// `make`/`just`/`npm run` find their recipes.
pub fn scan(cmd: &str, worktree: bool, cwd: &Path) -> Vec<Hit> {
    let mut out = Vec::new();
    scan_into(cmd, worktree, cwd, 0, &mut Ctx::default(), &mut out);
    out
}

fn scan_into(cmd: &str, worktree: bool, cwd: &Path, depth: u8, ctx: &mut Ctx, out: &mut Vec<Hit>) {
    if depth > 3 {
        out.push(hit("nested scripts", "scripts nest too deeply to read"));
        return;
    }
    let segs = match bash::deep_segments(cmd, 0) {
        Some(s) => s,
        None => {
            // unbalanced / too deep: scan what the lenient splitter finds, and ask if it names a tool
            static NAMES: LazyLock<Regex> = LazyLock::new(|| {
                Regex::new(r"\b(?:git|npm|yarn|pnpm|bun|cargo|docker|podman|terraform|tofu|kubectl|helm|gh|aws|gcloud|az|ssh|scp|sftp|rsync|curl|wget|pip3?|twine|nc|ncat|socat)\b").unwrap()
            });
            if NAMES.is_match(cmd) {
                out.push(hit(
                    "unparsable",
                    "the command cannot be parsed and names a tool that acts externally",
                ));
            }
            bash::lenient_segments(cmd)
        }
    };
    let (mut downloads, mut runs) = (false, false);
    for seg in &segs {
        // the segment itself AND what it wraps (npx/dlx/setsid/ionice/flock ...), bounded
        let mut variants: Vec<Words> = vec![bash::unwrap_words(seg, true)];
        for _ in 0..6 {
            match unwrap_more(variants.last().unwrap()) {
                Some(w) => variants.push(bash::unwrap_words(&w, true)),
                None => break,
            }
        }
        for u in variants {
            let Some(first) = u.first() else { continue };
            if first.contains(['$', '`']) {
                out.push(hit(
                    "dynamic command",
                    "the command name is built at run time",
                ));
                continue;
            }
            let name = base(first);
            let args = &u[1..];
            runs |= is_interpreter(name);
            match name {
                "git" => git(args, worktree, out),
                "gh" => match sub(args) {
                    Some(("release", _)) => {
                        out.push(hit("gh release", "creates or changes a public release"))
                    }
                    Some(("pr", r)) if sub(r).is_some_and(|(s, _)| s == "merge") => {
                        out.push(hit("gh pr merge", "merges a pull request"));
                    }
                    Some(("repo", r))
                        if sub(r)
                            .is_some_and(|(s, _)| matches!(s, "delete" | "archive" | "rename")) =>
                    {
                        out.push(hit("gh repo", "deletes or changes a repository"));
                    }
                    Some(("api", r)) => gh_api(r, out),
                    Some(("alias", r)) if has(r, &["set", "import"]) => {
                        out.push(hit("gh alias", "a gh alias can run any shell command"));
                    }
                    _ => {}
                },
                "aws" => cloud("aws", args, out),
                "gcloud" => cloud("gcloud", args, out),
                "az" => cloud("az", args, out),
                "ssh" | "sftp" | "mosh" | "ssh-copy-id" => {
                    out.push(hit(name_key(name), "opens a connection to a remote host"))
                }
                "scp" | "rsync"
                    if args
                        .iter()
                        .filter(|a| !a.starts_with('-'))
                        .any(|a| REMOTE_OPERAND.is_match(a)) =>
                {
                    out.push(hit(name_key(name), "copies to or from a remote host"));
                }
                "curl" => {
                    downloads = true;
                    if curl_writes(args) && !only_local_targets(args) {
                        out.push(hit("curl", "sends data to a remote host"));
                    }
                }
                "wget" => {
                    downloads = true;
                    let writes = args.iter().any(|a| {
                        a.starts_with("--post-")
                            || a.starts_with("--body-")
                            || (a.starts_with("--method=") && !a.to_uppercase().ends_with("=GET"))
                    });
                    if writes && !only_local_targets(args) {
                        out.push(hit("wget", "sends data to a remote host"));
                    }
                }
                n if RAW_NET.contains(&n) => {
                    // `nc -z localhost 5432`: the first host-like word decides
                    let host = args
                        .iter()
                        .find(|a| !a.starts_with('-') && a.parse::<u32>().is_err());
                    if !host.is_some_and(|h| local_host(h)) {
                        out.push(hit("nc", "opens a raw network connection"));
                    }
                }
                "openssl" if has(args, &["s_client", "s_server"]) => {
                    if !args.iter().any(|a| local_host(a)) {
                        out.push(hit(
                            "openssl s_client",
                            "opens a TLS connection to a remote host",
                        ));
                    }
                }
                "make" | "gmake" | "just" | "npm" | "pnpm" | "yarn" | "bun" => {
                    match project_scripts(name, args, cwd, ctx) {
                        Ok(lines) => {
                            for (line, dir) in lines {
                                scan_into(&line, worktree, &dir, depth + 1, ctx, out);
                            }
                        }
                        Err(()) => out.push(hit(
                            "large recipes",
                            "the recipes or scripts are too large to inspect",
                        )),
                    }
                }
                _ => {}
            }
            // table-driven programs (publish, deploy, apply ...)
            if let Some((_, key, why, words)) = WORDS.iter().find(|(p, ..)| *p == name)
                && has(args, words)
            {
                out.push(hit(key, why));
            }
            if let Some((_, reads)) = UNLESS_READ.iter().find(|(p, _)| *p == name)
                && !has(args, reads)
            {
                out.push(hit(
                    match name {
                        "ansible-playbook" => "ansible-playbook",
                        "pulumi" => "pulumi",
                        "vercel" => "vercel",
                        "netlify" => "netlify",
                        _ => "wrangler",
                    },
                    "deploys or changes remote infrastructure",
                ));
            }
            if name == "cargo"
                && has(args, &["install"])
                && args.iter().any(|a| a == "--git" || a.starts_with("--git="))
            {
                out.push(hit(
                    "install from url",
                    "installs a crate from a git remote",
                ));
            }
            if name == "docker" && has(args, &["compose"]) && has(args, &["push"]) {
                out.push(hit("docker push", "pushes images"));
            }
            // installs from a URL / git remote run that code's install scripts
            if matches!(name, "npm" | "yarn" | "pnpm" | "bun")
                && sub(args).is_some_and(|(s, _)| matches!(s, "install" | "i" | "add"))
                && args.iter().any(|a| URL_ARG.is_match(a))
            {
                out.push(hit(
                    "install from url",
                    "installs a package from a URL or git remote",
                ));
            }
            if matches!(name, "pip" | "pip3" | "uv" | "pipx" | "python" | "python3")
                && has(args, &["install", "add"])
                && args.iter().any(|a| URL_ARG.is_match(a))
                && (name.starts_with("pip") || name == "uv" || has(args, &["pip"]))
            {
                out.push(hit(
                    "install from url",
                    "installs a package from a URL or git remote",
                ));
            }
            // a shell fed from stdin (pipe, here-doc, here-string, `< file`, `-s`, `-`) runs text this
            // scan never sees
            if is_shell(name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.')) {
                let has_c = args.iter().any(|a| bash::is_dash_c(a));
                let redirect = seg.iter().any(|w| w.contains('<'));
                let operand = args
                    .iter()
                    .any(|a| !a.starts_with('-') && !a.contains('<') && a != "-");
                if !has_c && (redirect || !operand || has(args, &["-s", "-"])) {
                    out.push(hit("shell from stdin", "a shell reads commands from stdin (pipe or here-document) that cannot be inspected"));
                }
            }
            // interpreter one-liners that name a table action
            let lang = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
            if matches!(
                lang,
                "python" | "pypy" | "node" | "nodejs" | "bun" | "deno" | "perl" | "ruby" | "php"
            ) && args.iter().any(|a| {
                matches!(
                    a.as_str(),
                    "-c" | "-e" | "-p" | "-r" | "-E" | "--eval" | "--print"
                )
            }) && text_hits(&args.join(" "))
            {
                out.push(hit("inline code", "inline code names an external action"));
            }
        }
    }
    // download-and-run: a downloader next to a shell/interpreter (`curl x | sh`, `sh -c "$(curl x)"`)
    if downloads && runs {
        out.push(hit("curl | sh", "downloads code and runs it"));
    }
}

fn name_key(n: &str) -> &'static str {
    match n {
        "scp" => "scp",
        "rsync" => "rsync",
        "sftp" => "sftp",
        "mosh" => "mosh",
        "ssh-copy-id" => "ssh-copy-id",
        _ => "ssh",
    }
}
