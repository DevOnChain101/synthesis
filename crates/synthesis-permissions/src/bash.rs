//! Bash command classifiers and parsers. Port of the shell half of Node `permissions.js`
//! (lines 20-350, 431-480). All regexes use explicit ASCII classes because JS `\w`/`\d` are
//! ASCII-only while the `regex` crate's are Unicode.

use std::sync::LazyLock;

use regex::Regex;

pub type Words = Vec<String>;

fn re(p: &str) -> Regex {
    Regex::new(p).expect("static regex")
}

pub const INTERPRETERS: &[&str] = &[
    "python",
    "python2",
    "python3",
    "py",
    "node",
    "nodejs",
    "deno",
    "bun",
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "fish",
    "ruby",
    "perl",
    "php",
    "lua",
    "pwsh",
    "powershell",
    "cmd",
    "eval",
    "exec",
    "env",
    "sudo",
    "xargs",
    "npx",
    "pnpx",
    "bunx",
    "uvx",
    "java",
    "dotnet",
];
const PKG_MANAGERS: &[&str] = &["npm", "yarn", "pnpm", "bun"];
const PKG_RUNNERS: &[&str] = &["run", "run-script", "exec", "dlx", "x", "create", "init"];

/// `Tool(spec)` -> (tool, Some(spec)); anything else -> (rule, None). Port of `parseRule`.
pub fn parse_rule(rule: &str) -> (&str, Option<&str>) {
    static R: LazyLock<Regex> = LazyLock::new(|| re(r"^([^(]+)\((.*)\)$"));
    match R.captures(rule) {
        Some(c) => (c.get(1).unwrap().as_str(), Some(c.get(2).unwrap().as_str())),
        None => (rule, None),
    }
}

/// Allow rules that grant arbitrary code execution (suspended in auto mode). Port of `isBroadAllow`.
pub fn is_broad_allow(rule: &str, classify_all_shell: bool) -> bool {
    static TAIL: LazyLock<Regex> = LazyLock::new(|| re(r":?\*+$"));
    static HEAD: LazyLock<Regex> = LazyLock::new(|| re(r"[:*].*$"));
    static GLUED: LazyLock<Regex> = LazyLock::new(|| re(r"^[^\s:]*\*$"));
    static STAR: LazyLock<Regex> = LazyLock::new(|| re(r"\*.*$"));
    let (tool, spec) = parse_rule(rule);
    if tool == "Task" || tool == "Agent" || tool == "*" {
        return true;
    }
    if tool != "Bash" {
        return false;
    }
    let Some(spec) = spec else { return true };
    if classify_all_shell {
        return true;
    }
    if !spec.contains('*') {
        return false; // an exact command is narrow
    }
    let trimmed = TAIL.replace(spec, " ");
    let words: Vec<&str> = trimmed.split_whitespace().collect();
    let head = HEAD
        .replace(words.first().copied().unwrap_or(""), "")
        .into_owned();
    if head.is_empty() {
        return true;
    }
    // a wildcard glued to the program name (python*, n*) can reach an interpreter
    if words.len() == 1 && GLUED.is_match(spec) && INTERPRETERS.iter().any(|i| i.starts_with(&head))
    {
        return true;
    }
    if INTERPRETERS.contains(&head.as_str()) {
        return true;
    }
    if PKG_MANAGERS.contains(&head.as_str()) {
        return words.len() < 2 || PKG_RUNNERS.contains(&STAR.replace(words[1], "").as_ref());
    }
    false
}

// --- static allowlists ---------------------------------------------------------------------

const NPM_RUN_OK: &[&str] = &["test", "lint", "build", "typecheck", "check"];
const BRANCH_LIST_MODE: &[&str] = &[
    "--list",
    "--merged",
    "--no-merged",
    "--contains",
    "--no-contains",
    "--points-at",
];

fn is_ws(c: char) -> bool {
    c.is_whitespace()
}

/// Length (in chars) of an allowed redirect at the start of `s` (`>/dev/null`, `>>/dev/null`,
/// `>&1`, `>&2`, `&>/dev/null`, `&>>/dev/null`) that is followed by whitespace, `;`, `&`, `|`
/// or the end. Port of `REDIRECT_OK` (the regex crate has no lookahead).
fn redirect_ok(s: &[char]) -> Option<usize> {
    fn dev_null(s: &[char], mut i: usize) -> Option<usize> {
        while s.get(i).is_some_and(|c| is_ws(*c)) {
            i += 1;
        }
        let lit: Vec<char> = "/dev/null".chars().collect();
        (s.get(i..i + lit.len())? == lit.as_slice()).then_some(i + lit.len())
    }
    let mut ends = Vec::new();
    if s.first() == Some(&'>') {
        ends.extend(dev_null(s, if s.get(1) == Some(&'>') { 2 } else { 1 }));
        if s.get(1) == Some(&'&') && matches!(s.get(2), Some('1' | '2')) {
            ends.push(3);
        }
    }
    if s.first() == Some(&'&') && s.get(1) == Some(&'>') {
        ends.extend(dev_null(s, if s.get(2) == Some(&'>') { 3 } else { 2 }));
    }
    ends.into_iter().find(|&e| {
        s.get(e)
            .is_none_or(|c| is_ws(*c) || matches!(c, ';' | '&' | '|'))
    })
}

