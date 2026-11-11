//! Every flag that `--help` lists is documented in docs/CLI.md, and every command has a row.

use std::collections::BTreeSet;
use std::process::Command;

/// Listed by `--help` but deliberately not in docs/CLI.md (flag, reason).
const UNDOCUMENTED: [(&str, &str); 2] = [
    ("--help", "printed by every command"),
    ("--version", "printed by every command"),
];

/// `--long` names of the option lines (`--x <V>` or `-x, --x <V>`) of clap's help.
fn flags(help: &str) -> BTreeSet<String> {
    help.lines()
        .map(str::trim_start)
        .filter(|l| l.starts_with("--") || l.starts_with("-") && !l.starts_with("- "))
        .flat_map(|l| l.split_whitespace().take(2))
        .map(|w| w.trim_end_matches(','))
        .filter(|w| w.starts_with("--"))
        .map(String::from)
        .collect()
}

fn help(args: &[&str]) -> String {
    let o = Command::new(env!("CARGO_BIN_EXE_synt"))
        .args(args)
        .arg("--help")
        .output()
        .unwrap();
    assert!(o.status.success(), "{args:?} --help failed");
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn every_flag_and_command_in_help_is_in_the_cli_doc() {
    let doc =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/CLI.md")).unwrap();
    let mut missing = vec![];
    for cmd in [&[][..], &["run"], &["models"], &["rewind"]] {
        for f in flags(&help(cmd)) {
            let allowed = UNDOCUMENTED.iter().any(|(u, _)| *u == f);
            if !allowed && !doc.contains(&format!("`{f}")) && !doc.contains(&format!(", {f}")) {
                missing.push(format!("synt {} {f}", cmd.join(" ")));
            }
        }
    }
    // commands of `synt --help`
    let top = help(&[]);
    let cmds = top
        .lines()
        .skip_while(|l| !l.starts_with("Commands:"))
        .skip(1)
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_whitespace().next());
    for c in cmds.filter(|c| *c != "help") {
        if !doc.contains(&format!("synt {c}")) {
            missing.push(format!("command synt {c}"));
        }
    }
    assert!(missing.is_empty(), "not in docs/CLI.md: {missing:#?}");
}
