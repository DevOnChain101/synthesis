# Spec: Configuration, layering, env vars, credentials — reviewed 0.1

Tags as in events.md. "Faithful port" means golden-tested equality with Node (eval-and-golden.md §B.2).

> Naming (owner decision 2026-10-30): the product, its directories, files, settings key and variables were renamed from Nemo to Synthesis (`~/.nemo` to `~/.synthesis`, `.nemo/` to `.synthesis/`, `nemo.json` to `synthesis.json`, `NEMO.md` to `SYNTHESIS.md`, `NEMO_*` to `SYNTHESIS_*`, settings section `"nemo"` to `"synthesis"`). The `[from-node ...]` tags cite the Node reference, which still uses the old names. Synthesis never reads the old names; `synth doctor` detects them and prints the migration steps.

## 1. Directories [from-node]
- `SYNTHESIS_HOME` = `$SYNTHESIS_HOME` or `~/.synthesis` [auth.js:26-28, config.js:250]. Holds everything Synthesis owns. The real user home (`~`) is a separate value, used for `~` expansion and for Claude/Codex discovery; the two must never be conflated [agent.js:298-302].
- Project root = nearest ancestor of cwd that contains `.synthesis/` or `.git/`, else cwd [claudecfg.js:411-419].

## 2. Files

| File | Scope | Format | Notes |
|---|---|---|---|
| `$SYNTHESIS_HOME/synthesis.json` | user config | JSON, `$schema:"config.json/v1"` required | providers, model, aliases, profiles, MCP [config.js:229-231, configschema.js:13] |
| `<cwd>/.synthesis/config.json` | project config | same schema | read from **cwd**, not the project root [config.js:278] |
| `$SYNTHESIS_HOME/state.json` | last-used values | `{model?, effort?, autocompact?}` | written by /model, /effort, /autocompact [state.js:3-46] |
| `$SYNTHESIS_HOME/auth.json` | credentials | `{"<provider>":{"type":"api","key":str}\|{"type":"token","token":str}}`, mode 0600 | plaintext [auth.js:57-78] |
| `$SYNTHESIS_HOME/settings.json` | user settings | JSON; unknown keys ignored, but a file that exists and is malformed / not an object / unreadable is a hard error: `synth run` exits 2, so its deny rules are never silently dropped (owner decision D5) | `permissions{allow,deny,ask,additionalDirectories,defaultMode,blockReadsOutsideWorkingDirectories}`, `hooks`, `disableAllHooks`, `autoMode{environment,allow,soft_deny,hard_deny,classifyAllShell,useAutoModeDuringPlan,model}`, MCP approval lists [claudecfg.js:145-183, 250-258] |
| `<projectRoot>/.synthesis/settings.json` | project settings | tolerant JSON | `defaultMode` `auto`/`bypassPermissions` ignored here [claudecfg.js:143,166]; `autoMode` and `blockReadsOutsideWorkingDirectories` are user-only [claudecfg.js:250, 260-262]; `autoMode.model` [new] = model name (same provider as the session) for the auto-mode judge, default the session model; `bypassPermissions` is never taken from settings, only from the `synth run --permission-mode` flag or the TUI confirmation dialog |
| `<projectRoot>/.synthesis/settings.local.json` | local settings [new] | like project (same trust rules: untrusted = deny/ask/safe modes only); meant to be gitignored, `synth` writes `.synthesis/.gitignore` with it | where "allow always" persists `permissions.allow` rules (trusted projects only; a file that cannot be parsed is never overwritten) |
| managed file [new]: macOS `/Library/Application Support/Synthesis/managed-settings.json`, Linux `/etc/synthesis/managed-settings.json`, Windows `C:\ProgramData\Synthesis\managed-settings.json` | organisation settings | same strictness as user (malformed = hard error); no environment override | same keys as the user file. Precedence **managed > CLI flags > local > project > user**. `deny`/`ask` (and all other lists) are unions, so no lower layer, CLI flag or allow rule can remove a managed deny; a managed `permissions.defaultMode` (not bypass/auto) overrides `--permission-mode` for the initial mode (the person can still cycle modes in the TUI) |
| `~/.claude/settings.json`, `~/.claude.json`, `<projectRoot>/.mcp.json` | Claude Code sources | tolerant JSON | read only when `synthesis.json` has `claude.readUserConfig: true` [claudecfg.js:780-789, 803] |
| `SYNTHESIS.md` / `AGENTS.md` / `CLAUDE.md` / `.claude/CLAUDE.md` in each directory from the workspace root down to cwd (not `~`), plus the private `CLAUDE.local.md` | memory | Markdown with `@imports` | per directory only the FIRST non-empty of SYNTHESIS.md > AGENTS.md > CLAUDE.md > .claude/CLAUDE.md is loaded (owner decision D2); root to cwd order; 32 KiB combined cap (also per file); imports as before [claudecfg.js:374-399] |
| `$SYNTHESIS_HOME/{agents,skills,commands}/`, `<projectRoot>/.synthesis/{agents,skills,commands}/` | extensions | frontmatter Markdown | built-in skills ship with the binary [claudecfg.js:823-837] |
| `$SYNTHESIS_HOME/projects/<sanitized-cwd>/memory/*.md` | auto-memory (staged by `synth import claude`), loaded at the lowest priority: `MEMORY.md` first, 8 KiB per file, 16 KiB total, untrusted data; the key is the workspace root or the launch dir | Markdown | [claudecfg.js:813-821] |

