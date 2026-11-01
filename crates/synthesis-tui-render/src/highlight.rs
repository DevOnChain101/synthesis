//! Small dependency-free lexer for code shown in the TUI (port of `highlight.js`).
//!
//! Colour identity matters: Node merges neighbouring tokens only when they share the same
//! colour *object*, and `keyword` / `operator` have equal values but are different objects, so
//! [`Col`] carries a tag next to the RGB value.

use crate::theme::Rgb;
use regex::Regex;
use std::collections::HashSet;
use std::sync::OnceLock;

/// An RGB colour plus the identity tag described in the module docs (0 for every ordinary
/// colour). Two runs are merged only when their `Col`s are equal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Col {
    pub rgb: Rgb,
    pub tag: u8,
}

impl From<Rgb> for Col {
    fn from(rgb: Rgb) -> Self {
        Col { rgb, tag: 0 }
    }
}

pub const fn col(rgb: Rgb) -> Col {
    Col { rgb, tag: 0 }
}

pub const FG: Col = col([248, 248, 242]);
pub const KEYWORD: Col = col([249, 38, 114]);
pub const STORAGE: Col = col([102, 217, 239]);
pub const BUILTIN: Col = col([166, 226, 46]);
pub const LITERAL: Col = col([190, 132, 255]);
pub const STRING: Col = col([230, 219, 116]);
pub const COMMENT: Col = col([117, 113, 94]);
pub const VARIABLE: Col = col([255, 255, 255]);
/// Same value as [`KEYWORD`], a different object in Node.
pub const OPERATOR: Col = Col {
    rgb: [249, 38, 114],
    tag: 2,
};
pub const ADD: Col = col([80, 200, 80]);
pub const DEL: Col = col([220, 90, 90]);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tok {
    pub t: String,
    pub c: Col,
}

fn group_of(lang: &str) -> Option<&'static str> {
    const TABLE: [(&str, &str); 12] = [
        ("js", "js javascript jsx mjs cjs node ts typescript tsx"),
        ("py", "py python python3"),
        ("sh", "sh bash zsh shell console"),
        ("go", "go golang"),
        ("rust", "rs rust"),
        (
            "clike",
            "java c h cpp c++ cc hpp cs csharp kotlin kt swift php scala dart",
        ),
        ("sql", "sql"),
        ("json", "json jsonc json5"),
        ("yaml", "yaml yml toml ini"),
        ("hcl", "tf hcl terraform tfvars"),
        ("diff", "diff patch"),
        ("", ""),
    ];
    let l = lang.to_lowercase();
    TABLE
        .iter()
        .find(|(g, names)| !g.is_empty() && names.split(' ').any(|n| n == l))
        .map(|(g, _)| *g)
}

/// Language group of a file name by extension (`langFromFile`).
pub fn lang_from_file(file: Option<&str>) -> Option<&'static str> {
    let f = file?;
    let dot = f.rfind('.')?;
    let ext = &f[dot + 1..];
    if ext.is_empty() || ext.contains(['\\', '/', '.']) {
        return None;
    }
    Some(match ext.to_lowercase().as_str() {
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" => "js",
        "py" => "py",
        "sh" | "bash" | "zsh" => "sh",
        "go" => "go",
        "rs" => "rust",
        "java" | "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "cs" | "kt" | "swift" | "php"
        | "scala" | "dart" => "clike",
        "sql" => "sql",
        "json" | "jsonc" | "json5" => "json",
        "yaml" | "yml" | "toml" | "ini" => "yaml",
        "tf" | "hcl" | "tfvars" => "hcl",
        "diff" | "patch" => "diff",
        _ => return None,
    })
}

struct Cfg {
    key: &'static str,
    built: &'static str,
    comment: &'static str,
    block: bool,
    tick: bool,
    triple: bool,
    shell: bool,
}

