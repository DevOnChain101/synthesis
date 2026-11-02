# Migrating to Synthesis

Two predecessors exist: the frozen Node CLI (`~/Vibe/nemo-cli`, command `nemo`) and early Rust builds that were still called Nemo. Synthesis reads neither one's files or variables automatically.

## From earlier Nemo builds (renamed 2026-10-30)

Everything was renamed: command `nemo` is `synt`, `NEMO_*` is `SYNTHESIS_*`, `~/.nemo` is `~/.synthesis`, project `.nemo/` is `.synthesis/`, `nemo.json` is `synthesis.json`, `NEMO.md` is `SYNTHESIS.md` (precedence `SYNTHESIS.md` > `AGENTS.md` > `CLAUDE.md`), `nemo.db` is `synthesis.db`, and the settings section `"nemo"` is `"synthesis"` (`synthesis.mcp.env_passthrough`, `synthesis.shell.*`, `synthesis.context.*`, `synthesis.pricing`, ...). Npm package `@devonchain101/synthesis`, archives `synthesis-<version>-<target>`.

Nothing is migrated for you. `synt doctor` prints a hint when `~/.nemo` exists and `~/.synthesis` does not, lists any `NEMO_*` variable that is set (names only), and warns when another `synt` earlier on PATH shadows the one you run. To migrate by hand (review first):

```
cp -R ~/.nemo ~/.synthesis
mv ~/.synthesis/nemo.json ~/.synthesis/synthesis.json      # NEMO.md -> SYNTHESIS.md, nemo.db* -> synthesis.db* likewise
# in settings.json rename the top-level key "nemo" to "synthesis"
# per project: git mv .nemo .synthesis   (and NEMO.md -> SYNTHESIS.md)
# shell profile: rename every NEMO_X variable to SYNTHESIS_X
```

## Coexistence with the Node CLI

The Node CLI is frozen and retired: the owner no longer uses it (2026-10-28), so a parallel run is NOT required for cutover. It stays read-only as a behaviour/fixture reference (ADR-010). If you still have it installed, both coexist without sharing anything:

- **Command names.** Node `nemo`/`nemocode`, Synthesis `synt`. The installers warn when another `synt` is already earlier on PATH; check with `which -a synt` (`where synt` on Windows). `scripts/package-npm.mjs --bin-names` and `SYNTHESIS_BIN_NAME` choose another command name.
- **Separate homes.** Node uses `~/.nemo` and `NEMO_HOME`, Synthesis `~/.synthesis` and `SYNTHESIS_HOME` (default `~/.synthesis`); no file is shared. Use a separate home for experiments: `SYNTHESIS_HOME=/tmp/synthesis-try synt ...`.
- **`compat.write_node_transcripts`** (default ON): the Rust session store also mirrors each session as a Node-format JSONL transcript under `projects/` of its own home. Node does not look there any more, so turn it off unless you copy the files over yourself.
- **Keys.** Synthesis uses `SYNTHESIS_*` variables only (ADR-011). It never reads `ANTHROPIC_*`, `OPENAI_*`, `CLAUDE_CONFIG_DIR` or `CODEX_HOME`. Import from `~/.claude` / `~/.codex` takes an explicit path.
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

If you kept a Node install, to go back: remove the Rust install (docs/INSTALL.md), put the Node CLI first on PATH. Node's `~/.nemo` is untouched by Synthesis; `$SYNTHESIS_HOME/synthesis.db` and `sessions/` can be deleted safely.