## 3. Layering (lowest → highest) [from-node config.js:3-24, 247-310]
1. Built-in defaults: providers `anthropic` + `openai` (official base URLs), `maxTokens: 32000`, aliases `opus|sonnet|haiku|fable` with null targets, no default model [config.js:45-81].
2. `$SYNTHESIS_HOME/synthesis.json` (deep merge; `providers.<id>` and `aliases` merged per key [config.js:87-106]).
3. `state.json` `model` (the last /model choice; marks the model non-explicit, so it may fall back to the same family [config.js:265-276]).
4. `<cwd>/.synthesis/config.json`.
5. Environment variables (§4).
Settings files (permissions/hooks) are concatenated in the order user Claude (if enabled) → user Synthesis → project; allow/deny/ask lists are unions, scalar values: last one wins [claudecfg.js:157-183]. Provider entries also accept the OpenCode `provider` key, `npm`, `options.*`, and `{env:VAR}` / `{file:path}` substitution [config.js:26-29, 147-162].

## 4. Environment variables [from-node]
Only `SYNTHESIS_*` is read for Synthesis's own settings and keys (ADR-011); the Node reference's `NEMO_*` names map one to one (`NEMO_X` is now `SYNTHESIS_X`) but are NOT read.
| Var | Effect |
|---|---|
| `SYNTHESIS_HOME` | data directory |
| `SYNTHESIS_PROVIDER` | provider id targeted by the flat vars below (default `anthropic`, case-insensitive) [auth.js:89-100] |
| `SYNTHESIS_API_KEY` / `SYNTHESIS_AUTH_TOKEN` | credential for `SYNTHESIS_PROVIDER` only; the token wins over the key [auth.js:94-100] |
| `SYNTHESIS_BASE_URL` | base URL for `SYNTHESIS_PROVIDER`; creates that provider entry if missing [config.js:180-196] |
| `SYNTHESIS_CUSTOM_HEADERS` | `Name: value` lines, merged into the provider's headers [config.js:137-145] |
| `SYNTHESIS_MODEL` / `SYNTHESIS_SMALL_MODEL` | model overrides (explicit, never swapped) [config.js:293-299] |
| `SYNTHESIS_DEFAULT_<ALIAS>_MODEL` | alias target [config.js:198-209] |
| ~~`CLAUDE_CONFIG_DIR`, `CODEX_HOME`~~ | Node reads these for import/`readUserConfig` [claudecfg.js:795, importcodex.js:234]. **Rust does not read them** (rule: only `SYNTHESIS_*` env vars are read); import takes an explicit path flag or the default `~/.claude` / `~/.codex` locations (`synth import claude`, see docs/IMPORT.md) |
`ANTHROPIC_*` and `OPENAI_*` are never read [config.js:11, auth.js:4]. Children (Bash, hooks, `!`) get the env minus `^SYNTHESIS_(?:[A-Z0-9]+_)*(?:API_KEY|AUTH_TOKEN|TOKEN)$` [auth.js:42-55]; hooks also get `SYNTHESIS_PROJECT_DIR` and `CLAUDE_PROJECT_DIR` = cwd [hooks.js:122].

## 5. Credential resolution [from-node auth.js:121-134]
env (gated to `SYNTHESIS_PROVIDER`) > provider config `authToken`/`apiKey` (after `{env:}`/`{file:}` substitution) > credential store. A `token` credential is sent as `Authorization: Bearer`; an `api` key as `x-api-key` (Anthropic) or Bearer (OpenAI) [providers/anthropic.js:45-53, providers/openai.js:87-89]; `authHeader` can override [configschema.js:160].

## 6. Schema versioning [from-node configschema.js:1-56, docs/config.schema.v1.json]
- `synthesis.json` and `.synthesis/config.json` MUST declare `$schema: "config.json/v1"`; a missing or unknown `$schema` is a hard error; unknown keys are errors (closed schema); file limit 1 MiB; at most 12 diagnostics; diagnostics never echo file contents [config.js:118-121, configschema.js:16-31].
- Rust ports this validator faithfully (golden cases include every error message).
- Rule for the parallel run [new]: Rust adds NO keys to `config.json/v1`; Node would reject them. Rust-only settings (budgets, verification, sandbox, compat switches) live under a top-level `"synthesis"` object in `settings.json`, which Node's tolerant reader ignores [claudecfg.js:158-183]. `config.json/v2` is allowed only after cutover.

