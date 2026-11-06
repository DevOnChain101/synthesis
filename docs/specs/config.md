# Spec: Configuration, layering, env vars, credentials — reviewed 0.1

Tags as in events.md. "Faithful port" means golden-tested equality with Node (eval-and-golden.md §B.2).

> Naming (owner decision 2026-10-30): the product, its directories, files, settings key and variables were renamed from Nemo to Synthesis (`~/.nemo` to `~/.synt`, `.nemo/` to `.synt/`, `nemo.json` to `synt.json`, `NEMO.md` to `AGENTS.md`, `NEMO_*` to `SYNT_*`, settings section `"nemo"` to `"synt"`). The `[from-node ...]` tags cite the Node reference, which still uses the old names. Synthesis never reads the old names; `synt doctor` detects them and prints the migration steps.

## 1. Directories [from-node]
- `SYNT_HOME` = `$SYNT_HOME` or `~/.synt` [auth.js:26-28, config.js:250]. Holds everything Synthesis owns. The real user home (`~`) is a separate value, used for `~` expansion and for Claude/Codex discovery; the two must never be conflated [agent.js:298-302].
- Project root = nearest ancestor of cwd that contains `.synt/` or `.git/`, else cwd [claudecfg.js:411-419].

## 2. Files

| File | Scope | Format | Notes |
|---|---|---|---|
| `$SYNT_HOME/synt.json` | user config | JSON, `$schema:"config.json/v1"` required | providers, model, aliases, profiles, MCP [config.js:229-231, configschema.js:13] |
| `<cwd>/.synt/config.json` | project config | same schema | read from **cwd**, not the project root [config.js:278] |
| `$SYNT_HOME/state.json` | last-used values | `{model?, effort?, autocompact?}` | written by /model, /effort, /autocompact [state.js:3-46] |
| `$SYNT_HOME/auth.json` | credentials | `{"<provider>":{"type":"api","key":str}\|{"type":"token","token":str}}`, mode 0600 | plaintext [auth.js:57-78] |
| `$SYNT_HOME/settings.json` | user settings | JSON; unknown keys ignored, but a file that exists and is malformed / not an object / unreadable is a hard error: `synt run` exits 2, so its deny rules are never silently dropped (owner decision D5) | `permissions{allow,deny,ask,additionalDirectories,defaultMode,blockReadsOutsideWorkingDirectories}`, `hooks`, `disableAllHooks`, `autoMode{environment,allow,soft_deny,hard_deny,classifyAllShell,useAutoModeDuringPlan,model}`, MCP approval lists [claudecfg.js:145-183, 250-258] |
| `<projectRoot>/.synt/settings.json` | project settings | tolerant JSON | `defaultMode` `auto`/`bypassPermissions` ignored here [claudecfg.js:143,166]; `autoMode` and `blockReadsOutsideWorkingDirectories` are user-only [claudecfg.js:250, 260-262]; `autoMode.model` [new] = model name (same provider as the session) for the auto-mode judge, default the session model; `bypassPermissions` is never taken from settings, only from the `synt run --permission-mode` flag or the TUI confirmation dialog |
| `<projectRoot>/.synt/settings.local.json` | local settings [new] | like project (same trust rules: untrusted = deny/ask/safe modes only); meant to be gitignored, `synt` writes `.synt/.gitignore` with it | where "allow always" persists `permissions.allow` rules (trusted projects only; a file that cannot be parsed is never overwritten) |
| managed file [new]: macOS `/Library/Application Support/Synthesis/managed-settings.json`, Linux `/etc/synthesis/managed-settings.json`, Windows `C:\ProgramData\Synthesis\managed-settings.json` | organisation settings | same strictness as user (malformed = hard error); no environment override | same keys as the user file. Precedence **managed > CLI flags > local > project > user**. `deny`/`ask` (and all other lists) are unions, so no lower layer, CLI flag or allow rule can remove a managed deny; a managed `permissions.defaultMode` (not bypass/auto) overrides `--permission-mode` for the initial mode (the person can still cycle modes in the TUI) |
| `~/.claude/settings.json`, `~/.claude.json`, `<projectRoot>/.mcp.json` | Claude Code sources | tolerant JSON | read only when `synt.json` has `claude.readUserConfig: true` [claudecfg.js:780-789, 803] |
| `AGENTS.md` / `CLAUDE.md` / `.claude/CLAUDE.md` in each directory from the workspace root down to cwd (not `~`), plus the private `CLAUDE.local.md` | memory | Markdown with `@imports` | per directory only the FIRST non-empty of AGENTS.md > CLAUDE.md > .claude/CLAUDE.md is loaded (owner decision D2); root to cwd order; 32 KiB combined cap (also per file); imports as before [claudecfg.js:374-399] |
| `$SYNT_HOME/{agents,skills,commands}/`, `<projectRoot>/.synt/{agents,skills,commands}/` | extensions | frontmatter Markdown | built-in skills ship with the binary [claudecfg.js:823-837] |
| `$SYNT_HOME/projects/<sanitized-cwd>/memory/*.md` | auto-memory (staged by `synt import claude`), loaded at the lowest priority: `MEMORY.md` first, 8 KiB per file, 16 KiB total, untrusted data; the key is the workspace root or the launch dir | Markdown | [claudecfg.js:813-821] |