/// Splits a command on `; && || | &` and newlines (quotes respected). `None` when the parser cannot
/// reason about it: substitution, backticks, process substitution, here-docs, unbalanced quotes,
/// output redirection other than /dev/null and 2>&1. Port of `parseSimpleCommands`.
pub fn parse_simple_commands(cmd: &str) -> Option<Vec<Words>> {
    // a compound that ends in an operator is incomplete, and a very long command is never
    // auto-allowed (Claude Code: 10,000 characters)
    let t = cmd.trim_end();
    if t.ends_with("&&") || t.ends_with('|') || cmd.len() > 10_000 {
        return None;
    }
    let chars: Vec<char> = cmd.chars().collect();
    let mut segs: Vec<Words> = Vec::new();
    let mut words: Words = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if quote == Some('\'') {
            if c == '\'' {
                quote = None;
            } else {
                cur.push(c);
            }
            i += 1;
            continue;
        }
        if c == '\\' {
            let n = next?;
            cur.push(n);
            has = true;
            i += 2;
            continue;
        }
        if c == '`' || (c == '$' && next == Some('(')) {
            return None;
        }
        if quote == Some('"') {
            if c == '"' {
                quote = None;
            } else {
                cur.push(c);
            }
            i += 1;
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            has = true;
            i += 1;
            continue;
        }
        if c == '<' && (next == Some('(') || next == Some('<')) {
            return None;
        }
        if c == '>' || (c == '&' && next == Some('>')) {
            let n = redirect_ok(&chars[i..])?;
            if c == '>' && (cur == "1" || cur == "2") {
                cur.clear();
                has = false;
            }
            i += n;
            continue;
        }
        if matches!(c, ';' | '|' | '&' | '\n') {
            end_seg(&mut words, &mut segs, &mut cur, &mut has);
            i += 1;
            continue;
        }
        if is_ws(c) {
            end_word(&mut words, &mut cur, &mut has);
            i += 1;
            continue;
        }
        cur.push(c);
        has = true;
        i += 1;
    }
    if quote.is_some() {
        return None;
    }
    end_seg(&mut words, &mut segs, &mut cur, &mut has);
    Some(segs)
}

fn end_word(words: &mut Words, cur: &mut String, has: &mut bool) {
    if *has {
        words.push(std::mem::take(cur));
    }
    cur.clear();
    *has = false;
}

fn end_seg(words: &mut Words, segs: &mut Vec<Words>, cur: &mut String, has: &mut bool) {
    end_word(words, cur, has);
    if !words.is_empty() {
        segs.push(std::mem::take(words));
    }
}

/// Allowed flags of one read-only command: `plain` short letters, `arg` short letters that take
/// the rest of the cluster as their value (`-n5`, `-d,`), exact long names (`--x` or `--x=v`;
/// GNU abbreviations such as `--outp` are NOT accepted). Digits (`-5`) are always fine.
struct Fl {
    plain: &'static str,
    arg: &'static str,
    long: &'static [&'static str],
}

const fn fl(plain: &'static str, arg: &'static str, long: &'static [&'static str]) -> Fl {
    Fl { plain, arg, long }
}

const GIT_DIFF_LONG: &[&str] = &[
    "stat",
    "numstat",
    "shortstat",
    "name-only",
    "name-status",
    "oneline",
    "graph",
    "decorate",
    "no-decorate",
    "abbrev-commit",
    "abbrev",
    "patch",
    "no-patch",
    "patch-with-stat",
    "pretty",
    "format",
    "date",
    "since",
    "until",
    "after",
    "before",
    "author",
    "committer",
    "grep",
    "all",
    "branches",
    "tags",
    "remotes",
    "merges",
    "no-merges",
    "first-parent",
    "reverse",
    "max-count",
    "skip",
    "follow",
    "cached",
    "staged",
    "raw",
    "summary",
    "color",
    "no-color",
    "color-words",
    "word-diff",
    "unified",
    "minimal",
    "histogram",
    "patience",
    "ignore-space-change",
    "ignore-all-space",
    "ignore-blank-lines",
    "full-index",
    "binary",
    "find-renames",
    "find-copies",
    "diff-filter",
    "root",
    "relative",
    "check",
    "source",
    "left-right",
    "cherry-pick",
    "boundary",
    "topo-order",
    "date-order",
    "author-date-order",
    "exit-code",
    "quiet",
    "submodule",
    "dirstat",
    "compact-summary",
    "no-color-moved",
    "color-moved",
    "show-signature",
    "notes",
    "no-notes",
];

