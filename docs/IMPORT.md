# Importing a Claude Code setup (`synt import claude`)

`synt import claude` COPIES and CONVERTS your Claude Code configuration into Synthesis-owned files so
Synthesis runs in an equivalent environment. Synthesis never reads `~/.claude` at runtime (ADR-011, only
`SYNTHESIS_*` settings); after the import the Synthesis files are yours and the Claude ones are untouched.

```
synt import claude [--scope user|project|all] [--project-dir DIR] [--from DIR]
                   [--dry-run | --apply] [--only LIST]
                   [--merge | --overwrite | --skip-existing]
                   [--accept-broad-allow] [--accept-hooks]
```

- Default is a **dry run**: a per-item plan (`source -> target : create|update|unchanged|skip`,
  warnings, a summary and a checklist of manual steps). `--apply` writes.
- `--from` defaults to `$HOME/.claude`; `~/.claude.json` is read from its parent directory.
  `--project-dir` is required for the project scope (`all` without it imports the user scope only).
  Targets are `$SYNTHESIS_HOME` (default `~/.synthesis`) and `<project>/.synthesis`. All paths derive from the
  injected environment, so `HOME=/tmp/x SYNTHESIS_HOME=/tmp/x/.synthesis synt import claude --from /tmp/x/.claude`
  never touches the real home.
- `--only` takes `instructions,permissions,hooks,env,agents,skills,commands,mcp,memory`
  (default: all but `memory`).

## What goes where

| Claude source | Synthesis target | Notes |
|---|---|---|
| `~/.claude/CLAUDE.md`, `~/.claude/rules/*.md` | `$SYNTHESIS_HOME/SYNTHESIS.md`, `rules/` | header `<!-- imported from ... on DATE by synt import claude -->`, content verbatim; relative `@imports` are copied to `imports/` and rewritten; path-scoped rules (`paths:`) load for every task (warned) |
| project `CLAUDE.md`, `CLAUDE.local.md`, `.claude/CLAUDE.md` | nothing | Synthesis already loads them from the project. Only when `.claude/rules/*.md` exist: `.synthesis/rules/*` plus a project `SYNTHESIS.md` (base text + `@.synthesis/rules/..` lines). A project `SYNTHESIS.md`/`AGENTS.md` wins over `CLAUDE.md` (reported) |
| `settings.json` (`.claude/settings.local.json` for the project) | `settings.json` (`.synthesis/settings.local.json`, listed in `.synthesis/.gitignore`) | `permissions.allow/ask/deny/additionalDirectories/defaultMode`, `hooks`, `env` |
| `agents/*.md`, `skills/*/` (whole directory), `commands/**/*.md` | `agents/`, `skills/`, `commands/` | frontmatter kept; tool names translated; unsupported fields listed. Synthesis has no slash-command loader yet: commands are staged only |
| `~/.claude.json` `mcpServers` | `$SYNTHESIS_HOME/synthesis.json` `mcpServers` (file gets `$schema` if new) | see MCP below |
| `~/.claude.json` `projects.<dir>.mcpServers` | `synthesis.json` `projects.<dir>.mcpServers` | |
| project `.mcp.json` | untouched | Synthesis reads it itself; the plan prints `--approve-mcp NAME@HASH` |
| `~/.claude/projects/<dir>/memory/*.md` (`--only memory`) | `$SYNTHESIS_HOME/projects/<dir>-<hash of the real path>/memory/` (the hash keeps `/a/b` and `/a-b` apart) | loaded at start as low-priority, size-capped (8 KiB per file, 16 KiB total) untrusted notes, `MEMORY.md` first |

Tool names: `Agent->Task`, `MultiEdit->Edit`, `LS->Glob`, `KillBash->KillShell`; `Bash`, `Read`,
`Edit`, `Write`, `Glob`, `Grep`, `WebFetch`, `WebSearch`, `Task`, `Skill`, `mcp__*` and
`Bash(spec)` forms are kept. Tools Synthesis lacks (`PowerShell`, `Monitor`, ...) are dropped with a
warning; an agent whose tools all vanish gets an empty `tools:` (no tools), never "all tools".

## Deliberately NOT imported (security rationale)

- **Credentials**: `apiKeyHelper`, OAuth accounts, tokens. Synthesis never writes credentials.
- **`defaultMode: bypassPermissions` or `auto`**: mapped to `default` with a warning.
- **Broad allow rules** (`Bash`, `Bash(*)`, `Write`, `Edit`, `WebFetch`, `*`): skipped unless `--accept-broad-allow`.
- **Rules Synthesis cannot represent** (unknown tool, malformed): dropped with a warning. A dropped `deny` rule is flagged loudly: that restriction is NOT carried over.
- **User-scope hooks**: printed (credential-looking words masked) and written only with `--accept-hooks`. Unsupported events (e.g. `SubagentStart`) are skipped. Project hooks are written but stay inert until the project is trusted (`--trust-project`). Treat imported hooks as unreviewed; `synt doctor` shows the count.
- **`env`**: secret-shaped names (`*KEY*`, `*TOKEN*`, `*SECRET*`, `*PASSWORD*`, ...) and `ANTHROPIC_/OPENAI_/CLAUDE_/CODEX_` prefixes are skipped; only names are listed.
- `model` (noted only; use `SYNTHESIS_MODEL`), `statusLine`, `enabledPlugins`, theme and other keys (listed by name).

## MCP servers

Entries are fitted to Synthesis's closed `config.json/v1` schema (unknown keys dropped by name,
`disabled` becomes `enabled`, unknown `type` with a `url` becomes `http`). **Literal secrets are
never copied**: a header/env/flag whose name is secret-shaped, a known token prefix (`ghp_`, `sk-`,
`xox*-`, `AKIA`, JWT...), a long random string, URL userinfo or a secret-shaped query parameter is
replaced by `${SERVER_KEY}` (`Authorization: Bearer x` becomes `Bearer ${SERVER_TOKEN}`).
Existing `${VAR}` references are kept. The checklist lists every variable you must export and add
to `synthesis.mcp.env_passthrough` in the user `settings.json` (the import does not widen that list
for you). OAuth servers are imported as-is with a warning; the login is left to the provider package.
The plan shows each definition hash. Project servers need hash approval as always; servers in
your own `synthesis.json` start without it (it is your file, and you just reviewed the plan).

## Safety properties

- All sources and targets are parsed before the first write: a malformed Claude or Synthesis file is a
  clear error (exit 2, no content echoed) and nothing is written.
- Each changed file is backed up first as `NAME.bak-YYYYmmdd-HHMMSS`, then replaced atomically
  (temp file + rename), mode 0600 on Unix.
- Idempotent: a second run reports `unchanged` and writes nothing (no new backup).
- `--merge` (default): lists are unioned, scalars and existing servers keep your value, files
  Synthesis did not write itself are kept (reported). `--overwrite` replaces what the import manages.
  `--skip-existing` never touches an existing file.
- Copied files are tracked in `<root>/imports/claude.json`; `synt doctor` prints its counts and which instruction files apply.

## Example

```
$ synt import claude --project-dir ~/code/app            # plan only
$ synt import claude --project-dir ~/code/app --apply    # write
$ synt import claude --apply --accept-hooks --only hooks # after reviewing the hooks
```

## Undo

Restore a file from its backup (`mv ~/.synthesis/settings.json.bak-20261029-101500 ~/.synthesis/settings.json`)
and delete files that were `create`d (listed in `imports/claude.json`). Nothing under `~/.claude`
is ever modified.
