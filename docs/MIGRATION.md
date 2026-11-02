# Migrating to Synthesis

Two predecessors exist: the frozen Node CLI (`~/Vibe/nemo-cli`, command `nemo`) and early Rust builds that were still called Nemo. Synthesis reads neither one's files or variables automatically.

## From builds before the `synt` rename (2026-11-02)

The config folder was renamed with the command: `~/.synthesis` is `~/.synt` (`%USERPROFILE%\.synthesis` is `%USERPROFILE%\.synt`), project `.synthesis/` is `.synt/`, `synthesis.json` is `synt.json`, `synthesis.db*` is `synt.db*`, and the instruction file `SYNTHESIS.md` is `AGENTS.md` (home, project root and `.synt/`; its place in the lookup order is now taken by `AGENTS.md`, so an existing `AGENTS.md` and a `SYNTHESIS.md` must be merged by hand). Environment variables `SYNTHESIS_*` are `SYNT_*` (rename each by hand; `synt doctor` names any old one that is set, never its value, and does not honour it). The `"synthesis"` settings key and crate/npm names are unchanged. Nothing is read from or moved out of the old locations; `synt doctor` detects them and prints the command. By hand: `mv ~/.synthesis ~/.synt`, then `mv ~/.synt/synthesis.json ~/.synt/synt.json` (likewise `synthesis.db*`), `mv ~/.synt/SYNTHESIS.md ~/.synt/AGENTS.md`, and per project `mv .synthesis .synt` and `mv SYNTHESIS.md AGENTS.md`.

## From earlier Nemo builds (renamed 2026-10-30)

Everything was renamed: command `nemo` is `synt`, `NEMO_*` is `SYNT_*`, `~/.nemo` is `~/.synt`, project `.nemo/` is `.synt/`, `nemo.json` is `synt.json`, `NEMO.md` is `AGENTS.md` (precedence `AGENTS.md` > `AGENTS.md` > `CLAUDE.md`), `nemo.db` is `synt.db`, and the settings section `"nemo"` is `"synthesis"` (`synthesis.mcp.env_passthrough`, `synthesis.shell.*`, `synthesis.context.*`, `synthesis.pricing`, ...). Npm package `@devonchain101/synthesis`, archives `synthesis-<version>-<target>`.

Nothing is migrated for you. `synt doctor` prints a hint when `~/.nemo` exists and `~/.synt` does not, lists any `NEMO_*` variable that is set (names only), and warns when another `synt` earlier on PATH shadows the one you run. To migrate by hand (review first):

```
cp -R ~/.nemo ~/.synt
mv ~/.synt/nemo.json ~/.synt/synt.json      # NEMO.md -> AGENTS.md, nemo.db* -> synt.db* likewise
# in settings.json rename the top-level key "nemo" to "synt"
# per project: git mv .nemo .synt   (and NEMO.md -> AGENTS.md)
# shell profile: rename every NEMO_X variable to SYNT_X
```

## Coexistence with the Node CLI

The Node CLI is frozen and retired: the owner no longer uses it (2026-10-28), so a parallel run is NOT required for cutover. It stays read-only as a behaviour/fixture reference (ADR-010). If you still have it installed, both coexist without sharing anything:

- **Command names.** Node `nemo`/`nemocode`, Synthesis `synt`. The installers warn when another `synt` is already earlier on PATH; check with `which -a synt` (`where synt` on Windows). `scripts/package-npm.mjs --bin-names` and `SYNT_BIN_NAME` choose another command name.
- **Separate homes.** Node uses `~/.nemo` and `NEMO_HOME`, Synthesis `~/.synt` and `SYNT_HOME` (default `~/.synt`); no file is shared. Use a separate home for experiments: `SYNT_HOME=/tmp/synthesis-try synt ...`.
- **`compat.write_node_transcripts`** (default ON): the Rust session store also mirrors each session as a Node-format JSONL transcript under `projects/` of its own home. Node does not look there any more, so turn it off unless you copy the files over yourself.
- **Keys.** Synthesis uses `SYNT_*` variables only (ADR-011). It never reads `ANTHROPIC_*`, `OPENAI_*`, `CLAUDE_CONFIG_DIR` or `CODEX_HOME`. Import from `~/.claude` / `~/.codex` takes an explicit path.
- **Config and sessions.** Formats stay compatible in shape (no longer a cutover gate). Config layering is a golden-tested port (the snapshots use the renamed names, see tests/golden/README.md). Do not assume a session written by one side resumes cleanly on the other.
- **Behaviour differences to expect.** Rust requires `--cwd`; sandbox defaults to `enforce` with network `deny` (the Node CLI had no OS sandbox); no plaintext `auth.json`.

## Cutover gates (design doc section 10, Phase 3)

Mark each with evidence (run link or output) before cutover. Node is already retired, so no parallel run or Node baseline is needed:

- [ ] Rust eval passes on a real model (`--agent rust`; needs API keys). A Node baseline comparison is optional.
- [ ] Golden tests green on macOS, Linux and Windows CI (CI has never run on GitHub).
- [ ] Open audit items verified on real runs: ConPTY, legacy conhost, Git Bash quoting, clipboard/notification tools per OS.
- [ ] Release pipeline produced signed, verified artifacts for the 6 targets (macOS and Windows signing, notarization).
- [ ] Linux sandbox job green with Landlock and bubblewrap on a real runner.
- [ ] Browser job green on all three OSes.
- [ ] Independent security re-review of the fixed audit findings (docs/STATUS.md).

## Rollback

If you kept a Node install, to go back: remove the Rust install (docs/INSTALL.md), put the Node CLI first on PATH. Node's `~/.nemo` is untouched by Synthesis; `$SYNT_HOME/synt.db` and `sessions/` can be deleted safely.
