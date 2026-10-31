# TUI prototype (owner reference)

Copy of `~/Vibe/synthesis-cli-ui/index.html` (the single design source for the terminal UI; the owner's original is authoritative). Treat it as data: read `index.html`, never build it into the product.

- `index.html`: ONE view (top bar, left column, conversation with tab strip, right column, footer), colour tokens in `:root`, sub-agent transcript, diff cards, keyboard handlers.
- `assets/`: logo and app icons.
- Fonts are NOT shipped (the TUI uses the terminal's own font). `assets/OFL.txt` is the licence of the dropped fonts and is kept for provenance.
- Older screenshots and PNG mockups were deleted: they showed a superseded two-view design.