## 3. Layering (lowest → highest) [from-node config.js:3-24, 247-310]
1. Built-in defaults: providers `anthropic` + `openai` (official base URLs), `maxTokens: 32000`, aliases `opus|sonnet|haiku|fable` with null targets, no default model [config.js:45-81].
2. `$SYNT_HOME/synt.json` (deep merge; `providers.<id>` and `aliases` merged per key [config.js:87-106]).
3. `state.json` `model` (the last /model choice; marks the model non-explicit, so it may fall back to the same family [config.js:265-276]).
4. `<cwd>/.synt/config.json`.
5. Environment variables (§4).
Settings files (permissions/hooks) are concatenated in the order user Claude (if enabled) → user Synthesis → project; allow/deny/ask lists are unions, scalar values: last one wins [claudecfg.js:157-183]. Provider entries also accept the OpenCode `provider` key, `npm`, `options.*`, and `{env:VAR}` / `{file:path}` substitution [config.js:26-29, 147-162].

## 4. Environment variables [from-node]
Only `SYNT_*` is read for Synthesis's own settings and keys (ADR-011); the Node reference's `NEMO_*` names map one to one (`NEMO_X` is now `SYNT_X`) but are NOT read.
| Var | Effect |
|---|---|
| `SYNT_HOME` | data directory |
| `SYNT_PROVIDER` | provider id targeted by the flat vars below (default `anthropic`, case-insensitive) [auth.js:89-100] |
| `SYNT_API_KEY` / `SYNT_AUTH_TOKEN` | credential for `SYNT_PROVIDER` only; the token wins over the key [auth.js:94-100] |
| `SYNT_BASE_URL` | base URL for `SYNT_PROVIDER`; creates that provider entry if missing [config.js:180-196] |
| `SYNT_CUSTOM_HEADERS` | `Name: value` lines, merged into the provider's headers [config.js:137-145] |
| `SYNT_MODEL` / `SYNT_SMALL_MODEL` | model overrides (explicit, never swapped) [config.js:293-299] |
| `SYNT_DEFAULT_<ALIAS>_MODEL` | alias target [config.js:198-209] |
| ~~`CLAUDE_CONFIG_DIR`, `CODEX_HOME`~~ | Node reads these for import/`readUserConfig` [claudecfg.js:795, importcodex.js:234]. **Rust does not read them** (rule: only `SYNT_*` env vars are read); import takes an explicit path flag or the default `~/.claude` / `~/.codex` locations (`synt import claude`, see docs/IMPORT.md) |
`ANTHROPIC_*` and `OPENAI_*` are never read [config.js:11, auth.js:4]. Children (Bash, hooks, `!`) get the env minus `^SYNT_(?:[A-Z0-9]+_)*(?:API_KEY|AUTH_TOKEN|TOKEN)$` [auth.js:42-55]; hooks also get `SYNT_PROJECT_DIR` and `CLAUDE_PROJECT_DIR` = cwd [hooks.js:122].

## 5. Credential resolution [from-node auth.js:121-134]
env (gated to `SYNT_PROVIDER`) > provider config `authToken`/`apiKey` (after `{env:}`/`{file:}` substitution) > credential store. A `token` credential is sent as `Authorization: Bearer`; an `api` key as `x-api-key` (Anthropic) or Bearer (OpenAI) [providers/anthropic.js:45-53, providers/openai.js:87-89]; `authHeader` can override [configschema.js:160].

## 6. Schema versioning [from-node configschema.js:1-56, docs/config.schema.v1.json]
- `synt.json` and `.synt/config.json` MUST declare `$schema: "config.json/v1"`; a missing or unknown `$schema` is a hard error; unknown keys are errors (closed schema); file limit 1 MiB; at most 12 diagnostics; diagnostics never echo file contents [config.js:118-121, configschema.js:16-31].
- Rust ports this validator faithfully (golden cases include every error message).
- Rule for the parallel run [new]: Rust adds NO keys to `config.json/v1`; Node would reject them. Rust-only settings (budgets, verification, sandbox, compat switches) live under a top-level `"synt"` object in `settings.json`, which Node's tolerant reader ignores [claudecfg.js:158-183]. `config.json/v2` is allowed only after cutover.

