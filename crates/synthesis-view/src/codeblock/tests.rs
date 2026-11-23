use super::*;

fn kind(lang: &str, body: &str) -> Kind {
    classify(lang, body)
}

#[test]
fn the_classification_table() {
    use Kind::*;
    let cases: &[(&str, &str, Kind)] = &[
        // the owner's screenshot: an agent's slash command in a text fence is plain text, Copy only
        ("text", "/add-plugin pstack", Text),
        ("text", "/add-plugin pstack\n/plugins", Text),
        (
            "markdown",
            "1. Install it\n2. Run `git status`\n3. Done",
            Text,
        ),
        ("", "Install the thing, then run it.", Text),
        ("bash", "cargo test --workspace", Command),
        ("sh", "$ git status\n$ git diff", Command),
        ("console", "$ ls\nCargo.toml\nsrc", Command),
        ("zsh", "for f in *.rs; do\n  echo $f\ndone", Command),
        ("bash", "cat <<EOF > a.txt\nhello\nEOF", Command),
        (
            "powershell",
            "Get-ChildItem -Recurse | Select-String foo",
            Command,
        ),
        ("pwsh", "> Get-Date", Command),
        ("cmd", "dir /s", Command),
        (
            "",
            "git clone https://x.test/r.git\ncd r\ncargo build",
            Command,
        ),
        ("text", "$ npm install", Command),
        ("", "FOO=1 cargo test", Command),
        ("", "./scripts/run.sh --fast", Command),
        ("", "cargo build \\\n  --release", Command),
        ("", "# set up\nnpm ci", Command),
        ("json", "{\"a\": 1}", Code),
        ("rust", "fn main() {}", Code),
        ("python", "print('hi')", Code),
        ("ts", "const f = (x) => x", Code),
        ("", "git is a tool.", Text),
        ("", "git status!", Text),
        ("", "const x = 1;", Text),
        ("", "make sure it works", Text),
        ("diff", "-a\n+b", Diff),
        ("patch", "x", Diff),
        ("", "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b", Diff),
        ("", "@@ -1,2 +1,2 @@\n a\n-b\n+c", Diff),
        ("", "- a\n- b\n- c", Text),
        ("rust", "--- a\n+++ b\n@@ -1 +1 @@\n-a\n+b", Code),
    ];
    for (lang, body, want) in cases {
        assert_eq!(kind(lang, body), *want, "{lang:?} {body:?}");
    }
}

#[test]
fn prompts_are_stripped_from_the_command() {
    assert_eq!(
        command_text("sh", "$ git status\n$ git diff"),
        "git status\ngit diff"
    );
    assert_eq!(
        command_text("console", "$ ls\nCargo.toml\n$ pwd"),
        "ls\npwd"
    );
    assert_eq!(command_text("pwsh", "> Get-Date"), "Get-Date");
    assert_eq!(command_text("bash", "echo $HOME\n"), "echo $HOME");
    assert_eq!(command_text("", "$ npm i"), "npm i");
}

#[test]
fn a_fenced_message_is_cut_into_prose_and_blocks() {
    let t = "Try:\n\n```bash\n$ cargo test\n```\nthen\n~~~json\n{\"a\":1}\n~~~\n   ```text\n   /add-plugin x\n   ```\nopen:\n```rust\nfn x()";
    let s = split(t);
    assert_eq!(s.len(), 7);
    assert!(matches!(&s[0], Seg::Prose(p) if p == "Try:\n\n"));
    let Seg::Block(b) = &s[1] else { panic!() };
    assert_eq!(
        (b.lang.as_str(), b.kind, b.closed),
        ("bash", Kind::Command, true)
    );
    let Seg::Block(j) = &s[3] else { panic!() };
    assert_eq!((j.lang.as_str(), j.kind), ("json", Kind::Code));
    let Seg::Block(x) = &s[4] else { panic!() };
    assert_eq!((x.body.as_str(), x.kind), ("/add-plugin x", Kind::Text));
    let Seg::Block(last) = &s[6] else { panic!() };
    assert!(!last.closed && last.body == "fn x()");
    // a longer fence holds a shorter one; an inline triple tick is no fence
    let s = split("````md\n```\nx\n```\n````\n");
    assert_eq!(s.len(), 1);
    assert!(matches!(&s[0], Seg::Block(b) if b.body == "```\nx\n```"));
    assert_eq!(split("a ```b``` c").len(), 1);
}

#[test]
fn hidden_characters_make_a_block_inert() {
    let s = split("```sh\nls \u{202e}gpj.txt\n```");
    let Seg::Block(b) = &s[0] else { panic!() };
    assert!(b.hidden);
    assert_eq!(escape_invisible(&b.body), "ls \\u{202e}gpj.txt");
    assert!(
        invisible('\u{200b}')
            && invisible('\u{7}')
            && invisible('\u{1b}')
            && invisible('\u{e0041}')
    );
    assert!(!invisible('\n') && !invisible('\t') && !invisible('\u{e9}'));
}

