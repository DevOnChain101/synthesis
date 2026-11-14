# design/

Brand kit and design references for Synthesis. Provenance: the owner's "Synthesis UI Kit" (`Synthesis_UI_Kit_Full`, v1.0, delivered as a zip on 2026-10-30). The files were copied unchanged and treated as data: nothing in them is executed or built. The kit's own readme is `KIT_README.md`, its index `manifest.json`.

| Folder | Content |
|---|---|
| `01-brand/logo` | symbol (transparent, mono light/dark/silver) as PNG up to 1024 px, horizontal lockups (PNG and SVG). The SVGs are monochrome approximations; the polished logo is the raster PNG. |
| `01-brand/app-icons` | app icon PNG 16 to 1024 px and `synthesis-app-icon.ico` (Windows). A macOS `.icns` is not in the kit: generate it from the 1024 px PNG when an app bundle or installer needs it. |
| `02-design-tokens` | `tokens.css`, `color-schema.json`/`.md`/preview PNG: background `#0B0C0F`, surface `#15171B`, elevated `#242830`, border `#3A3F4A`, text `#9CA3AF`, strong text `#E5E7EB`, accent `#D9D9E0`. |
| `03-designs` | `landing/landing.html` and `desktop/desktop.html` with their preview PNGs. |

## How to use it

- `03-designs/desktop` (HTML plus preview) is the reference for the future GUI (an app on the Rust core, not part of the CLI).
- `03-designs/landing` (HTML plus preview) is the reference for the future landing page.
- Take colours and radii from `tokens.css`; do not copy the mockup HTML into product code.
- Packaging: the Windows icon is `01-brand/app-icons/synthesis-app-icon.ico` (no installer uses it yet, see docs/RELEASE.md).
- TUI: `~/Vibe/synthesis-cli-ui/index.html` is the ONLY design source (copy in `design/cli-prototype/`; one view; no fonts shipped, the TUI uses the terminal's own font). `docs/specs/tui-design.md` §16 lists what the TUI takes from it.

## Not included

The kit's brand boards (`04-brand-boards`, about 6 MB of PNG), the extras (`05-extras`, including the nested `Synthesis_Design_Concept_original.zip`) were left out to keep this folder small (about 3 MB). They remain in the owner's original zip.

## App icons

The art is the owner's icon image, `01-brand/app-icons/source/owner-icon-master.jpg` (294x296 JPEG supplied by the owner, 2026-11-14; it is the master, do not repaint it). `swift scripts/make-icons.swift` (macOS) crops it inside its old rounded edge, lightly denoises, upscales to 1024 (Lanczos), sharpens (unsharp mask) and cuts it to the Apple continuous-corner squircle. It writes `01-brand/app-icons/*` (full bleed, for Linux and Windows) and `apps/desktop/assets/icons/*` (the `.icns` uses the Apple grid, an 824 px body in 1024, with no shadow). A higher-resolution master would give a sharper result: replace the JPEG and re-run the script.