fn cfg_of(group: &str) -> Cfg {
    let base = Cfg {
        key: "",
        built: "",
        comment: "//",
        block: false,
        tick: false,
        triple: false,
        shell: false,
    };
    match group {
        "js" => Cfg {
            key: "if else for while do switch case break continue return throw try catch finally new import export default async await yield in instanceof typeof void delete with debugger extends static get set public private protected readonly declare abstract from of as",
            built: "String Number Boolean Object Array Map Set Promise Date RegExp Error console require Math JSON Symbol BigInt",
            block: true,
            tick: true,
            ..base
        },
        "py" => Cfg {
            key: "if elif else for while break continue return yield raise try except finally with as import from pass lambda global nonlocal assert async await match case and or not is in",
            built: "str int float bool list dict set tuple range len print open input object Exception",
            comment: "#",
            triple: true,
            ..base
        },
        "sh" => Cfg {
            key: "if then else elif fi for while until do done case esac in function select time",
            built: "echo printf cd pwd export readonly local unset test true false",
            comment: "#",
            shell: true,
            ..base
        },
        "go" => Cfg {
            key: "if else for range switch case default break continue return defer go select fallthrough package import",
            built: "string int int64 float64 bool byte rune error make new append len cap close delete copy",
            block: true,
            tick: true,
            ..base
        },
        "rust" => Cfg {
            key: "if else for while loop match break continue return pub use mod crate where move async await unsafe extern ref mut",
            built: "String Vec Option Result Some None Ok Err usize i32 u32 bool Self",
            block: true,
            ..base
        },
        "clike" => Cfg {
            key: "if else for while do switch case default break continue return throw try catch finally new import package public private protected static final extends implements using virtual override",
            built: "String Integer Boolean Object List Map Set void int long double float char bool",
            block: true,
            ..base
        },
        "sql" => Cfg {
            key: "select from where join left right inner outer on insert into update delete create alter drop table view as and or not order by group having limit offset union all distinct values set",
            built: "count sum avg min max coalesce",
            comment: "--",
            ..base
        },
        "yaml" => Cfg {
            comment: "#",
            ..base
        },
        "hcl" => Cfg {
            key: "resource variable output module provider terraform data locals dynamic for_each count",
            comment: "#",
            ..base
        },
        _ => base, // json
    }
}

fn words(s: &str) -> HashSet<&str> {
    s.split(' ').filter(|w| !w.is_empty()).collect()
}

const STORAGE_WORDS: &str = "const let var function class type interface enum namespace module def fn func struct trait impl";
const COMMON_LITERAL: &str =
    "true false null nil None True False undefined NaN Infinity yes no on off";

struct Res {
    num: Regex,
    op: Regex,
    sh_op: Regex,
    shvar: Regex,
    deco: Regex,
    include: Regex,
    rust_char: Regex,
    call: Regex,
    key: Regex,
}

fn res() -> &'static Res {
    static R: OnceLock<Res> = OnceLock::new();
    R.get_or_init(|| Res {
        num: Regex::new(
            r"^(?:0[xX][0-9a-fA-F]+|0[bB][01]+|(?:\d+\.?\d*|\.\d+)(?:[eE][+-]?\d+)?)[uUlLfFnN]*",
        )
        .unwrap(),
        op: Regex::new(
            r"^(=>|->|===|!==|==|!=|<=|>=|&&|\|\||\?\?|\+\+|--|\*\*|=|\+|-|\*|%|!|&|\||\^|~|\?)",
        )
        .unwrap(),
        sh_op: Regex::new(r"^(&&|\|\||\|)").unwrap(),
        shvar: Regex::new(r"^\$(?:\{[A-Za-z_]\w*\}|[A-Za-z_]\w*|\d+|[?$])").unwrap(),
        deco: Regex::new(r"^@[A-Za-z_$][\w$]*").unwrap(),
        include: Regex::new(r"^#\s*(include|define)\b").unwrap(),
        rust_char: Regex::new(r"^'(?:\\.|[^'\\])'").unwrap(),
        call: Regex::new(r"^\s*(?:\(|!\()").unwrap(),
        key: Regex::new(
            r#"^\s*(?:[,{]\s*)?(?:"([^"\\]*(?:\\.[^"\\]*)*)"|([A-Za-z_][\w.-]*))\s*[:=]"#,
        )
        .unwrap(),
    })
}

fn push(out: &mut Vec<(String, Col)>, t: &str, c: Col) {
    if t.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut()
        && last.1 == c
    {
        last.0.push_str(t);
        return;
    }
    out.push((t.to_string(), c));
}