fn flags_for(name: &str) -> Option<Fl> {
    Some(match name {
        "ls" => fl(
            "aAbcCdFghHiklLmnopqrRsStuUvx1",
            "",
            &[
                "all",
                "almost-all",
                "classify",
                "color",
                "directory",
                "dereference",
                "file-type",
                "human-readable",
                "inode",
                "numeric-uid-gid",
                "recursive",
                "reverse",
                "size",
                "sort",
                "time",
                "time-style",
                "group-directories-first",
                "si",
                "literal",
                "quote-name",
                "full-time",
            ],
        ),
        "pwd" => fl("LP", "", &[]),
        "cat" => fl(
            "AbeEnstTuv",
            "",
            &[
                "show-all",
                "number-nonblank",
                "show-ends",
                "number",
                "squeeze-blank",
                "show-tabs",
                "show-nonprinting",
            ],
        ),
        "head" | "tail" => fl(
            "qvz",
            "nc",
            &[
                "lines",
                "bytes",
                "quiet",
                "silent",
                "verbose",
                "zero-terminated",
            ],
        ),
        "wc" => fl(
            "clmwL",
            "",
            &["lines", "words", "bytes", "chars", "max-line-length"],
        ),
        "which" => fl("a", "", &[]),
        "type" => fl("afptP", "", &[]),
        "true" => fl("", "", &[]),
        "rg" => fl(
            "iIsSnNHhlLcovwxFaAbBCeEgGjmMtTuUVpPfr",
            "",
            &[
                "ignore-case",
                "case-sensitive",
                "smart-case",
                "line-number",
                "no-line-number",
                "with-filename",
                "no-filename",
                "files-with-matches",
                "files-without-match",
                "count",
                "only-matching",
                "word-regexp",
                "line-regexp",
                "fixed-strings",
                "regexp",
                "glob",
                "type",
                "type-not",
                "hidden",
                "no-ignore",
                "max-count",
                "max-depth",
                "context",
                "after-context",
                "before-context",
                "files",
                "json",
                "heading",
                "no-heading",
                "color",
                "invert-match",
                "multiline",
                "follow",
                "sort",
                "sortr",
                "stats",
                "column",
                "vimgrep",
                "pretty",
                "replace",
                "text",
                "binary",
                "threads",
                "max-filesize",
                "null",
                "trim",
                "file",
            ],
        ),
        "grep" => fl(
            "iIvnHhlLcoqswxFEGPraAbBCeRTzZf",
            "",
            &[
                "ignore-case",
                "invert-match",
                "line-number",
                "with-filename",
                "no-filename",
                "files-with-matches",
                "files-without-match",
                "count",
                "only-matching",
                "quiet",
                "silent",
                "no-messages",
                "word-regexp",
                "line-regexp",
                "fixed-strings",
                "extended-regexp",
                "basic-regexp",
                "perl-regexp",
                "recursive",
                "dereference-recursive",
                "include",
                "exclude",
                "exclude-dir",
                "regexp",
                "file",
                "after-context",
                "before-context",
                "context",
                "color",
                "colour",
                "max-count",
                "binary-files",
                "text",
                "null",
                "initial-tab",
            ],
        ),
        "sort" => fl(
            "bcdfghiMnrRsuVzC",
            "kt",
            &[
                "ignore-leading-blanks",
                "dictionary-order",
                "ignore-case",
                "general-numeric-sort",
                "human-numeric-sort",
                "ignore-nonprinting",
                "month-sort",
                "numeric-sort",
                "reverse",
                "sort",
                "stable",
                "unique",
                "version-sort",
                "zero-terminated",
                "key",
                "field-separator",
                "check",
            ],
        ),
        "uniq" => fl(
            "cdDiuz",
            "fsw",
            &[
                "count",
                "repeated",
                "unique",
                "ignore-case",
                "skip-fields",
                "skip-chars",
                "check-chars",
                "zero-terminated",
            ],
        ),
        "tr" => fl(
            "cdsC",
            "",
            &["complement", "delete", "squeeze-repeats", "truncate-set1"],
        ),
        "cut" => fl(
            "nsz",
            "bcdf",
            &[
                "bytes",
                "characters",
                "delimiter",
                "fields",
                "only-delimited",
                "complement",
                "zero-terminated",
            ],
        ),
        "diff" => fl(
            "abBcEiNpqrsStTuwyZ",
            "UCxXILF",
            &[
                "brief",
                "ignore-case",
                "ignore-space-change",
                "ignore-all-space",
                "ignore-blank-lines",
                "unified",
                "context",
                "recursive",
                "new-file",
                "report-identical-files",
                "show-c-function",
                "text",
                "side-by-side",
                "color",
                "strip-trailing-cr",
                "exclude",
                "label",
                "minimal",
                "expand-tabs",
                "initial-tab",
            ],
        ),
        "stat" => fl(
            "LftxF",
            "c",
            &["dereference", "file-system", "format", "terse"],
        ),
        "file" => fl(
            "bhiLnNps",
            "",
            &[
                "brief",
                "mime",
                "mime-type",
                "mime-encoding",
                "dereference",
                "no-dereference",
                "keep-going",
                "no-pad",
                "raw",
            ],
        ),
        "du" => fl(
            "abchHkLmsxP0",
            "dB",
            &[
                "all",
                "apparent-size",
                "bytes",
                "total",
                "summarize",
                "human-readable",
                "si",
                "max-depth",
                "dereference",
                "one-file-system",
                "separate-dirs",
                "count-links",
                "block-size",
                "exclude",
                "time",
            ],
        ),
        "df" => fl(
            "aHhikPTl",
            "tx",
            &[
                "all",
                "human-readable",
                "si",
                "inodes",
                "local",
                "portability",
                "print-type",
                "type",
                "exclude-type",
                "total",
                "block-size",
            ],
        ),
        "date" => fl(
            "uRI",
            "d",
            &[
                "utc",
                "universal",
                "rfc-3339",
                "rfc-email",
                "iso-8601",
                "date",
            ],
        ),
        "basename" => fl("az", "s", &["multiple", "suffix", "zero"]),
        "dirname" => fl("z", "", &["zero"]),
        "realpath" => fl(
            "eEmLPqsz",
            "",
            &[
                "canonicalize-existing",
                "canonicalize-missing",
                "logical",
                "physical",
                "quiet",
                "strip",
                "no-symlinks",
                "zero",
                "relative-to",
                "relative-base",
            ],
        ),
        _ => return None,
    })
}

fn flags_ok(args: &[String], f: &Fl) -> bool {
    for a in args {
        if a == "--" {
            return true;
        }
        if let Some(l) = a.strip_prefix("--") {
            if !f.long.contains(&l.split('=').next().unwrap_or("")) {
                return false;
            }
        } else if a.len() > 1 && a.starts_with('-') {
            for c in a[1..].chars() {
                if c.is_ascii_digit() || f.plain.contains(c) {
                    continue;
                }
                if f.arg.contains(c) {
                    break;
                }
                return false;
            }
        }
    }
    true
}

/// find: only these single-dash tests/actions; no -exec/-delete/-fprint*/-ok.
const FIND_OK: &[&str] = &[
    "-L",
    "-H",
    "-P",
    "-name",
    "-iname",
    "-path",
    "-ipath",
    "-wholename",
    "-iwholename",
    "-regex",
    "-iregex",
    "-type",
    "-maxdepth",
    "-mindepth",
    "-size",
    "-mtime",
    "-atime",
    "-ctime",
    "-mmin",
    "-amin",
    "-cmin",
    "-newer",
    "-empty",
    "-print",
    "-print0",
    "-not",
    "-and",
    "-or",
    "-o",
    "-a",
    "-prune",
    "-perm",
    "-user",
    "-group",
    "-depth",
    "-mount",
    "-xdev",
    "-readable",
    "-writable",
    "-executable",
    "-true",
    "-false",
    "-links",
    "-lname",
    "-ilname",
    "-samefile",
    "-inum",
    "-nouser",
    "-nogroup",
    "-follow",
    "-ls",
    "-quit",
];

