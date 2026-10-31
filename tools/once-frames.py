#!/usr/bin/env python3
"""Frame dumps of `synth --once` (stdlib only): the welcome frame at four sizes (plain text, numbered
rows) and at 120x36 in every theme (ANSI text, run in a pty so the colour codes are kept).

Usage: python3 tools/once-frames.py [--bin target/debug/synth] [--out DIR]  (default: <tmp>/synthesis-once-frames)
The user settings of each run live in a throwaway SYNTHESIS_HOME; the process working directory
and your own settings are never used.
"""
import os
import pty
import re
import subprocess
import sys
import tempfile

THEMES = ["synthesis", "classic", "graphite", "midnight", "high-contrast", "pearl", "mono"]
COMPARE = ["synthesis", "classic", "graphite"]  # also at 140x40, for the contact sheet
SIZES = [(80, 24), (100, 30), (140, 40), (200, 50)]


def run(binary, repo, home, cols, lines, tty, extra_env=None):
    env = {"PATH": os.environ.get("PATH", ""), "HOME": home, "SYNTHESIS_HOME": home, "COLUMNS": str(cols),
           "LINES": str(lines), "TERM": "xterm-256color", "COLORTERM": "truecolor"}
    env.update(extra_env or {})
    argv = [binary, "--once", "--cwd", repo]
    if not tty:
        return subprocess.run(argv, env=env, stdin=subprocess.DEVNULL, capture_output=True, check=True).stdout.decode()
    master, slave = pty.openpty()
    p = subprocess.Popen(argv, env=env, stdin=subprocess.DEVNULL, stdout=slave, stderr=subprocess.DEVNULL)
    os.close(slave)
    out = b""
    while True:
        try:
            chunk = os.read(master, 65536)
        except OSError:
            break
        if not chunk:
            break
        out += chunk
    p.wait()
    os.close(master)
    return out.decode().replace("\r\n", "\n")


def main(argv):
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    binary = os.path.join(root, "target/debug/synth")
    out = os.path.join(tempfile.gettempdir(), "synthesis-once-frames")
    it = iter(argv)
    for a in it:
        if a == "--bin":
            binary = os.path.abspath(next(it))
        elif a == "--out":
            out = os.path.abspath(next(it))
    os.makedirs(os.path.join(out, "themes"), exist_ok=True)
    with tempfile.TemporaryDirectory() as tmp:
        repo = os.path.join(tmp, "qa-repo")
        os.mkdir(repo)
        subprocess.run(["git", "init", "-q", repo], check=True)
        home = os.path.join(tmp, "home")
        os.mkdir(home)
        for cols, rows in SIZES:
            text = run(binary, repo, home, cols, rows, tty=False)
            lines = text.rstrip("\n").split("\n")
            body = "\n".join(f"{i:02d}|{l}" for i, l in enumerate(lines))
            open(os.path.join(out, f"{cols}x{rows}.txt"), "w").write(
                f"===== {cols}x{rows} welcome (synth --once, default theme) [{cols}x{rows}]\n{body}\n")
        for theme in THEMES:
            with open(os.path.join(home, "settings.json"), "w") as f:
                f.write('{"synthesis": {"tui": {"theme": "%s"}}}' % theme)
            text = run(binary, repo, home, 120, 36, tty=True)
            open(os.path.join(out, "themes", f"{theme}.ansi.txt"), "w").write(text)
            if theme in COMPARE:
                text140 = run(binary, repo, home, 140, 40, tty=True)
                open(os.path.join(out, "themes", f"compare-140x40-{theme}.ansi.txt"), "w").write(text140)
            plain = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", text)
            print(f"{theme}: {len(text.splitlines())} rows, {text.count(chr(27) + '[')} escapes, plain {len(plain)} bytes")


if __name__ == "__main__":
    main(sys.argv[1:])
