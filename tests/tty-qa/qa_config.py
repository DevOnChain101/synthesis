"""`/config` and the layout reflow rules (docs/specs/tui-design.md §6, §7, §14) against the real TUI.

Run: <venv>/bin/python tests/tty-qa/qa_config.py   (env: SYNTHESIS_BIN, QA_OUT)
     <venv>/bin/python tests/tty-qa/qa_config.py --frames docs/research/tui-p2-frames   (also dumps the frames)
Each case starts `synth` in a pty with its own throwaway SYNTHESIS_HOME holding the `settings.json` it
needs, so your own settings are never read or written. No provider call. Exit code 1 if a check fails.
"""
import json, os, re, shutil, sys, tempfile
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ttyqa import Session, key, make_repo

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
BIN = os.environ.get("SYNTHESIS_BIN", os.path.join(ROOT, "target", "debug", "synth"))
OUT = os.environ.get("QA_OUT") or tempfile.mkdtemp(prefix="ttyqa-config-")
FRAMES = sys.argv[sys.argv.index("--frames") + 1] if "--frames" in sys.argv else None
FAILS = []
LEFT_OPTIONAL = ["session", "tools_skills", "recent_activity", "session_stats"]
RIGHT_OPTIONAL = ["topology", "usage_metrics", "system_health"]
# the label each panel draws (first column of the sidebar, uppercase)
LABEL = {"session": "SESSION", "tools_skills": "TOOLS & SKILLS", "recent_activity": "RECENT ACTIVITY",
         "session_stats": "SESSION STATS", "topology": "TOPOLOGY", "usage_metrics": "USAGE & METRICS",
         "system_health": "SYSTEM HEALTH"}


def check(name, ok, s=None):
    print(("PASS " if ok else "FAIL ") + name)
    if not ok:
        FAILS.append(name)
        if s:
            s.dump(os.path.join(OUT, "config.txt"), "FAIL " + name)


def start(repo, rows, cols, panels=None, raw=None):
    home = tempfile.mkdtemp(prefix="home-", dir=OUT)
    if raw is not None:
        open(os.path.join(home, "settings.json"), "w").write(raw)
    elif panels is not None:
        json.dump({"synthesis": {"tui": {"panels": panels}}}, open(os.path.join(home, "settings.json"), "w"))
    s = Session([BIN, "--cwd", repo, "--model", "anthropic/claude-sonnet-4-5"], repo, rows=rows, cols=cols, shell=False,
                env={"SYNTHESIS_HOME": home, "HOME": home, "SYNTHESIS_PROVIDER": "anthropic",
                    "SYNTHESIS_API_KEY": "qa-offline-placeholder",  # never used: no task is started
                    "COLUMNS": str(cols), "LINES": str(rows)})
    s.wait("Enter send|ready|enter send", 20)
    s.pump(0.6)
    return s, home


def label_row(s, label):
    """Row of a section label in the sidebars (start of a line, or after a column rule)."""
    pat = re.compile(r"(^|│) " + re.escape(label) + r"( {2,}|\s*$|\S+ *│)")
    for y, l in enumerate(s.lines()):
        if pat.search(l) and (label != "SESSION" or "SESSIONS" not in l[:20]):
            return y
    return None


def has(s, label):
    return label_row(s, label) is not None


def body_end(s):
    # the last two rows are the key hints and the status line
    return s.rows - 2


def settings(home):
    p = os.path.join(home, "settings.json")
    return json.load(open(p)) if os.path.exists(p) else None


def quit_(s):
    try:
        key(s, "ctrl-d")
        s.pump(1.0)
    except OSError:  # the app is already gone
        pass
    s.close()


