# TUI reference

The two PNG mockups that used to live here were removed (owner, 2026-10-31). The visual source of truth
for the terminal UI is now the owner's HTML prototype, kept OUTSIDE this repository:

    ~/Vibe/synthesis-cli-ui/   (index.html, assets/, screenshots/view1.png, screenshots/view2.png)

Two views: **Mission** (view 1) and **Chat** (view 2). Colour tokens are the `:root` variables of `index.html`
(bg `#0B0C0F`, surface `#15171B`, elevated `#242830`, border `#606978`, text `#C4CAD6`, strong `#F2F4F8`,
accent `#D9D9E0`, green `#4ade80`, amber `#e0b252`, red `#e5636b`, blue `#6ea8ff`, teal `#4fd1c5`,
purple `#b794f6`, pink `#f472b6`, orange `#f59e42`). `docs/specs/tui-design.md` describes how the TUI follows it.
