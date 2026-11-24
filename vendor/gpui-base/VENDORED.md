# gpui-base 0.8.1, vendored with one patch

- Source: crates.io `gpui-base` 0.8.1 (Apache-2.0, `LICENSE-APACHE` kept), `.crate` sha256
  `d862bc8f2e27710039013d075ac4995892e9687ff207fa530f880cbcd513936f` (the checksum `Cargo.lock` had for the registry
  crate). Everything is the unpacked crate byte for byte except `Cargo.lock` (not copied) and the patch below.
- Why: gpui-base has no way to colour `**bold**` text (`mark_highlight` sets only the font weight). Owner decision
  2026-11-24: bold in assistant output shows in the light accent, like headings (the old Node CLI had
  `strong: ACCENT.light`).
- Patch (`// synthesis patch` / `(synthesis patch)` marks it), `src/text/style.rs` and `src/text/node.rs`:
  - `TextViewStyle` gets `strong: Option<Hsla>` (default `None`, part of `PartialEq`), `with_strong(color)` and `strong()`.
  - `mark_highlight`: `if mark.bold { ...; highlight.color = node_cx.style.strong(); }`. With `None` nothing changes;
    inline code, links and highlights still apply after it and win.
  - Header cells: `TextViewStyle::with_centered_table_head()` / `centers_table_head()` (default off); in
    `render_table_rows` the header row (row 0) uses `ColumnumnAlign::Center` instead of the column alignment. Body cells,
    selection and copy (`to_markdown`) keep the markdown alignment.
- Used by: `assistant_style` in `apps/desktop/src/tui_view/chat.rs` (`.with_strong(ACCENT_LIGHT)`), so the TUI chat,
  run blocks and Super Agent markdown get it.
- Wired in the root `Cargo.toml` by `[patch.crates-io] gpui-base = { path = "vendor/gpui-base" }`; `exclude =
  ["vendor"]` keeps it out of the workspace (fmt, clippy, tests skip it).
- On a gpui-component / gpui-base bump: unpack the new `gpui-base` into this directory, re-apply the patch above
  (about 25 lines, `git log -p -- vendor/gpui-base` shows it), update the version and sha256 here, or drop the patch
  and this directory if upstream gained a bold colour.
