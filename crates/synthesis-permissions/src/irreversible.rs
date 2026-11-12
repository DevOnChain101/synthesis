//! Code-level table of irreversible EXTERNAL actions (publish, push, deploy, remote shells,
//! cloud writes, download-and-run). A hit always asks (headless: denied) in every mode,
//! bypassPermissions and the auto judge included, unless a USER-level allow rule names the action
//! (the hit's `key` must appear in the rule). Force pushes are denied outright unless a user rule
//! names `--force`. Found with the shell parser: compound commands, env prefixes, sudo, xargs,
//! `command`, `bash -c`, `$(...)` and quoting are looked through.
//! shortcut: the table is a list of known tools, not a proof; an unknown tool or an interpreter
//! program that pushes (`python deploy.py`) is for the sandbox (no network) and the judge.

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

/// First non-flag word of `args` and what follows it.
fn sub(args: &[String]) -> Option<(&str, &[String])> {
    let i = args.iter().position(|a| !a.starts_with('-'))?;
    Some((args[i].as_str(), &args[i + 1..]))
}

fn git(args: &[String], worktree: bool, out: &mut Vec<Hit>) {
    // global options before the subcommand; `-C`/`--git-dir`/`--work-tree` leave the session worktree
    let (mut i, mut elsewhere) = (0, false);
    while let Some(a) = args.get(i) {
        match a.as_str() {
            "-C" | "--git-dir" | "--work-tree" => {
                elsewhere = true;
                i += 2;
            }
            "-c" | "--namespace" | "--exec-path" => i += 2,
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

/// Packages and registries: `publish` and friends.
fn publish(name: &str, args: &[String], out: &mut Vec<Hit>) {
    let (key, bad): (&'static str, &[&str]) = match name {
        "npm" => (
            "npm publish",
            &["publish", "unpublish", "deprecate", "dist-tag"],
        ),
        "yarn" => ("yarn publish", &["publish", "npm"]),
        "pnpm" => ("pnpm publish", &["publish"]),
        "bun" => ("bun publish", &["publish"]),
        "cargo" => ("cargo publish", &["publish", "yank"]),
        "twine" => ("twine upload", &["upload"]),
        "docker" | "podman" => ("docker push", &["push"]),
        _ => return,
    };
    // any word, not the first: `npm --registry x publish`, `docker --context c push`
    if args.iter().any(|a| bad.contains(&a.as_str())) {
        out.push(hit(
            key,
            "publishes an artifact to a public or shared registry",
        ));
    }
}

const K8S_WRITE: &[&str] = &[
    "apply", "delete", "replace", "patch", "create", "scale", "drain", "cordon", "taint", "edit",
];
const READ_VERBS: &[&str] = &[
    "list", "ls", "get", "describe", "show", "head", "lookup", "search", "query", "scan",
    "version", "help", "whoami", "info", "validate",
];

/// aws / gcloud / az: mutating unless the verb (last bare word before the first flag) is a read.
fn cloud(name: &str, args: &[String], out: &mut Vec<Hit>) {
    let mut a = args;
    // leading global flags (`--region x`, `--profile=y`)
    while let Some(f) = a.first().filter(|f| f.starts_with('-')) {
        a = &a[if f.contains('=') { 1 } else { 2.min(a.len()) }..];
    }
    let words: Vec<&String> = a
        .iter()
        .take_while(|w| !w.starts_with('-'))
        .filter(|w| !w.contains([':', '/', '=']))
        .take(4)
        .collect();
    let read = words
        .last()
        .is_some_and(|v| READ_VERBS.iter().any(|r| v.starts_with(r)));
    if !read {
        out.push(hit(
            match name {
                "aws" => "aws",
                "gcloud" => "gcloud",
                _ => "az",
            },
            "cloud CLI call that is not a recognised read-only verb",
        ));
    }
}

static URL_ARG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[a-z][a-z0-9+.-]*://|git\+|git@|github:|gitlab:|bitbucket:)|^[^/\s]+@[^/\s]+:")
        .unwrap()
});
static REMOTE_OPERAND: LazyLock<Regex> = LazyLock::new(|| {
    // `host:path`, `user@host:path`, `rsync://`; not a Windows drive (`C:\x`) or a local path
    Regex::new(r"^(?:[a-z][a-z0-9+.-]*://|[^/\\\s:]{2,}:)").unwrap()
});

fn is_shell_like(name: &str) -> bool {
    let n = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    bash::SHELLS.contains(&n) || matches!(n, "python" | "perl" | "ruby" | "node" | "php" | "sudo")
}

/// Every hit in `cmd`. `worktree`: the session runs in its own git worktree.
pub fn scan(cmd: &str, worktree: bool) -> Vec<Hit> {
    let mut out = Vec::new();
    let segs = match bash::deep_segments(cmd, 0) {
        Some(s) => s,
        None => {
            // unbalanced / too deep: scan what the lenient splitter finds, and ask if it names a tool
            static NAMES: LazyLock<Regex> = LazyLock::new(|| {
                Regex::new(r"\b(?:git|npm|yarn|pnpm|bun|cargo|docker|podman|terraform|tofu|kubectl|helm|gh|aws|gcloud|az|ssh|scp|sftp|rsync|curl|wget|pip3?|twine)\b").unwrap()
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
        let u: Words = bash::unwrap_words(seg, true);
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
        runs |= is_shell_like(name);
        match name {
            "git" => git(args, worktree, &mut out),
            "npm" | "yarn" | "pnpm" | "bun" | "cargo" | "twine" | "docker" | "podman" => {
                publish(name, args, &mut out);
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
            }
            "pip" | "pip3" | "uv" | "pipx" | "python" | "python3" => {
                let i = args.iter().position(|a| a == "install" || a == "add");
                if i.is_some()
                    && args.iter().any(|a| URL_ARG.is_match(a))
                    && (name.starts_with("pip") || name == "uv" || args.iter().any(|a| a == "pip"))
                {
                    out.push(hit(
                        "install from url",
                        "installs a package from a URL or git remote",
                    ));
                }
            }
            "terraform" | "tofu"
                if args
                    .iter()
                    .any(|a| matches!(a.as_str(), "apply" | "destroy")) =>
            {
                out.push(hit("terraform", "changes or destroys real infrastructure"));
            }
            "kubectl" | "oc" if args.iter().any(|a| K8S_WRITE.contains(&a.as_str())) => {
                out.push(hit("kubectl", "changes a live cluster"));
            }
            "helm"
                if args.iter().any(|a| {
                    matches!(
                        a.as_str(),
                        "install" | "upgrade" | "uninstall" | "rollback" | "delete"
                    )
                }) =>
            {
                out.push(hit("helm", "changes a live cluster"));
            }
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
                Some(("api", r))
                    if r.iter().enumerate().any(|(i, a)| {
                        matches!(a.as_str(), "-X" | "--method")
                            && r.get(i + 1).is_some_and(|m| !m.eq_ignore_ascii_case("GET"))
                            || a.starts_with("--method=") && !a.eq_ignore_ascii_case("--method=GET")
                    }) =>
                {
                    out.push(hit(
                        "gh api",
                        "gh api with a non-GET method writes to GitHub",
                    ));
                }
                _ => {}
            },
            "aws" | "gcloud" | "az" => cloud(name, args, &mut out),
            "ssh" | "sftp" | "mosh" | "ssh-copy-id" => {
                out.push(hit(name_key(name), "opens a connection to a remote host"))
            }
            "scp" | "rsync"
                if args.iter().filter(|a| !a.starts_with('-')).any(|a| {
                    REMOTE_OPERAND.is_match(a) && !a.chars().nth(1).is_some_and(|c| c == ':')
                }) =>
            {
                out.push(hit(name_key(name), "copies to or from a remote host"));
            }
            "curl" | "wget" => downloads = true,
            _ => {}
        }
    }
    // download-and-run: a downloader next to a shell/interpreter (`curl x | sh`, `sh -c "$(curl x)"`)
    if downloads && runs {
        out.push(hit("curl | sh", "downloads code and runs it"));
    }
    out
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