pub fn is_read_only_simple(words: &[String]) -> bool {
    static BRANCH_FLAGS: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r"^(?:-a|-r|-v|-vv|--all|--remotes|--verbose|--list|--show-current|--merged|--no-merged|--contains|--no-contains|--points-at|--sort(?:=.*)?|--format(?:=.*)?|--abbrev(?:=[0-9]+)?|--column|--no-column|--color(?:=[A-Za-z0-9_]+)?|--no-color)$",
        )
    });
    static FIND_NUM: LazyLock<Regex> = LazyLock::new(|| re(r"^-[0-9]+[ckMGbw]?$"));
    let Some(name) = words.first().map(String::as_str) else {
        return false;
    };
    // `$VAR` / `${..}` expand to something unknown (echo $SECRET, a flag smuggled via a variable)
    if words.iter().any(|w| w.contains('$')) {
        return false;
    }
    let sub = words.get(1).map(String::as_str);
    let args = &words[1..];
    // A builtin that only prints its arguments (`$` expansion is refused above, redirects by the
    // parser). Every word is text, so `echo ---` is not a flag error.
    if name == "echo" {
        return true;
    }
    if name == "cd" {
        return words.len() <= 2;
    }
    if name == "find" {
        return args.iter().all(|a| {
            !a.starts_with('-') || a == "-" || FIND_OK.contains(&a.as_str()) || FIND_NUM.is_match(a)
        });
    }
    if let Some(f) = flags_for(name) {
        return flags_ok(args, &f)
            && match name {
                "uniq" => args.iter().filter(|a| !a.starts_with('-')).count() <= 1, // 2nd positional is an output file
                _ => true,
            };
    }
    if name == "git" {
        let rest = words.get(2..).unwrap_or(&[]);
        return match sub {
            Some("status") => flags_ok(
                rest,
                &fl(
                    "sbvz",
                    "u",
                    &[
                        "short",
                        "branch",
                        "porcelain",
                        "long",
                        "ignored",
                        "untracked-files",
                        "ahead-behind",
                        "no-ahead-behind",
                        "renames",
                        "no-renames",
                        "column",
                        "no-column",
                        "null",
                        "verbose",
                        "show-stash",
                    ],
                ),
            ),
            Some("diff" | "log" | "show") => {
                flags_ok(rest, &fl("pPsuqrRtzwabcCMBvl", "SGULn", GIT_DIFF_LONG))
            }
            Some("rev-parse") => flags_ok(
                rest,
                &fl(
                    "",
                    "",
                    &[
                        "show-toplevel",
                        "show-cdup",
                        "show-prefix",
                        "git-dir",
                        "abbrev-ref",
                        "short",
                        "verify",
                        "is-inside-work-tree",
                        "is-inside-git-dir",
                        "is-bare-repository",
                        "symbolic",
                        "symbolic-full-name",
                        "absolute-git-dir",
                        "git-common-dir",
                        "all",
                        "branches",
                        "tags",
                        "remotes",
                    ],
                ),
            ),
            Some("ls-files") => flags_ok(
                rest,
                &fl(
                    "cdmosuzt",
                    "x",
                    &[
                        "cached",
                        "deleted",
                        "modified",
                        "others",
                        "ignored",
                        "stage",
                        "unmerged",
                        "killed",
                        "directory",
                        "no-empty-directory",
                        "exclude-standard",
                        "full-name",
                        "error-unmatch",
                        "deduplicate",
                        "eol",
                        "exclude",
                    ],
                ),
            ),
            Some("remote") => words.len() == 3 && words[2] == "-v",
            Some("stash") => words.get(2).map(String::as_str) == Some("list"),
            Some("branch") => {
                let flags: Vec<&String> = rest.iter().filter(|a| a.starts_with('-')).collect();
                if !flags.iter().all(|a| BRANCH_FLAGS.is_match(a)) {
                    return false;
                }
                rest.len() == flags.len()
                    || flags.iter().any(|a| BRANCH_LIST_MODE.contains(&a.as_str()))
            }
            _ => false,
        };
    }
    if name == "node" {
        return words.len() == 2 && matches!(sub, Some("--version" | "-v"));
    }
    if name == "npm" {
        return matches!(sub, Some("ls" | "list" | "view"))
            && flags_ok(
                words.get(2..).unwrap_or(&[]),
                &fl(
                    "gl",
                    "",
                    &[
                        "json",
                        "long",
                        "all",
                        "depth",
                        "global",
                        "parseable",
                        "production",
                        "omit",
                        "link",
                        "unicode",
                        "workspaces",
                    ],
                ),
            );
    }
    false
}

pub fn is_auto_safe_simple(words: &[String]) -> bool {
    if is_read_only_simple(words) {
        return true;
    }
    let Some(name) = words.first().map(String::as_str) else {
        return false;
    };
    let sub = words.get(1).map(String::as_str);
    match name {
        "node" => matches!(sub, Some("--test" | "--check")),
        "npm" => {
            sub == Some("test")
                || (matches!(sub, Some("run" | "run-script"))
                    && words
                        .get(2)
                        .is_some_and(|w| NPM_RUN_OK.contains(&w.as_str())))
        }
        "npx" => sub == Some("tsc") && words.iter().any(|w| w == "--noEmit"),
        _ => false,
    }
}

fn all_segments(cmd: &str, ok: fn(&[String]) -> bool) -> bool {
    parse_simple_commands(cmd).is_some_and(|s| !s.is_empty() && s.iter().all(|w| ok(w)))
}

/// Runs without a prompt in every mode.
pub fn is_read_only_bash(cmd: &str) -> bool {
    all_segments(cmd, is_read_only_simple)
}
/// Read-only plus build/test/lint; only in `auto`.
pub fn is_auto_safe_bash(cmd: &str) -> bool {
    all_segments(cmd, is_auto_safe_simple)
}
pub fn is_safe_bash_command(cmd: &str) -> bool {
    is_auto_safe_bash(cmd)
}

// --- acceptEdits file commands, protected and critical paths -----------------------------------

pub const FS_CMDS: &[&str] = &["mkdir", "touch", "rm", "rmdir", "mv", "cp", "sed"];
pub const SHELLS: &[&str] = &["bash", "sh", "zsh", "dash", "ksh"];