fn is_id_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '$'
}
fn is_id(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

fn soft_plain(word: &str, line: &[char], at: usize) -> bool {
    if !"type from of as readonly declare abstract namespace"
        .split(' ')
        .any(|w| w == word)
    {
        return false;
    }
    let next: Vec<char> = line.iter().skip(at).take(64).copied().collect();
    let first = next
        .iter()
        .find(|c| !synthesis_tui_base::text::is_js_space(**c));
    match first {
        None => true,
        Some(c) => ":,)=.;(}]".contains(*c),
    }
}

fn find(line: &[char], pat: &str, from: usize) -> Option<usize> {
    let p: Vec<char> = pat.chars().collect();
    if p.is_empty() || line.len() < p.len() {
        return None;
    }
    (from..=line.len() - p.len()).find(|&i| line[i..i + p.len()] == p[..])
}

fn starts(rest: &str, p: &str) -> bool {
    rest.starts_with(p)
}

/// Highlights `code` (lines are split on LF after CRLF/CR normalisation). Unknown languages give
/// one plain token per non-empty line.
#[allow(unused_assignments)]
pub fn highlight(code: &str, lang: &str) -> Vec<Vec<Tok>> {
    let group = group_of(lang);
    let norm = code.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = norm.split('\n').collect();
    let plain = |line: &str, c: Col| {
        if line.is_empty() {
            vec![]
        } else {
            vec![Tok {
                t: line.to_string(),
                c,
            }]
        }
    };
    let Some(group) = group else {
        return lines.iter().map(|l| plain(l, FG)).collect();
    };
    if group == "diff" {
        return lines
            .iter()
            .map(|l| {
                let c = match l.chars().next() {
                    Some('+') => ADD,
                    Some('-') => DEL,
                    _ if l.starts_with("@@") => STORAGE,
                    _ if ["diff ", "index ", "+++", "---"]
                        .iter()
                        .any(|p| l.starts_with(p)) =>
                    {
                        COMMENT
                    }
                    _ => FG,
                };
                plain(l, c)
            })
            .collect();
    }
    let cfg = cfg_of(group);
    let r = res();
    let key = words(cfg.key);
    let built = words(cfg.built);
    let storage = words(STORAGE_WORDS);
    let common = words(COMMON_LITERAL);
    let mut state: Option<&str> = None;
    let mut title_next = false;
    let mut result = Vec::with_capacity(lines.len());
    for text in lines {
        let line: Vec<char> = text.chars().collect();
        let n = line.len();
        let mut out: Vec<(String, Col)> = Vec::new();
        let mut i = 0usize;
        let slice = |a: usize, b: usize| -> String { line[a..b.min(n)].iter().collect() };
        while i < n {
            // helper: emit line[i..end] with colour c, advance
            macro_rules! emit {
                ($end:expr, $c:expr) => {{
                    let end: usize = $end;
                    push(&mut out, &slice(i, end), $c);
                    i = end;
                }};
            }
            if state == Some("block") {
                match find(&line, "*/", i) {
                    None => {
                        emit!(n, COMMENT);
                        break;
                    }
                    Some(e) => {
                        emit!(e + 2, COMMENT);
                        state = None;
                        continue;
                    }
                }
            }
            if let Some(st) = state.filter(|s| ["tick", "\"\"\"", "'''"].contains(s)) {
                let close = if st == "tick" { "`" } else { st };
                match find(&line, close, i) {
                    None => {
                        emit!(n, STRING);
                        break;
                    }
                    Some(e) => {
                        emit!(e + close.chars().count(), STRING);
                        state = None;
                        continue;
                    }
                }
            }
            let rest = slice(i, i + 64);
            if cfg.block && starts(&rest, "/*") {
                match find(&line, "*/", i + 2) {
                    None => {
                        emit!(n, COMMENT);
                        state = Some("block");
                        break;
                    }
                    Some(e) => {
                        emit!(e + 2, COMMENT);
                        continue;
                    }
                }
            }
            if cfg.triple && (starts(&rest, "\"\"\"") || starts(&rest, "'''")) {
                let q: &'static str = if starts(&rest, "\"\"\"") {
                    "\"\"\""
                } else {
                    "'''"
                };
                match find(&line, q, i + 3) {
                    None => {
                        emit!(n, STRING);
                        state = Some(q);
                        break;
                    }
                    Some(e) => {
                        emit!(e + 3, STRING);
                        continue;
                    }
                }
            }
            let comment_ok = !cfg.comment.is_empty()
                && starts(&rest, cfg.comment)
                && (cfg.comment != "#"
                    || i == 0
                    || synthesis_tui_base::text::is_js_space(line[i - 1]));
            if comment_ok {
                emit!(n, COMMENT);
                break;
            }
            if cfg.tick && line[i] == '`' {
                match find(&line, "`", i + 1) {
                    None => {
                        emit!(n, STRING);
                        state = Some("tick");
                        break;
                    }
                    Some(e) => {
                        emit!(e + 1, STRING);
                        continue;
                    }
                }
            }
            if line[i] == '"'
                || (line[i] == '\'' && !(group == "rust" && !r.rust_char.is_match(&rest)))
            {
                let q = line[i];
                let mut p = i + 1;
                while p < n && line[p] != q {
                    p += if line[p] == '\\' { 2 } else { 1 };
                }
                if p < n {
                    emit!(p + 1, STRING);
                    continue;
                }
            }
            if cfg.shell
                && line[i] == '$'
                && let Some(m) = r.shvar.find(&rest)
            {
                emit!(i + m.as_str().chars().count(), VARIABLE);
                continue;
            }
            if matches!(group, "js" | "py" | "clike")
                && line[i] == '@'
                && let Some(m) = r.deco.find(&rest)
            {
                emit!(i + m.as_str().chars().count(), COMMENT);
                continue;
            }
            if group == "clike" && i == 0 && r.include.is_match(&rest) {
                emit!(n, COMMENT);
                break;
            }
            if let Some(m) = r.num.find(&rest) {
                emit!(i + m.as_str().chars().count(), LITERAL);
                continue;
            }
            if is_id_start(line[i]) {
                let mut e = i + 1;
                while e < n && is_id(line[e]) {
                    e += 1;
                }
                let m = slice(i, e);
                let mut q = i as isize - 1;
                while q >= 0 && synthesis_tui_base::text::is_js_space(line[q as usize]) {
                    q -= 1;
                }
                let prev = if q >= 0 { Some(line[q as usize]) } else { None };
                let next = slice(e, e + 64);
                let mut c = FG;
                if prev != Some('.') {
                    if title_next {
                        c = BUILTIN;
                        title_next = false;
                    } else if (key.contains(m.as_str()) || storage.contains(m.as_str()))
                        && !soft_plain(&m, &line, e)
                    {
                        c = if storage.contains(m.as_str()) {
                            STORAGE
                        } else {
                            KEYWORD
                        };
                        if storage.contains(m.as_str()) || m == "function" {
                            title_next = true;
                        }
                    } else if common.contains(m.as_str()) {
                        c = LITERAL;
                    } else if ["this", "self", "cls", "super"].contains(&m.as_str()) {
                        c = VARIABLE;
                    } else if built.contains(m.as_str())
                        || (m.starts_with(|ch: char| ch.is_ascii_uppercase())
                            && m.chars().any(|ch| ch.is_ascii_lowercase()))
                        || r.call.is_match(&next)
                    {
                        c = BUILTIN;
                    }
                }
                push(&mut out, &m, c);
                i = e;
                continue;
            }
            if let Some(m) = r.op.find(&rest)
                && !matches!(group, "json" | "yaml")
                && !(group == "sh" && !r.sh_op.is_match(&rest))
            {
                emit!(i + m.as_str().chars().count(), OPERATOR);
                continue;
            }
            push(&mut out, &line[i].to_string(), FG);
            i += 1;
        }
        // JSON/YAML/HCL property names are attributes, including quoted JSON keys.
        if matches!(group, "json" | "yaml" | "hcl")
            && let Some(caps) = r.key.captures(text)
        {
            let key_text = caps
                .get(1)
                .map(|m| m.as_str())
                .filter(|s| !s.is_empty())
                .or_else(|| caps.get(2).map(|m| m.as_str()));
            if let Some(kt) = key_text
                && let Some(at) = find(&line, kt, 0)
            {
                let mut seen = 0;
                for (t, c) in out.iter_mut() {
                    let l = t.chars().count();
                    if seen <= at && at < seen + l {
                        *c = BUILTIN;
                    }
                    seen += l;
                }
            }
        }
        result.push(out.into_iter().map(|(t, c)| Tok { t, c }).collect());
    }
    result
}