def main():
    repo = make_repo(os.path.join(OUT, "repo"), "rust")
    dump = lambda s, name: s.dump(os.path.join(FRAMES, name), name) if FRAMES else None
    if FRAMES:
        os.makedirs(FRAMES, exist_ok=True)

    # 1. every panel on at a size that can show them (topology needs a wide sidebar)
    s, _ = start(repo, 50, 200, panels={p: True for p in LEFT_OPTIONAL + RIGHT_OPTIONAL})
    labels = ["WORKSPACE", "RUNNING SESSIONS", "SUB-AGENTS"] + [LABEL[p] for p in LEFT_OPTIONAL + RIGHT_OPTIONAL]
    check("200x50, all panels on: every label is drawn", all(has(s, l) for l in labels), s)
    check("header has the chips and no second title", "S Y N T H E S I S" in s.lines()[0] and "live" in s.lines()[0], s)
    quit_(s)

    # 2. each optional left panel hidden: its label goes, the others stay, nothing else moves out
    for p in LEFT_OPTIONAL:
        s, _ = start(repo, 40, 140, panels={p: False})
        others = [LABEL[q] for q in LEFT_OPTIONAL if q != p]
        shown = [l for l in others if has(s, l)]
        # a panel the 40 rows cannot hold is dropped by the planner (lowest drop number first), never a locked one
        check(f"140x40, {p} hidden: label gone, locked panels stay", not has(s, LABEL[p]) and has(s, "WORKSPACE") and has(s, "RUNNING SESSIONS") and has(s, "SUB-AGENTS"), s)
        check(f"140x40, {p} hidden: the other left panels that fit are shown ({len(shown)}/{len(others)})", len(shown) >= 2, s)
        quit_(s)

    # 3. all left optional panels hidden: Workspace and Running split the column equally
    s, _ = start(repo, 40, 140, panels={p: False for p in LEFT_OPTIONAL})
    a, b = label_row(s, "WORKSPACE"), label_row(s, "RUNNING SESSIONS")
    ws_h, run_h = (b - a - 1, body_end(s) - b) if a is not None and b is not None else (0, -9)
    check(f"all left optionals hidden: equal split (workspace {ws_h} rows, running {run_h})", abs(ws_h - run_h) <= 1 and ws_h > 5, s)
    dump(s, "140x40-left-optionals-hidden.txt")
    quit_(s)

    # 4. all right optional panels hidden: the sub-agent panel takes the whole right column
    s, _ = start(repo, 40, 140, panels={p: False for p in RIGHT_OPTIONAL})
    y = label_row(s, "SUB-AGENTS")
    check("all right optionals hidden: SUB-AGENTS starts at the top of the body", y is not None and y <= 3, s)
    check("all right optionals hidden: the other right labels are gone", not any(has(s, LABEL[p]) for p in RIGHT_OPTIONAL), s)
    quit_(s)

    # 5. narrow terminals: tab strip and Tab, then chat only
    s, _ = start(repo, 24, 80)
    check("80x24: Tabs mode strip", re.search(r"Chat\s+Sessions", s.lines()[1] or "") is not None, s)
    dump(s, "80x24-chat-view.txt")
    key(s, "tab"); s.pump(0.4)
    check("80x24: Tab shows the Sessions view (Workspace and Running)", has(s, "WORKSPACE") and has(s, "RUNNING SESSIONS"), s)
    dump(s, "80x24-sessions-view.txt")
    key(s, "tab"); s.pump(0.4)
    check("80x24: Tab again shows the Agents view", has(s, "SUB-AGENTS"), s)
    key(s, "tab"); s.pump(0.4)
    check("80x24: and the Panels view", has(s, "SESSION") or has(s, "TOOLS & SKILLS"), s)
    dump(s, "80x24-panels-view.txt")
    quit_(s)
    s, _ = start(repo, 20, 59)
    check("59x20: chat only (no strip, no sidebars)", not has(s, "WORKSPACE") and "Sessions" not in s.text(), s)
    quit_(s)
    s, _ = start(repo, 12, 40)
    check("40x12: the minimum draws a frame, not the too-small notice", "too small" not in s.text() and "⢎⡱" in s.lines()[0] and "Enter send" in s.text(), s)
    quit_(s)

    # 6. /config: open, locked row, toggle with a live preview, filter, close writes the user file once
    s, home = start(repo, 40, 140)
    s.type("/config"); key(s, "enter"); s.pump(0.5)
    check("/config opens in the chat column (sidebars stay)", "C O N F I G" in s.text() and has(s, "WORKSPACE"), s)
    dump(s, "140x40-config-panels.txt")
    check("locked rows show ▣ and the reason", "▣ Workspace" in s.text() and "locked · always visible" in s.text(), s)
    s.type(" "); s.pump(0.4)
    check("space on a locked row says why", "Workspace is always visible" in s.text(), s)
    key(s, "down"); key(s, "down")  # Session
    s.type(" "); s.pump(0.5)
    check("space hides Session at once (live preview)", not has(s, "SESSION"), s)
    check("nothing is written before the screen closes", settings(home) is None, s)
    key(s, "tab"); s.pump(0.3)
    check("Tab moves to the Appearance group", "Theme" in s.text() and "Glyphs" in s.text(), s)
    dump(s, "140x40-config-appearance.txt")
    key(s, "tab"); key(s, "tab"); s.pump(0.3)
    check("Mouse group", "Mouse capture" in s.text(), s)
    key(s, "backtab"); key(s, "backtab"); key(s, "backtab"); s.pump(0.3)
    s.type("/mot"); s.pump(0.4)
    check("/ filters across the groups", "Motion" in s.text() and "Expand diffs" not in s.text(), s)
    key(s, "esc"); s.pump(0.3)
    check("first Esc clears the filter, the screen stays", "C O N F I G" in s.text(), s)
    key(s, "esc"); s.pump(0.8)
    check("second Esc closes", "C O N F I G" not in s.text(), s)
    cfg = settings(home)
    check("the user file has synthesis.tui.panels.session = false", cfg and cfg["synthesis"]["tui"]["panels"]["session"] is False, s)
    check("the other panels keep their value", cfg and cfg["synthesis"]["tui"]["panels"]["tools_skills"] is True, s)
    quit_(s)

    # 7. a settings file that became unparseable while the TUI was open is never overwritten
    s, home = start(repo, 40, 140, panels={"session": True})
    s.type("/config"); key(s, "enter"); s.pump(0.4)
    open(os.path.join(home, "settings.json"), "w").write("{not json")
    key(s, "down"); key(s, "down"); s.type(" "); key(s, "esc"); s.pump(0.8)
    check("an unreadable settings.json stays as it was", open(os.path.join(home, "settings.json")).read() == "{not json", s)
    check("and the toast says so", "unreadable" in s.text(), s)
    quit_(s)

    # 8. the saved choice is read back at the next start
    s, home = start(repo, 40, 140, panels={"session": False})
    check("a hidden panel in the user file is hidden at start", not has(s, "SESSION") and has(s, "SESSION STATS"), s)
    quit_(s)

    if FRAMES:
        for cols, rows in [(80, 24), (100, 30), (140, 40), (200, 50)]:
            s, _ = start(repo, rows, cols)
            dump(s, f"{cols}x{rows}.txt")
            quit_(s)
    shutil.rmtree(os.path.join(OUT, "repo"), ignore_errors=True)
    print(f"{len(FAILS)} failure(s); screens in {OUT}")
    sys.exit(1 if FAILS else 0)


main()