/// Strips safe env assignments and the wrappers timeout/nice/nohup. `loose` (critical-path scan
/// only) strips any assignment plus sudo/command/exec/time. Port of `unwrap`.
pub fn unwrap_words(words: &[String], loose: bool) -> Words {
    static ASSIGN: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Za-z_][A-Za-z0-9_]*="));
    static SAFE_ENV: LazyLock<Regex> = LazyLock::new(|| {
        re(r"^(?:LANG|LC_[A-Za-z0-9_]+|NO_COLOR|FORCE_COLOR|TZ|CI)=[A-Za-z0-9_.@:+/-]*$")
    });
    static DURATION: LazyLock<Regex> = LazyLock::new(|| re(r"^[0-9]+(?:\.[0-9]+)?[smhd]?$"));
    static NICE_N: LazyLock<Regex> = LazyLock::new(|| re(r"^-?[0-9]+$"));
    static NICE_L: LazyLock<Regex> = LazyLock::new(|| re(r"^-[0-9]+$"));
    let mut w: std::collections::VecDeque<String> = words.iter().cloned().collect();
    while let Some(a) = w.front().cloned() {
        if ASSIGN.is_match(&a) && (loose || SAFE_ENV.is_match(&a)) {
            w.pop_front();
        } else if a == "timeout" && (loose || w.get(1).is_some_and(|d| DURATION.is_match(d))) {
            w.pop_front();
            while loose && w.front().is_some_and(|x| x.starts_with('-')) {
                if matches!(w.pop_front().as_deref(), Some("-k" | "-s")) {
                    w.pop_front();
                }
            }
            w.pop_front();
        } else if a == "nice" {
            w.pop_front();
            if w.front().map(String::as_str) == Some("-n")
                && w.get(1).is_some_and(|n| NICE_N.is_match(n))
            {
                w.drain(..2);
            } else if loose && w.front().is_some_and(|n| NICE_L.is_match(n)) {
                w.pop_front();
            }
        } else if a == "nohup" || a == "time" || a == "command" {
            w.pop_front();
        } else if a == "stdbuf" && w.get(1).is_some_and(|x| x.starts_with('-')) {
            w.pop_front();
            while w.front().is_some_and(|x| x.starts_with('-')) {
                w.pop_front();
            }
        } else if loose
            && matches!(
                a.as_str(),
                "sudo"
                    | "doas"
                    | "command"
                    | "exec"
                    | "time"
                    | "env"
                    | "xargs"
                    | "watch"
                    | "parallel"
            )
        {
            // the wrapper's own flags (and their values) up to the wrapped command
            let valued: &[&str] = match a.as_str() {
                "sudo" => &["-u", "-g", "-h", "-p", "-C", "-D", "-R", "-T", "-U"],
                "doas" => &["-u", "-C"],
                "watch" => &["-n"],
                "parallel" => &["-j", "-n", "-N", "-a", "-S", "-I", "--jobs", "--arg-file"],
                "env" => &["-u", "-C", "-S"],
                "xargs" => &["-n", "-I", "-L", "-P", "-s", "-d", "-E", "-a"],
                _ => &[],
            };
            w.pop_front();
            while w.front().is_some_and(|x| x.starts_with('-') && x != "--") {
                if valued.contains(&w.pop_front().unwrap().as_str()) {
                    w.pop_front();
                }
            }
        } else {
            break;
        }
    }
    w.into()
}

/// Only `s` with plain flags, or an address + d/p/q are accepted: GNU sed can run commands
/// (e, w, r, f...), so every other script asks.
fn sed_script_ok(sc: Option<&str>) -> bool {
    static SIMPLE: LazyLock<Regex> = LazyLock::new(|| {
        let addr = r"(?:[0-9]+|\$|/(?:[^/\\]|\\.)*/)";
        re(&format!("^(?:{addr}(?:,{addr})?)?[dpq]$"))
    });
    static FLAGS: LazyLock<Regex> = LazyLock::new(|| re(r"^[gIi0-9p]*$"));
    let Some(sc) = sc else { return false };
    if SIMPLE.is_match(sc) {
        return true;
    }
    let chars: Vec<char> = sc.chars().collect();
    if chars.first() != Some(&'s') {
        return false;
    }
    let Some(&d) = chars.get(1) else { return false };
    if d.is_ascii_alphanumeric() || d == '_' || d == '\\' || is_ws(d) {
        return false;
    }
    let (mut n, mut last, mut i) = (1, 1, 2);
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 1;
        } else if chars[i] == d {
            n += 1;
            last = i;
        }
        i += 1;
    }
    n == 3
        && FLAGS.is_match(
            &chars[(last + 1).min(chars.len())..]
                .iter()
                .collect::<String>(),
        )
}

/// Path operands of one fs command, or `None` when a flag or script is not understood.
pub fn fs_operands(name: &str, args: &[String]) -> Option<Vec<String>> {
    static SED_E: LazyLock<Regex> = LazyLock::new(|| re(r"^-[nErsz]*e$"));
    static SED_OK: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r"^(?:-[nErsz]+|-i\S*|--(?:in-place(?:=\S*)?|quiet|silent|regexp-extended|null-data|separate))$",
        )
    });
    static TOUCH_ARG: LazyLock<Regex> = LazyLock::new(|| re(r"^(?:-[dtr]|--date|--reference)$"));
    static CPMV_BAD: LazyLock<Regex> = LazyLock::new(|| {
        re(r"^(?:-[a-zA-Z]*[tTS][a-zA-Z]*|--target-directory.*|--no-target-directory|--suffix.*)$")
    });
    let mut ops: Vec<String> = Vec::new();
    let mut scripts: Vec<Option<String>> = Vec::new();
    let mut dd = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if dd || !a.starts_with('-') || a == "-" {
            ops.push(a.to_string());
        } else if a == "--" {
            dd = true;
        } else if name == "sed" {
            if a == "--expression" {
                i += 1;
                scripts.push(args.get(i).cloned());
            } else if let Some(s) = a.strip_prefix("--expression=") {
                scripts.push(Some(s.to_string()));
            } else if SED_E.is_match(a) {
                i += 1;
                scripts.push(args.get(i).cloned());
            } else if !SED_OK.is_match(a) {
                return None;
            }
        } else if (name == "mkdir" && (a == "-m" || a == "--mode"))
            || (name == "touch" && TOUCH_ARG.is_match(a))
        {
            i += 1;
        } else if (name == "cp" || name == "mv") && CPMV_BAD.is_match(a) {
            return None; // names a path outside the operand list
        }
        i += 1;
    }
    if name != "sed" {
        return Some(ops);
    }
    if scripts.is_empty() {
        scripts.push((!ops.is_empty()).then(|| ops.remove(0)));
    }
    scripts
        .iter()
        .all(|s| sed_script_ok(s.as_deref()))
        .then_some(ops)
}