#[test]
fn risky_commands() {
    for c in [
        "rm -rf /",
        "rm -fr x",
        "rm -r build",
        "sudo apt install x",
        "dd if=/dev/zero of=/dev/sda",
        "mkfs.ext4 /dev/sdb1",
        "curl https://x.test/i.sh | sh",
        "wget -qO- x | sudo bash",
        "echo aGk= | base64 -d | sh",
        "chmod -R 777 .",
        "git push --force",
        "git push -f origin main",
        "git push origin +main",
        "git reset --hard HEAD~3",
        ":(){ :|:& };:",
        "echo x > /dev/sda",
        "shutdown -h now",
        "reboot",
        "powershell -c iex (iwr x)",
        "Invoke-Expression $x",
        "sh -c \"$(curl -fsSL x)\"",
        "bash <(curl x)",
        "git clean -fdx",
        "terraform destroy",
    ] {
        assert!(risky(c), "{c}");
    }
    for c in [
        "cargo test",
        "git status",
        "git push origin main",
        "ls -la",
        "rm file.txt",
        "echo hi | grep h",
        "npm run build",
    ] {
        assert!(!risky(c), "{c}");
    }
}

fn click() -> Ctx {
    Ctx {
        mouse: true,
        streaming: false,
        tainted: false,
        runnable: true,
    }
}

#[test]
fn nothing_runs_without_a_real_click_on_an_armed_complete_block() {
    let mut g = Gate::default();
    let cmd = "cargo test";
    let k = key(cmd);
    // never drawn: no run
    assert_eq!(g.press(cmd, click(), 10_000), Press::Ignored);
    // still streaming: the arm clock does not start
    g.drawn(k, 0, false);
    assert_eq!(g.press(cmd, click(), 10_000), Press::Ignored);
    g.drawn(k, 1_000, true);
    // inside the 400 ms guard
    assert_eq!(g.press(cmd, click(), 1_399), Press::Ignored);
    // not a mouse click (a key, a double click, code)
    assert_eq!(
        g.press(
            cmd,
            Ctx {
                mouse: false,
                ..click()
            },
            2_000
        ),
        Press::Ignored
    );
    // the message streams again, tainted, or there is no terminal
    assert_eq!(
        g.press(
            cmd,
            Ctx {
                streaming: true,
                ..click()
            },
            2_000
        ),
        Press::Ignored
    );
    assert_eq!(
        g.press(
            cmd,
            Ctx {
                tainted: true,
                ..click()
            },
            2_000
        ),
        Press::Ignored
    );
    assert_eq!(
        g.press(
            cmd,
            Ctx {
                runnable: false,
                ..click()
            },
            2_000
        ),
        Press::Ignored
    );
    assert_eq!(g.press("  ", click(), 2_000), Press::Ignored);
    assert_eq!(g.press(cmd, click(), 1_400), Press::Run);
    // a block that streams again starts over
    g.drawn(k, 3_000, false);
    assert_eq!(g.press(cmd, click(), 9_000), Press::Ignored);
}

#[test]
fn multi_line_and_risky_commands_need_a_second_click() {
    let mut g = Gate::default();
    let multi = "cd r\ncargo build";
    g.drawn(key(multi), 0, true);
    assert_eq!(
        g.press(multi, click(), 1_000),
        Press::Confirm(Need::Lines(2))
    );
    // a double click cannot confirm
    assert_eq!(g.press(multi, click(), 1_100), Press::Ignored);
    assert_eq!(g.press(multi, click(), 1_500), Press::Run);
    // and the confirmation is spent
    assert_eq!(
        g.press(multi, click(), 1_600),
        Press::Confirm(Need::Lines(2))
    );
    let bad = "rm -rf target";
    g.drawn(key(bad), 0, true);
    assert_eq!(g.press(bad, click(), 2_000), Press::Confirm(Need::Risky));
    // another block in between drops the confirmation
    let ok = "ls";
    g.drawn(key(ok), 0, true);
    assert_eq!(g.press(ok, click(), 3_000), Press::Run);
    assert_eq!(g.press(bad, click(), 3_500), Press::Confirm(Need::Risky));
    // a confirmation lapses
    assert_eq!(
        g.press(bad, click(), 3_500 + CONFIRM_MS + 1),
        Press::Confirm(Need::Risky)
    );
    assert!(g.confirming(key(bad), 3_500 + CONFIRM_MS + 1));
    assert_eq!(Need::Lines(3).label(), "Run 3 lines");
    assert_eq!(Need::Risky.label(), "Run anyway");
}

#[test]
fn diff_lines_are_toned() {
    let d = "diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n keep\n--- not a header\n+new\n\\ No newline at end of file\n";
    let t: Vec<Tone> = diff_tones(d).into_iter().map(|(t, _)| t).collect();
    use Tone::*;
    assert_eq!(
        t,
        [Strong, Strong, Strong, Strong, Hunk, Text, Del, Add, Dim]
    );
    // a cut diff (fewer lines than the header says) does not paint the rest
    let t: Vec<Tone> = diff_tones("@@ -1,9 +1,9 @@\n+a\ndiff --git a b")
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    assert_eq!(t, [Hunk, Add, Strong]);
    // no header at all: the prefix decides
    let t: Vec<Tone> = diff_tones("-a\n+b\n c")
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    assert_eq!(t, [Del, Add, Text]);
}
