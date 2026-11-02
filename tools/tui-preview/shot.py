#!/usr/bin/env python3
"""Screenshot of the REAL `synth` TUI: runs the binary on a pty, feeds its output through a
terminal emulator (pyte) to the final screen grid, writes HTML (one fixed-size cell per character,
24-bit colours kept) and has headless Chrome take the PNG. Dev tool, not part of the build.

  python3 -m venv /tmp/venv && /tmp/venv/bin/pip install pyte
  /tmp/venv/bin/python tools/tui-preview/shot.py scene.json OUT.png

scene.json: {"argv": [...], "env": {...}, "cols": 120, "rows": 40,
             "steps": [["send", "text\\r"], ["wait", 2], ["expect", "text on screen", 15]],
             "chrome": "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"}
"""
import fcntl, html, json, os, pty, select, struct, subprocess, sys, tempfile, termios, time

import pyte

sc = json.load(open(sys.argv[1]))
out_png = sys.argv[2]
cols, rows = sc.get("cols", 120), sc.get("rows", 40)
env = {"PATH": os.environ["PATH"], "TERM": "xterm-256color", "COLORTERM": "truecolor"}
env.update(sc["env"])
pid, fd = pty.fork()
if pid == 0:
    os.execve(sc["argv"][0], sc["argv"], env)
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
screen = pyte.Screen(cols, rows)
stream = pyte.ByteStream(screen)


def pump(secs):
    end = time.time() + secs
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                d = os.read(fd, 65536)
            except OSError:
                return
            if not d:
                return
            stream.feed(d)


for st in sc["steps"]:
    if st[0] == "send":
        os.write(fd, st[1].encode().decode("unicode_escape").encode("latin1"))
        pump(0.3)
    elif st[0] == "wait":
        pump(st[1])
    elif st[0] == "expect":
        end = time.time() + st[2]
        while time.time() < end and st[1] not in "\n".join(screen.display):
            pump(0.2)
        if st[1] not in "\n".join(screen.display):
            print("expect failed:", st[1], file=sys.stderr)
            sys.exit(1)
pump(0.5)
os.kill(pid, 9)
os.waitpid(pid, 0)

BG, FG = "0b0c0f", "9ca3af"
CW, CH, FS = 9, 19, 15  # px per cell, line height, font size


def colour(c, default):
    if c == "default":
        return default
    if len(c) == 6 and all(x in "0123456789abcdefABCDEF" for x in c):
        return c
    names = {"black": "000000", "red": "cd3131", "green": "0dbc79", "brown": "e5e510", "blue": "2472c8",
             "magenta": "bc3fbc", "cyan": "11a8cd", "white": "e5e5e5", "brightblack": "666666",
             "brightred": "f14c4c", "brightgreen": "23d18b", "brightbrown": "f5f543", "brightblue": "3b8eea",
             "brightmagenta": "d670d6", "brightcyan": "29b8db", "brightwhite": "ffffff"}
    return names.get(c, default)


lines = []
for y in range(rows):
    row, run, key = [], "", None
    for x in range(cols):
        ch = screen.buffer[y][x]
        fg, bg = colour(ch.fg, FG), colour(ch.bg, BG)
        if ch.reverse:
            fg, bg = bg, fg
        k = (fg, bg, ch.italics, ch.underscore, ch.bold)
        if k != key:
            if run:
                row.append((key, run))
            key, run = k, ""
        run += ch.data or " "
    row.append((key, run))
    spans = []
    for (fg, bg, it, ul, bd), text in row:
        style = f"color:#{fg};"
        if bg != BG:
            style += f"background:#{bg};"
        if it:
            style += "font-style:italic;"
        if ul:
            style += "text-decoration:underline;"
        if bd:
            style += "font-weight:bold;"
        # a fallback font may have another advance for non-ASCII glyphs: pin each to one cell
        body = "".join(
            html.escape(c) if ord(c) < 128 else f'<i class="c">{c}</i>' for c in text
        )
        spans.append(f'<span style="{style}">{body}</span>')
    lines.append("".join(spans))
page = f"""<!doctype html><meta charset=utf-8><style>
html,body{{margin:0;background:#{BG}}}
pre{{margin:0;padding:8px;font:{FS}px/{CH}px Menlo,'SF Mono',monospace;color:#{FG};background:#{BG};
width:{cols * CW}px;height:{rows * CH}px;overflow:hidden;letter-spacing:0}}
span{{white-space:pre}}
i.c{{display:inline-block;width:{CW}px;font-style:inherit;overflow:visible;white-space:pre}}
</style><pre>{chr(10).join(lines)}</pre>"""
with tempfile.TemporaryDirectory() as d:
    p = os.path.join(d, "s.html")
    open(p, "w").write(page)
    chrome = sc.get("chrome", "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
    subprocess.run([chrome, "--headless=new", f"--screenshot={out_png}",
                    f"--window-size={cols * CW + 16},{rows * CH + 16}", "--hide-scrollbars",
                    "--force-device-scale-factor=2", "file://" + p],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
print("wrote", out_png)