/// Like `parse_simple_commands` but never gives up: used only to FIND rm/rmdir targets inside
/// constructs the strict parser rejects: (...), { ...; }, $(...), backticks. Substitutions stay
/// verbatim in the word. Port of `lenientSegments`.
pub fn lenient_segments(cmd: &str) -> Vec<Words> {
    let chars: Vec<char> = cmd.chars().collect();
    let mut segs: Vec<Words> = Vec::new();
    let mut words: Words = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if quote == Some('\'') {
            if c == '\'' {
                quote = None;
            } else {
                cur.push(c);
            }
        } else if c == '\\' {
            // before a word char keep it (C:\Windows): reading it as a path separator is the safe side here
            if next.is_some_and(|n| n.is_ascii_alphanumeric() || n == '_') {
                cur.push(c);
            } else if let Some(n) = next {
                cur.push(n);
                i += 1;
            } else {
                cur.push('\\');
            }
            has = true;
        } else if c == '$' && matches!(next, Some('(' | '{')) {
            let (open, close) = if next == Some('(') {
                ('(', ')')
            } else {
                ('{', '}')
            };
            let mut depth = 0;
            let mut j = i + 1;
            while j < chars.len() {
                if chars[j] == open {
                    depth += 1;
                } else if chars[j] == close {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                j += 1;
            }
            cur.extend(&chars[i..(j + 1).min(chars.len())]);
            has = true;
            i = j;
        } else if c == '`' {
            let j = chars[i + 1..]
                .iter()
                .position(|&x| x == '`')
                .map(|p| p + i + 1);
            cur.extend(&chars[i..j.map_or(chars.len(), |j| j + 1)]);
            has = true;
            i = j.unwrap_or(chars.len());
        } else if quote == Some('"') {
            if c == '"' {
                quote = None;
            } else {
                cur.push(c);
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
            has = true;
        } else if matches!(c, ';' | '|' | '&' | '\n' | '(' | ')')
            || ((c == '{' || c == '}') && !has && next.is_none_or(|n| is_ws(n) || n == ';'))
        {
            end_seg(&mut words, &mut segs, &mut cur, &mut has);
        } else if is_ws(c) {
            end_word(&mut words, &mut cur, &mut has);
        } else {
            cur.push(c);
            has = true;
        }
        i += 1;
    }
    end_seg(&mut words, &mut segs, &mut cur, &mut has);
    segs
}

/// First path component made only of wildcards (optionally after `./`) and the operand ends in `*`
/// or `*/`: `*`, `.*`, `*/`, `*/*`, `./*`, `[a-z]*`. `*.o` and `src/*` are not.
pub fn bare_glob(op: &str) -> bool {
    static LEAD: LazyLock<Regex> = LazyLock::new(|| re(r"^(?:\.[\\/])+"));
    static FIRST: LazyLock<Regex> = LazyLock::new(|| re(r"^\.?(?:[*?]|\[[^\]]*\])+$"));
    static END: LazyLock<Regex> = LazyLock::new(|| re(r"\*[\\/]?$"));
    let s = LEAD.replace(op, "");
    let first = s.split(['\\', '/']).next().unwrap_or("");
    FIRST.is_match(first) && END.is_match(&s)
}

/// `$VAR`, `${VAR}/`, `\`, `$(...)`, backticks: a target the shell expands to something unknown.
pub fn unknown_expansion(t: &str) -> bool {
    static R: LazyLock<Regex> = LazyLock::new(|| {
        re(r"^(?:\$\{?[A-Za-z0-9_]+\}?(?:[\\/](?:\*+[\\/]?)?)?|\\|\$\(.*\)|`.*`)$")
    });
    R.is_match(t)
}

// --- allow/deny/ask matching ---------------------------------------------------------------------

/// `-c`, `-ec`, `-lc` ...
pub fn is_dash_c(x: &str) -> bool {
    x.strip_prefix('-')
        .is_some_and(|r| r.contains('c') && r.chars().all(|c| c.is_ascii_lowercase()))
}

/// Quotes closed, `(`/`$(` balanced, backticks paired, no trailing backslash.
fn balanced(cmd: &str) -> bool {
    let (mut quote, mut depth, mut ticks) = (None, 0i32, 0);
    let mut it = cmd.chars().peekable();
    while let Some(c) = it.next() {
        match (quote, c) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('\''), _) => {}
            (_, '\\') => {
                if it.next().is_none() {
                    return false;
                }
            }
            (None, '\'' | '"') => quote = Some(c),
            (_, '`') => ticks += 1,
            (_, '$') if it.peek() == Some(&'(') => {
                it.next();
                depth += 1;
            }
            (None, '(') => depth += 1,
            (_, ')') if quote.is_none() || depth > 0 => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    quote.is_none() && depth == 0 && ticks % 2 == 0
}

/// Inner commands of `$(...)`, `<(...)`, `>(...)` and backticks inside one word.
fn substitutions(w: &str) -> Vec<String> {
    let c: Vec<char> = w.chars().collect();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < c.len() {
        if c[i] == '`' {
            let end = c[i + 1..].iter().position(|&x| x == '`');
            out.push(
                c[i + 1..end.map_or(c.len(), |e| i + 1 + e)]
                    .iter()
                    .collect(),
            );
            i += end.map_or(c.len(), |e| e + 2);
            continue;
        }
        if matches!(c[i], '$' | '<' | '>') && c.get(i + 1) == Some(&'(') {
            let (mut d, mut j) = (0, i + 1);
            while j < c.len() {
                if c[j] == '(' {
                    d += 1;
                } else if c[j] == ')' {
                    d -= 1;
                    if d == 0 {
                        break;
                    }
                }
                j += 1;
            }
            out.push(c[i + 2..j.min(c.len())].iter().collect());
            i = j;
        }
        i += 1;
    }
    out
}

/// Every segment of `cmd` from the lenient splitter (`& && || ; | newline`, `(...)`), plus those
/// of every `$(...)`/backtick body, `sh -c '...'` and `eval ...` inside it. `None` = not
/// confidently parsable (unbalanced quotes/parens, too deep): deny/ask rules then match.
pub fn deep_segments(cmd: &str, depth: u32) -> Option<Vec<Words>> {
    if depth > 4 || !balanced(cmd) {
        return None;
    }
    let mut out = Vec::new();
    for seg in lenient_segments(cmd) {
        add_seg(seg, depth, &mut out)?;
    }
    Some(out)
}

/// `git -c key=value`: values that run a command (`core.pager`, `alias.x=!cmd`, ...).
const GIT_EXEC_KEYS: &[&str] = &[
    "core.pager",
    "core.editor",
    "core.sshcommand",
    "core.fsmonitor",
    "core.askpass",
    "sequence.editor",
    "diff.external",
    "gpg.program",
];

fn git_c_bodies(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next().filter(|a| a.starts_with('-')) {
        let kv = match a.as_str() {
            "-c" => it.next().map(String::as_str),
            "-C" | "--git-dir" | "--work-tree" | "--namespace" => {
                it.next();
                None
            }
            x => x.strip_prefix("-c"), // `-ckey=value`
        };
        if let Some((k, v)) = kv.and_then(|kv| kv.split_once('=')) {
            if let Some(body) = v.strip_prefix('!') {
                out.push(body.to_string());
            } else if GIT_EXEC_KEYS.contains(&k.to_ascii_lowercase().as_str()) {
                out.push(v.to_string());
            }
        }
    }
    out
}

/// One segment plus everything it runs: `$(..)` bodies, `sh -c`, `eval`, `find -exec/-delete`
/// and `git -c` command values.
fn add_seg(seg: Words, depth: u32, out: &mut Vec<Words>) -> Option<()> {
    for inner in seg.iter().flat_map(|w| substitutions(w)) {
        out.extend(deep_segments(&inner, depth + 1)?);
    }
    let u = unwrap_words(&seg, true);
    let name = u
        .first()
        .map_or("", |f| f.rsplit(['/', '\\']).next().unwrap_or(""));
    if SHELLS.contains(&name) {
        let i = u
            .iter()
            .enumerate()
            .position(|(k, x)| k > 0 && is_dash_c(x));
        if let Some(body) = i.and_then(|i| u.get(i + 1)) {
            out.extend(deep_segments(body, depth + 1)?);
        }
    } else if name == "eval" {
        out.extend(deep_segments(&u[1..].join(" "), depth + 1)?);
    } else if name == "find" {
        let mut it = u.iter();
        while let Some(a) = it.next() {
            if a == "-delete" {
                out.push(["rm", "-rf", "<find -delete>"].map(String::from).to_vec());
            } else if matches!(a.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir") {
                let inner: Words = it
                    .by_ref()
                    .take_while(|x| !matches!(x.as_str(), ";" | "+"))
                    .cloned()
                    .collect();
                if depth > 4 {
                    return None;
                }
                add_seg(inner, depth + 1, out)?;
            }
        }
    } else if name == "git" {
        for body in git_c_bodies(&u[1..]) {
            out.extend(deep_segments(&body, depth + 1)?);
        }
    } else if u.first().is_some_and(|f| f.contains(is_ws)) {
        // `watch 'rm x'`: the wrapper's command is one quoted word
        out.extend(deep_segments(&u[0], depth + 1)?);
    }
    out.push(seg);
    Some(())
}

/// A segment that runs code written on its command line: a language interpreter with
/// `-c` / `-e` (opaque: the body is not shell), or, with `shells`, `sh -c` and `eval`.
/// shortcut: any later argument counts as the flag, so `python x.py -c` also matches; fine
/// for deny (over-match) and for allow (a prefix rule is refused), not for anything else.
fn runs_inline_code(seg: &[String], shells: bool) -> bool {
    let u = unwrap_words(seg, true);
    let Some(first) = u.first() else { return false };
    let name = first.rsplit(['/', '\\']).next().unwrap_or("");
    let lang = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    let flag = |f: fn(&str) -> bool| u[1..].iter().any(|x| f(x));
    let cluster =
        |x: &str, set: &[char]| x.starts_with('-') && !x.starts_with("--") && x[1..].contains(set);
    match lang {
        "python" | "pypy" => flag(is_dash_c),
        "node" | "nodejs" | "bun" => u[1..]
            .iter()
            .any(|x| matches!(x.as_str(), "-e" | "-p" | "--eval" | "--print")),
        "perl" | "ruby" => u[1..].iter().any(|x| cluster(x, &['e', 'E'])),
        "php" => u[1..].iter().any(|x| x == "-r"),
        _ => shells && ((SHELLS.contains(&name) && flag(is_dash_c)) || name == "eval"),
    }
}

/// Redirect targets in one segment (`>f`, `> f`, `2>f`, `&>f`, `cat>f`): over-approximate, for
/// the protected-path scan.
pub fn redirect_targets(seg: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for (i, w) in seg.iter().enumerate() {
        if let Some(k) = w.rfind('>') {
            let rest = &w[k + 1..];
            if !rest.is_empty() {
                out.push(rest.to_string());
            } else if let Some(n) = seg.get(i + 1) {
                out.push(n.clone());
            }
        }
    }
    out
}

const KEYWORDS: &[&str] = &[
    "do", "then", "else", "elif", "if", "while", "until", "!", "{", "}",
];

/// Command strings a segment may run as: as written and unwrapped (assignments, sudo, env,
/// timeout, xargs ...), first word also as basename (`/bin/rm`) and with backslashes removed (`r\m`).
fn candidates(seg: &[String]) -> Vec<String> {
    let skip = seg
        .iter()
        .take_while(|w| KEYWORDS.contains(&w.as_str()))
        .count();
    let seg = &seg[skip..];
    let mut out = Vec::new();
    for w in [seg.to_vec(), unwrap_words(seg, true)] {
        let Some(first) = w.first() else { continue };
        out.push(w.join(" "));
        let bare = first.replace('\\', "");
        for name in [
            first.rsplit(['/', '\\']).next().unwrap_or(""),
            bare.as_str(),
            bare.rsplit('/').next().unwrap_or(""),
        ] {
            let mut x = w.clone();
            x[0] = name.to_string();
            out.push(x.join(" "));
        }
    }
    out
}

/// `Bash(x:*)`: prefix test for deny/ask (over-matching is the safe side); for allow
/// (`boundary`) the prefix must end at a word boundary: `ls:*` matches `ls -l`, not `lsblk`.
fn spec_matches(spec: &str, cand: &str, boundary: bool) -> bool {
    if spec == "*" {
        return true;
    }
    match spec.strip_suffix(":*") {
        Some(p) if boundary => {
            cand == p || (!p.is_empty() && cand.strip_prefix(p).is_some_and(|r| r.starts_with(' ')))
        }
        Some(p) => cand.starts_with(p),
        None if spec.contains('*') => glob_match(spec, cand),
        None => cand == spec,
    }
}

/// `*` matches any run of characters (official `Bash(git * main)`, `Bash(ls*)`).
fn glob_match(pat: &str, s: &str) -> bool {
    match pat.split_once('*') {
        None => pat == s,
        Some((head, rest)) => s.strip_prefix(head).is_some_and(|t| {
            (0..=t.len())
                .filter(|&i| t.is_char_boundary(i))
                .any(|i| glob_match(rest, &t[i..]))
        }),
    }
}

/// `Bash(spec)` rules; a bare `Bash` (or `*`) rule applies to every Bash call. Quoted words in
/// a spec compare by their unquoted value, like the words of the parsed command.
fn bash_specs(rules: &[String]) -> Vec<String> {
    let unquote = |s: &str| {
        // official `Bash(ls *)` is the word-boundary prefix rule `ls:*`
        let s = match s.strip_suffix(" *") {
            Some(p) if !p.contains('*') => format!("{p}:*"),
            _ => s.to_string(),
        };
        let s = s.as_str();
        if !s.contains(['\'', '"']) {
            return s.to_string();
        }
        let (head, tail) = s.strip_suffix(":*").map_or((s, ""), |h| (h, ":*"));
        match parse_simple_commands(head).as_deref() {
            Some([w]) => format!("{}{tail}", w.join(" ")),
            _ => s.to_string(),
        }
    };
    rules
        .iter()
        .filter_map(|r| match parse_rule(r) {
            ("*", _) | ("Bash", None) => Some("*".to_string()),
            ("Bash", Some(spec)) => Some(unquote(spec)),
            _ => None,
        })
        .collect()
}

/// Allow semantics. Needs the strict parser to succeed and EVERY simple command to match some
/// rule on a word boundary; otherwise allow rules are ignored (the caller then asks). A bare
/// `Bash` / `Bash(*)` allow is the user's explicit allow-all and needs neither. A command that
/// runs inline code (`bash -c`, `eval`, `python -c`, `node -e`) is allowed by an exact spec only,
/// never by a `prefix:*` rule.
pub fn bash_all_match(rules: &[String], command: &str) -> bool {
    bash_all_match_or(rules, command, |_| false)
}

/// [`bash_all_match`] where a simple command that no rule covers still passes when `also` says it
/// is safe (read-only filters after an allowed `node --test`). `also` sees segments in order.
pub fn bash_all_match_or(
    rules: &[String],
    command: &str,
    mut also: impl FnMut(&[String]) -> bool,
) -> bool {
    let specs = bash_specs(rules);
    if specs.iter().any(|s| s == "*") {
        return true;
    }
    let Some(segs) = parse_simple_commands(command) else {
        return false;
    };
    !segs.is_empty()
        && !specs.is_empty()
        && segs.iter().all(|w| {
            let j = w.join(" ");
            // `time`, `timeout 5`, `nohup`, `stdbuf -oL`, `command` do not change what runs
            let unwrapped = unwrap_words(w, false).join(" ");
            let inline = runs_inline_code(w, true);
            // `also` runs for every segment so it can track state such as `cd`
            let safe = also(w);
            specs.iter().any(|sp| {
                !(inline && sp.contains('*'))
                    && (spec_matches(sp, &j, true) || spec_matches(sp, &unwrapped, true))
            }) || safe
        })
}

/// Deny/ask semantics: ANY segment (see `deep_segments`, wrappers unwrapped) matching ANY rule.
/// Fail closed: an unparsable command, a command name that is only known at run time
/// (`$CMD`, `r?`) or code in another language on the command line (`python -c`) matches.
pub fn bash_any_match(rules: &[String], command: &str) -> bool {
    let specs = bash_specs(rules);
    if specs.is_empty() {
        return false;
    }
    if specs.iter().any(|s| s == "*") {
        return true;
    }
    let Some(segs) = deep_segments(command, 0) else {
        return true;
    };
    segs.iter().any(|seg| {
        let u = unwrap_words(seg, true);
        u.first()
            .is_some_and(|f| f.contains(['$', '*', '?', '[', '{', '`']))
            || runs_inline_code(seg, false)
            || candidates(seg)
                .iter()
                .any(|c| specs.iter().any(|sp| spec_matches(sp, c, false)))
    })
}

#[cfg(test)]
mod space_star_tests {
    use super::{bash_all_match, bash_any_match};

    fn r(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn space_star_rules_match_like_official_syntax() {
        for (rule, cmd) in [
            ("Bash(git push *)", "git push origin main"),
            ("Bash(git push *)", "echo a && git push origin"),
            ("Bash(touch *)", "touch x"),
            ("Bash(ls *)", "ls -l"),
            ("Bash(ls*)", "lsof -i"),
            ("Bash(git * main)", "git push origin main"),
            ("Bash(npm run *)", "npm run build"),
        ] {
            assert!(bash_any_match(&r(&[rule]), cmd), "{rule} vs {cmd}");
        }
        assert!(!bash_any_match(
            &r(&["Bash(git * main)"]),
            "git push origin dev"
        ));
        assert!(!bash_any_match(&r(&["Bash(git push *)"]), "git status"));
        assert!(bash_all_match(&r(&["Bash(ls *)"]), "ls -l"));
        assert!(bash_all_match(&r(&["Bash(ls *)"]), "ls"));
        assert!(!bash_all_match(&r(&["Bash(ls *)"]), "lsof -i"));
        assert!(bash_all_match(&r(&["Bash(ls*)"]), "lsof -i"));
        assert!(bash_all_match(&r(&["Bash(git * main)"]), "git merge main"));
        assert!(!bash_all_match(&r(&["Bash(git * main)"]), "git merge dev"));
        assert!(bash_all_match(
            &r(&["Bash(ls)", "Bash(npm run *)"]),
            "ls && npm run build"
        ));
        assert!(!bash_all_match(&r(&["Bash(ls *)"]), "ls && curl x"));
        assert!(bash_all_match(&r(&["Bash(git diff:*)"]), "git diff HEAD"));
        assert!(!bash_all_match(
            &r(&["Bash(python *)"]),
            "python -c 'print(1)'"
        ));
        // wrappers are stripped for allow; a trailing operator or a huge command is unparseable
        let git = r(&["Bash(git status:*)"]);
        for c in [
            "time git status",
            "timeout 5 git status",
            "nohup git status",
            "stdbuf -oL git status",
            "command git status",
        ] {
            assert!(bash_all_match(&git, c), "{c}");
        }
        assert!(!bash_all_match(&git, "git status &&"));
        assert!(!bash_all_match(&git, "git status |"));
        assert!(!bash_all_match(
            &git,
            &format!("git status {}", "a ".repeat(6000))
        ));
    }
}
