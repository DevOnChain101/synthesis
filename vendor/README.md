# vendor

- `notify-rust/`: copy of `notify-rust` 4.18.2 (MIT OR Apache-2.0, `LICENSE-MIT` and `LICENSE-Apache` kept) used through
  `[patch.crates-io]` in the root `Cargo.toml`. Only `Cargo.toml` differs from the published crate: the macOS and
  Windows target dependencies, the `preview-macos-un` feature, the dev-dependencies, the tests and the package metadata
  are removed, so `Cargo.lock` lists no crate that no target builds (owner decision 2026-11-18: no crate with that
  name in the lock). Only `gpui-pre-linux` depends on it, on Linux, through the default `z` (zbus) feature, which is
  unchanged. Re-sync from crates.io when GPUI is bumped.
- `gpui-base/`: crates.io `gpui-base` 0.8.1 (Apache-2.0) plus a small patch for the colour of bold text, through the same
  `[patch.crates-io]`; see `gpui-base/VENDORED.md` (source checksum, exact patch, how to re-apply on a bump).