```json
// $SYNTHESIS_HOME/settings.json (Rust-only section, draft)
{"permissions":{"allow":["Bash(npm test:*)"]},
 "synthesis":{"budget":{"max_turns":50,"max_tool_calls":200,"max_verify_attempts":3},
         "compat":{"write_node_transcripts":true}}}
```

### 6.1 Rust-only `synthesis` keys read by `synthesis-cli` [new]
```json
{"env": {"RUST_LOG": "info"},
 "synthesis": {
   "verification": {"inferred_only": "cap_unverified", "max_attempts": 3,
                    "checks": ["make lint", {"id": "unit", "text": "unit tests", "command": "make test"}]},
   "pricing": {"anthropic/claude-sonnet-4": {"input_per_mtok": 3.0, "output_per_mtok": 15.0}}}}
```
- `synthesis.verification` (user file, or project file when trusted; keys shallow-merged): `inferred_only` = `cap_unverified` (default) | `allow_passed` (verification.md §3 rule 4); `max_attempts` = integer 1..100 (overrides `synthesis.budget.max_verify_attempts` for the gate and the session budget); `checks` = extra REQUIRED checks, each a command string or `{id?, text?, command}`, run in the sandbox as user-declared criteria (so they gate completion exactly like `--criteria-file`; they do not disable or replace inferred checks, and have no per-check timeout: synthesis-verification has no field for either yet). Unknown keys and invalid values are ignored with a diagnostic that names the key only. Headless `synth run` applies `checks`; the interactive app does not yet.
- `synthesis.context` (USER file only): `auto_compact` = `false` disables proactive compaction (default `true`; the reactive compaction after a provider context-overflow stays); `auto_compact_threshold` = number 0.5..0.95 (default 0.85, out-of-range values are clamped with a diagnostic naming the key): before each model request the harness estimates the context size (the previous response's provider-reported input tokens plus a chars/4 estimate, calibrated against provider usage, for what was added since) and compacts when it reaches threshold x the model's context window (200000 if unknown). It then continues the same turn. A compaction that frees less than 15% pauses auto-compact for 5 requests with a `notice` warning; a summarizer failure emits `context.compaction_failed` and retries after a growing pause. The PreCompact hook runs first; its `additionalContext` is added to the summary instructions. PreCompact fails open like every event except PreToolUse/UserPromptSubmit, so a hook cannot cancel a compaction. `synth doctor` prints the effective values.
- `synthesis.mcp.toolSearch` (USER file only): `"auto"` (default) defers MCP tool schemas behind the `ToolSearch` tool once they take 10% of the context window, `"on"` always defers, `"off"` sends every schema (tools.md §6, Tool search). A project file cannot change it. Any other value keeps `auto` with a diagnostic naming the key.
- `synthesis.images.retentionDays` (USER file only): days a stored image (a pasted screenshot) is kept in `synthesis.db`; default `30`, `0` keeps them forever, at most `3650`. At the start of the interactive app every image block of a session untouched for that long becomes the text `[image removed after N days]` (message ids and order unchanged; a database locked by another process is skipped until the next start). The Node-format transcript mirror (`synthesis.compat.write_node_transcripts`) is not rewritten. Any other value keeps `30` with a diagnostic naming the key.
- `synthesis.shell` (USER file only): `user_commands_model_turn` = `false` stops the model turn that normally follows a `!cmd` (default `true`; `!!cmd` never has one); `user_commands_unsandboxed` = `true` runs `!cmd` outside the OS sandbox (default `false` = the same sandbox, policy and `--network` mode as the Bash tool, so the network is denied unless `--network loopback|allow`). Wrong types keep the default with a diagnostic naming the key.
- `synthesis.pricing` (USER file only, never the project's): key = `provider/model` (or bare model), values USD per million tokens. Needed by `--max-cost-usd`: without a price for the model the run is rejected before it starts (exit 2). The config.json/v1 model schema is closed and has no cost field, so pricing lives here.
- `env` (top level, Node's own-settings block): applied to command environments (Bash, hooks, verification) ON TOP of the `Env::child_env()` allowlist, never over a variable the allowlist already supplies (Node: "a variable from the environment wins"). Names are refused when secret-shaped (`synthesis-settings::is_secret_env_key`, listed by name in a warning) or malformed (`=`/NUL). Project-file `env` counts only with `--trust-project`. MCP stdio servers do not receive it (set `env` on the server entry instead).
- MCP: sources are synthesis.json `mcpServers`/`mcp`/`projects.<path>.mcpServers` (user), and project `.mcp.json` / `.synthesis/config.json` `mcp`/`mcpServers`. A project-scope server starts only with `--trust-project` plus approval via `--approve-mcp NAME` or the USER settings `enabledMcpjsonServers` / `enableAllProjectMcpServers`; `disabledMcpjsonServers` always wins. Tools are named `mcp__<server>__<tool>` for permission rules (`mcp__server` covers the server); results are labelled untrusted data. Servers are not OS-sandboxed.
- Custom agents: `<synthesis_home>/agents/*.md`, and `<project>/.synthesis/agents/*.md` only with `--trust-project`. Skills: `discover_skills`; names and descriptions go in the system context, project skills flagged untrusted.

## 7. Ported vs changed
| Area | Status |
|---|---|
| Layering, env vars, `$schema` validator, provider normalisation, model/alias family fallback (`resolveModelDefaults`, `bestModelForFamily` [config.js:312-416]) | faithful port, golden-tested |
| Settings merge, project-forbidden modes, user-only keys | faithful port |
| `readUserConfig` Claude-source gating | faithful port |
| Credentials in plaintext `auth.json` | changed [new], §8 |
| `effort` read straight from state.json, outside layering [config.js:31-32] | kept as is |
| Project config read from cwd while settings use project root | kept for parity; listed in Open |

## 8. Credential policy (ADR-011) [new]
1. API keys only. No subscription/OAuth login flow, no reuse of Claude Code/Codex tokens. `token` in §5 means a Bearer-style API key for a compatible gateway.
2. **The user manages their own keys.** Sources, in the §5 order: environment variables (`SYNTHESIS_*`, see §4) > provider config `apiKey`/`authToken` with `{env:NAME}` / `{file:path}` substitution. Recommended practice: keep keys in environment variables or a secrets file outside the repo.
3. **No OS keychain, no credential store written by Rust.** `synth` (Rust) never creates or modifies `auth.json`, and has no `auth login` / `auth migrate`. During the parallel run it MAY read a legacy `$SYNTHESIS_HOME/auth.json` created by the Node CLI, read-only, as the lowest-priority source.
4. `synth auth status` (Rust) reports, per provider, which source supplied the key and a masked value (`abcd…wxyz`) [auth.js:80-84]; it never prints full keys.
5. Secrets never enter events, logs, transcripts or artifacts (events.md §5). Child processes (Bash, hooks, `!`) get the env minus the key-pattern in §4 [auth.js:42-55].

## Open
- Project `.synthesis/config.json` is read from cwd, but `.synthesis/settings.json` from the project root: keep the inconsistency or unify after cutover?
- ~~Read `ANTHROPIC_API_KEY` / `OPENAI_API_KEY`?~~ Resolved by the product owner: no. Synthesis's own settings and API keys come from `SYNTHESIS_*` variables only; it never reads another product's prefix (`ANTHROPIC_*`, `OPENAI_*`, `CLAUDE_*`, `CODEX_*`, ...). Clarified by the product owner: generic standardized variables (proxy, TLS CA, terminal, platform, locale) MUST be honoured per their own standards. The full list is in CLAUDE.md; all reads go through the injectable `Env`.

## 9. Security amendments (post-review, supersede the Node-parity text above)
- **Child env is an allowlist.** `synthesis_config::Env::child_env()` is the only way to build a child process environment (Bash, hooks, MCP stdio): PATH/HOME/USERPROFILE/TMPDIR/TEMP/SystemRoot/ComSpec/PATHEXT/TERM/COLORTERM/NO_COLOR/LANG/`LC_*`/`XDG_*`/proxy vars/`SSL_CERT_*`/`CI` and non-secret-shaped `SYNTHESIS_*` names. `{env:NAME}` and explicit per-server env maps are the only opt-ins. The §4 denylist (`without_secrets`) is deprecated.
- **Project config is untrusted** (`ProjectTrust::Untrusted` by default). A project `.synthesis/config.json` may set model/small model/aliases/maxTokens/profiles and provider display fields, but NOT `baseURL`, `apiKey`, `authToken`, `authHeader`, `headers`, `options`, `mcp`, `mcpServers`, `projects`, `claude`. `{file:}` substitution is never read from the project layer. Dropped fields yield value-free warnings.
- **Settings:** project hooks and project `allow` rules are ignored unless the caller passes `ProjectTrust::Trusted`; untrusted project settings may only add `deny`/`ask` rules.
- **Redaction:** `Debug` of `Provider`/`Env`/`Config` never prints key material; `mask` shows 4+4 characters only for keys of 16+ characters.