```json
// $SYNT_HOME/settings.json (Rust-only section, draft)
{"permissions":{"allow":["Bash(npm test:*)"]},
 "synt":{"budget":{"max_turns":50,"max_tool_calls":200,"max_verify_attempts":3},
         "compat":{"write_node_transcripts":true}}}
```

### 6.1 Rust-only `synt` keys read by `synthesis-cli` [new]
```json
{"env": {"RUST_LOG": "info"},
 "synt": {
   "verification": {"inferred_only": "cap_unverified", "max_attempts": 3,
                    "checks": ["make lint", {"id": "unit", "text": "unit tests", "command": "make test"}]},
   "pricing": {"anthropic/claude-sonnet-4": {"input_per_mtok": 3.0, "output_per_mtok": 15.0}}}}
```
- `synt.verification` (user file, or project file when trusted; keys shallow-merged): `inferred_only` = `cap_unverified` (default) | `allow_passed` (verification.md §3 rule 4); `max_attempts` = integer 1..100 (overrides `synt.budget.max_verify_attempts` for the gate and the session budget); `checks` = extra REQUIRED checks, each a command string or `{id?, text?, command}`, run in the sandbox as user-declared criteria (so they gate completion exactly like `--criteria-file`; they do not disable or replace inferred checks, and have no per-check timeout: synthesis-verification has no field for either yet). Unknown keys and invalid values are ignored with a diagnostic that names the key only. Headless `synt run` applies `checks`; the interactive app does not yet.
- `synt.context` (USER file only): `auto_compact` = `false` disables proactive compaction (default `true`; the reactive compaction after a provider context-overflow stays, up to 3 per task; HTTP 413 and request-size errors count as overflow; the verbatim tail keeps the newest message and drops images over 4 MiB, and requests carry images only for the 2 newest image messages, 8 MiB); `auto_compact_threshold` = number 0.5..0.95 (default 0.85, out-of-range values are clamped with a diagnostic naming the key): before each model request the harness estimates the context size (the previous response's provider-reported input tokens plus a chars/4 estimate, images by size (1 token per 600 bytes, 1500..8000 each), calibrated against provider usage, for what was added since) and compacts when it reaches threshold x the model's context window (200000 if unknown, see §6.2). It then continues the same turn. A compaction that frees less than 15% pauses auto-compact for 5 requests with a `notice` warning; a summarizer failure emits `context.compaction_failed` and retries after a growing pause. The PreCompact hook runs first; its `additionalContext` is added to the summary instructions. PreCompact fails open like every event except PreToolUse/UserPromptSubmit, so a hook cannot cancel a compaction. `synt doctor` prints the effective values.
- `synt.mcp.toolSearch` (USER file only): `"auto"` (default) defers MCP tool schemas behind the `ToolSearch` tool once they take 10% of the context window, `"on"` always defers, `"off"` sends every schema (tools.md §6, Tool search). A project file cannot change it. Any other value keeps `auto` with a diagnostic naming the key.
- `synt.images.retentionDays` (USER file only): days a stored image (a pasted screenshot, or a screenshot a tool returned) is kept in the session store; default `30`, `0` keeps them forever, at most `3650`. "Removed" means rewritten in `synt.db`; this is exactly what is guaranteed: at the start of the interactive app, in every session untouched for that long, each image block of a message, each image inside a `tool_result`'s content, and each image anywhere in a message's `meta` becomes the text `[image removed after N days]` (message ids and order unchanged; `tool_use` inputs untouched). Freed pages are zeroed (`secure_delete`) and the write-ahead log is truncated with `wal_checkpoint(TRUNCATE)`, retried for half a second; if another process keeps the log open the app says so, and old copies stay in the log until the next checkpoint. A database locked by another process is skipped until the next start. No `VACUUM` (exclusive lock and a full copy). Not image-bearing, so not touched: `events` and their `payload_ref` files (events carry text only; tool events hold the text of a result, never its image), `tool_calls` (its refs are unused by the app). NOT rewritten: the Node-format transcript mirror (`synt.compat.write_node_transcripts`), backups and copies of the database, and the PNG files the browser tool writes under the session's artifact directory. Any other value keeps `30` with a diagnostic naming the key.
- `synt.tui` (USER file only; project, local and managed files are ignored with a diagnostic): the interactive app. `panels` = `{session, mcp, tools, activity, status, tokens, topology, usage}` booleans (default all `true`), `logo` = `"art"` | `"off"`, and booleans `show_thinking`, `queue_indicators`, `motion`, `mouse_capture`, `copy_on_select` (default `true`) `diffs_expanded` and `verbose` (default `false`; `verbose` also draws the internal chat rows, see docs/TUI.md). A wrong type or an unknown key keeps the default with a diagnostic naming the key. Written by `/config` (atomic edit of this file; every other key is kept). Defined once in `synthesis-settings::TuiSettings`.
- `synt.browser` (USER settings file only; project, local, managed and `--settings` files are ignored with a diagnostic): `secrets` (name -> `{source: "{env:SYNT_..._PASSWORD}" | "{file:/abs/path}", origins: [exact origins], field_kind?: password|username|totp}`), `profiles` (name -> `{allow_domains: [...]}`), `login_timeout_sec` (10..3600, default 600), `allow_unsandboxed` (bool, default false), `approved` (`{profiles: [names], secrets: [{secret, origin}]}` pre-approvals for unattended runs). Semantics, threat model and limits: docs/BROWSER-LOGIN.md.
- `synt.shell` (USER file only): `user_commands_model_turn` = `false` stops the model turn that normally follows a `!cmd` (default `true`; `!!cmd` never has one); `user_commands_unsandboxed` = `true` runs `!cmd` outside the OS sandbox (default `false` = the same sandbox, policy and `--network` mode as the Bash tool, so the network is denied unless `--network loopback|allow`). Wrong types keep the default with a diagnostic naming the key.
- `synt.pricing` (USER file only, never the project's): key = `provider/model` (or bare model), values USD per million tokens. Needed by `--max-cost-usd`: without a price for the model the run is rejected before it starts (exit 2). The config.json/v1 model schema is closed and has no cost field, so pricing lives here.
- `env` (top level, Node's own-settings block): applied to command environments (Bash, hooks, verification) ON TOP of the `Env::child_env()` allowlist, never over a variable the allowlist already supplies (Node: "a variable from the environment wins"). Names are refused when secret-shaped (`synthesis-settings::is_secret_env_key`, listed by name in a warning) or malformed (`=`/NUL). Project-file `env` counts only with `--trust-project`. MCP stdio servers do not receive it (set `env` on the server entry instead).
- MCP: sources are synt.json `mcpServers`/`mcp`/`projects.<path>.mcpServers` (user), and project `.mcp.json` / `.synt/config.json` `mcp`/`mcpServers`. A project-scope server starts only with `--trust-project` plus approval via `--approve-mcp NAME` or the USER settings `enabledMcpjsonServers` / `enableAllProjectMcpServers`; `disabledMcpjsonServers` always wins. Tools are named `mcp__<server>__<tool>` for permission rules (`mcp__server` covers the server); results are labelled untrusted data. Servers are not OS-sandboxed.
- Custom agents: `<synthesis_home>/agents/*.md`, and `<project>/.synt/agents/*.md` only with `--trust-project`. Skills: `discover_skills`; names and descriptions go in the system context, project skills flagged untrusted.

### 6.2 Model info: context window, max output, vision [new]
Per field, the first of these that has a value wins:
1. `synt.json`: `limit.context` / `limit.output` and `capabilities` of the model.
2. The model-info cache, which is also what a fetch in this run fills (in place, so later tasks see it).
3. A tiny built-in table of documented limits, only for `api.openai.com` (its `/v1/models` reports no context length): `gpt-6*`, `gpt-4.1*`, `gpt-4o*` (`synthesis-providers/src/modelinfo.rs`, `OPENAI_KNOWN`; add a row only from a documented number).
4. Unknown. Compaction then assumes 200k, one warning notice per session says so (`context window unknown for <model>: assuming 200k; set limit.context`), `context.usage` carries `window_tokens: 0`, and the TUI shows `ctx 12.3k/?` instead of a percentage.

Fetching: at start, one background task per provider asks `GET /v1/models` (Anthropic: `max_input_tokens`, `max_tokens`, `capabilities.image_input`, `capabilities.effort.<tier>`; OpenAI-compatible gateways: `context_length`, `context_window`, `max_context_length`, `top_provider.context_length`, `max_output_tokens`, `top_provider.max_completion_tokens`). Contacted at start: the provider of this run, plus every other provider of the user's own config that has a credential and either declared `models` or `discover: true`; each is asked with its own key only. `discover: false` is never contacted, and a project's config adds no provider to this list. `synt.modelInfo.fetch: false` (user settings only; a project or managed file cannot set it) turns the startup fetch off entirely; the cache is still read, and `synt models --refresh` still asks explicitly. It never blocks the UI or the first prompt (the first task uses the cache or the fallback), is limited to 5 s, and any failure (offline, timeout, 401) is silent: a stale cache stays in use. `effort_tiers` are cached but `/effort` still follows `capabilities.effort_tiers` in `synt.json`.

Safety bounds on what a provider returns: a model id with a control, bidi, zero-width or other invisible character (or whitespace, or over 256 characters) is dropped, never stored or printed; a context window outside 1024..=100,000,000 is unknown; a max output below 16 is unknown and one above the window is clamped to it; a list page is read up to 8 MiB and 30 s (error beyond); `--refresh` allows 30 s per provider; a cache file over 4 MiB is ignored and the same checks apply when the cache is read. Everything printed goes through the display sanitiser.

Cache: `$SYNT_HOME/cache/models/<id>-<hash>.json` (`<id>` with unsafe characters replaced by `_`, a Windows device name prefixed `p-`, `<hash>` the first 8 hex digits of sha256 of the raw provider id), mode 0600, written atomically, no secrets: `{schema_version, base_url, protocol, fetched_at, models: {id: {context_window, max_output, vision, effort_tiers?, raw_capabilities?}}}`. Younger than 24 h = fresh (no network at start). A changed `baseURL` or protocol, an unknown `schema_version` or a corrupt file is ignored. `/model` restarts the session on the new `(provider, model)`, which reads its own entry. `synt models --refresh` ignores freshness and rewrites the cache.

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
2. **The user manages their own keys.** Sources, in the §5 order: environment variables (`SYNT_*`, see §4) > provider config `apiKey`/`authToken` with `{env:NAME}` / `{file:path}` substitution. Recommended practice: keep keys in environment variables or a secrets file outside the repo.
3. **No OS keychain, no credential store written by Rust.** `synt` (Rust) never creates or modifies `auth.json`, and has no `auth login` / `auth migrate`. During the parallel run it MAY read a legacy `$SYNT_HOME/auth.json` created by the Node CLI, read-only, as the lowest-priority source.
4. `synt auth status` (Rust) reports, per provider, which source supplied the key and a masked value (`abcd…wxyz`) [auth.js:80-84]; it never prints full keys.
5. Secrets never enter events, logs, transcripts or artifacts (events.md §5). Child processes (Bash, hooks, `!`) get the env minus the key-pattern in §4 [auth.js:42-55].

## Open
- Project `.synt/config.json` is read from cwd, but `.synt/settings.json` from the project root: keep the inconsistency or unify after cutover?
- ~~Read `ANTHROPIC_API_KEY` / `OPENAI_API_KEY`?~~ Resolved by the product owner: no. Synthesis's own settings and API keys come from `SYNT_*` variables only; it never reads another product's prefix (`ANTHROPIC_*`, `OPENAI_*`, `CLAUDE_*`, `CODEX_*`, ...). Clarified by the product owner: generic standardized variables (proxy, TLS CA, terminal, platform, locale) MUST be honoured per their own standards. The full list is in CLAUDE.md; all reads go through the injectable `Env`.

## 9. Security amendments (post-review, supersede the Node-parity text above)
- **Child env is an allowlist.** `synthesis_config::Env::child_env()` is the only way to build a child process environment (Bash, hooks, MCP stdio): PATH/HOME/USERPROFILE/TMPDIR/TEMP/SystemRoot/ComSpec/PATHEXT/TERM/COLORTERM/NO_COLOR/LANG/`LC_*`/`XDG_*`/proxy vars/`SSL_CERT_*`/`CI` and non-secret-shaped `SYNT_*` names. `{env:NAME}` and explicit per-server env maps are the only opt-ins. The §4 denylist (`without_secrets`) is deprecated.
- **Project config is untrusted** (`ProjectTrust::Untrusted` by default). A project `.synt/config.json` may set model/small model/aliases/maxTokens/profiles and provider display fields, but NOT `baseURL`, `apiKey`, `authToken`, `authHeader`, `headers`, `options`, `mcp`, `mcpServers`, `projects`, `claude`. `{file:}` substitution is never read from the project layer. Dropped fields yield value-free warnings.
- **Settings:** project hooks and project `allow` rules are ignored unless the caller passes `ProjectTrust::Trusted`; untrusted project settings may only add `deny`/`ask` rules.
- **Redaction:** `Debug` of `Provider`/`Env`/`Config` never prints key material; `mask` shows 4+4 characters only for keys of 16+ characters.
