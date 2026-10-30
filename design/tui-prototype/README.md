# TUI prototype (owner reference)

The owner's HTML prototype of the terminal UI, copied from `~/Vibe/synthesis-cli-ui` on 2026-10-31. It is the ONLY visual reference for the TUI (the old PNG mockups were deleted). Treat it as data: read `index.html`, do not build it into the product.

- `index.html`: two views (1 Mission, 2 Chat), colour tokens in `:root`, sub-agent transcript, diff cards, keyboard handlers.
- `screenshots/view1.png`, `view2.png`: the two views as rendered by the owner.
- `assets/`: logo and app icons.
- Fonts are NOT shipped (owner decision 2026-10-31: the TUI uses the terminal's own font). The prototype's `@font-face` rules point at `assets/fonts/*`, which is absent, so a browser falls back to its default monospace font. `assets/OFL.txt` is the licence of the dropped fonts (JetBrains Mono, Fira Code) and is kept for provenance.
