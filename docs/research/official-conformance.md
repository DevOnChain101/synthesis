# Official conformance checklist for Synthesis

Research date: 2026-10-28. Scope: quick facts plus a per-area checklist, with a comparison matrix.

**Source notes**
- Claude Code pages are on code.claude.com. None of them shows a publication date. They reference versions up to v2.1.311, so they appear current but this is inferred.
- Codex pages on developers.openai.com now redirect (308) to learn.chatgpt.com. I cite the final URL. None of the Codex pages shows a date.
- Tags: **documented** means the official page states it. **inferred** means I derived it or it is a recommendation for Synthesis.
- Where the request's premise conflicts with the official text, I flag it under "Conflicts" rather than picking silently.
- The official pages I fetched were long. Some sections were truncated or only partly read. Those gaps are listed at the end.

**Source key**
- CC-PERM = https://code.claude.com/docs/en/permissions
- CC-MODES = https://code.claude.com/docs/en/permission-modes
- CC-SET = https://code.claude.com/docs/en/settings
- CC-MEM = https://code.claude.com/docs/en/memory
- CC-HOOK = https://code.claude.com/docs/en/hooks
- CC-SBX = https://code.claude.com/docs/en/sandboxing
- CC-MCP = https://code.claude.com/docs/en/mcp
- CC-HEAD = https://code.claude.com/docs/en/headless
- CC-TOOLS = https://code.claude.com/docs/en/tools-reference
- CC-CTX = https://code.claude.com/docs/en/context-window
- CC-MODEL = https://code.claude.com/docs/en/model-config
- CC-IM = https://code.claude.com/docs/en/interactive-mode
- CC-SUB = https://code.claude.com/docs/en/sub-agents
- CC-BP = https://code.claude.com/docs/en/best-practices
- CC-SEC = https://code.claude.com/docs/en/security
- CX-APP = https://learn.chatgpt.com/docs/agent-approvals-security (from developers.openai.com/codex/agent-approvals-security)
- CX-EXEC = https://learn.chatgpt.com/docs/non-interactive-mode (from developers.openai.com/codex/noninteractive)
- CX-CFG = https://learn.chatgpt.com/docs/config-file/config-reference
- CX-AGM = https://learn.chatgpt.com/docs/agent-configuration/agents-md
- AGM = https://agents.md/
- MCP-SPEC = https://modelcontextprotocol.io/specification/2025-06-18/server/tools

---

## 1. Permissions

**Claude Code**

1. **documented.** Modes are `default` (labeled Manual; `manual` is an alias), `acceptEdits`, `plan`, `auto`, `dontAsk`, `bypassPermissions`. Source: CC-PERM.
2. **documented.** Rules are evaluated deny, then ask, then allow. The first match decides, and specificity does not change the order. Source: CC-PERM.
3. **documented.** A broad deny such as `Bash(aws *)` beats a narrower allow such as `Bash(aws s3 ls)`. A matching ask prompts even when a more specific allow also matches. Source: CC-PERM.
4. **documented.** A bare tool name in deny (for example `Bash`) removes the tool from Claude's context. A scoped deny (`Bash(rm *)`) leaves the tool available and blocks only matching calls. Source: CC-PERM.
5. **documented.** `*` in a Bash rule matches any text including spaces. A rule with no `*` matches one exact command. Put `*` after the subcommand, because `Bash(git *)` allows every git command. Source: CC-PERM.
6. **documented.** A space before a trailing `*` is significant. `Bash(ls *)` matches `ls -la` but not `lsof`. `Bash(ls*)` matches `lsof`. The `:*` suffix (`Bash(ls:*)`) is equivalent to ` *` only at the end of a pattern. Source: CC-PERM.
7. **documented.** Before matching, these wrappers are stripped: `timeout`, `time`, `nice`, `nohup`, `stdbuf`, `command`, `builtin`, zsh `noglob`, and bare `xargs`. Allow rules match past an assignment only for known-safe variables. Deny and ask match past any leading assignment. Source: CC-PERM.
8. **documented.** Compound commands are split on `&&`, `||`, `;`, `|`, `|&`, `&`, and newlines. Every subcommand must be allowed. Deny and ask also apply inside subshells, command substitutions, and loop bodies. An unparseable compound (for example one ending in a bare `&&`) is not split and does not match an allow rule. Commands over 10,000 characters always prompt. Source: CC-PERM.
9. **documented.** Bash rules match command text, not the program. `Bash(curl *)` does not stop `/usr/bin/curl` or `sh -c 'curl …'`. For enforcement, use the sandbox or a PreToolUse hook. Source: CC-PERM, CC-SEC.
10. **documented.** Read and Edit rules use gitignore syntax with four anchors: `//abs` (filesystem root), `~/` (home), `/path` (relative to the settings source's project directory), and `path` or `./path` (relative to cwd). A single leading `/` is not an absolute path. Source: CC-PERM.
11. **documented.** `Write(…)`, `NotebookEdit(…)`, and `Glob(…)` path rules are accepted but never match. Use `Edit(…)` and `Read(…)`. Source: CC-PERM.
12. **documented.** A `Read(path)` deny also blocks Edit and Write to that path, including creating it. Read and Edit denies apply to built-in tools, recognized file commands (`cat`, `head`, `tail`, `sed`, `tee`), and redirection targets. They do not stop a subprocess that opens files itself. Source: CC-PERM.
13. **documented.** Read-only commands run without a prompt in every mode. The set includes `ls`, `cat`, `echo`, `pwd`, `head`, `tail`, `grep`, `find`, `wc`, `which`, `diff`, `stat`, `du`, `cd`, and read-only `git`. It is not configurable. Source: CC-PERM.
14. **documented.** Unparseable commands prompt in Manual mode. Allow rules do not skip that command-injection prompt. Source: CC-SEC.
15. **documented.** "Yes, and don't ask again" writes an allow rule to `.claude/settings.local.json`. For a compound command it saves one rule per subcommand that needed approval, up to 5. Source: CC-PERM, CC-SET.
16. **documented.** Permission rules are enforced by Claude Code, not the model. CLAUDE.md "shapes what Claude tries to do" but does not enforce. Source: CC-PERM, CC-MEM.
17. **documented.** Protected directories: `.git`, `.config/git`, `.vscode`, `.idea`, `.husky`, `.cargo`, `.devcontainer`, `.yarn`, `.mvn`, `.claude` (with exceptions). Protected files include `.gitconfig`, `.gitmodules`, shell startup files (`.bashrc`, `.zshrc`, `.profile`, `.envrc`), `.npmrc`, `.mcp.json`, `.claude.json`, and others. Allow rules do not pre-approve these writes. Source: CC-MODES.
18. **documented.** Per-mode protected-path write outcome: `default` and `acceptEdits` prompt; `plan` allows or defers; `auto` routes to the classifier; `dontAsk` denies; `bypassPermissions` allows. Source: CC-MODES.
19. **documented.** Actions no mode auto-approves, including bypass: ask-rule matches, `AskUserQuestion`, MCP tools marked `requiresUserInteraction`, and recognized critical `rm` and `rmdir` removals. Source: CC-MODES.
20. **documented.** Claude's auto mode blocks by default: `curl | bash`, sending sensitive data externally, force push, `git reset --hard`, `terraform destroy`, and similar. It pauses to prompting after 3 consecutive blocks or 20 total. Source: CC-MODES.
21. **documented.** Shift+Tab cycle: default, acceptEdits, plan, [bypassPermissions if enabled], auto, back to default. `dontAsk` is never in the cycle. Source: CC-MODES.
22. **documented.** Project `permissions.allow` and `additionalDirectories` take effect only after workspace trust. `deny` and `ask` apply immediately. `defaultMode` values `auto` and `bypassPermissions` are ignored from project or local settings. Source: CC-SET, CC-PERM.

**Codex**

23. **documented.** Approval policies: `on-request`, `never`, and a `granular` table that keeps chosen prompt categories interactive and auto-rejects the rest. `untrusted` is "no longer supported" and can stop the client from starting. `on-failure` is deprecated. Source: CX-APP, CX-CFG.
24. **documented.** Sandbox modes: `read-only`; `workspace-write` (writable roots are the cwd and temp directories such as `/tmp`); `danger-full-access` ("No sandbox; no approvals"). Source: CX-APP.
25. **documented.** Network in `workspace-write` is off by default. Enable with `[sandbox_workspace_write] network_access = true`. Source: CX-APP.
26. **documented.** Protected inside writable roots, read-only and recursive: `<root>/.git` (including a `gitdir:` pointer and the resolved git dir), `<root>/.agents`, `<root>/.codex`, and `<root>/.aws`, when they exist. Source: CX-APP.
27. **documented.** Presets: Auto is `--sandbox workspace-write --ask-for-approval on-request`. Non-interactive read-only is `--sandbox read-only --ask-for-approval never`. `--dangerously-bypass-approvals-and-sandbox` (alias `--yolo`) disables both. `--full-auto` is "no longer supported." Source: CX-APP, CX-EXEC.
28. **documented.** `codex exec` runs in a read-only sandbox by default. Source: CX-EXEC.
29. **documented.** Codex loads project `.codex/` config only when the project is trusted. Untrusted projects skip project-local config, hooks, and rules. Version-controlled folders default to Auto. Non-version-controlled folders default to `read-only`. Source: CX-APP, CX-CFG.

## 2. Settings and config locations, precedence, env

30. **documented.** Claude files: user `~/.claude/settings.json`; shared project `.claude/settings.json` (committed); project local `.claude/settings.local.json` (gitignored); managed (`managed-settings.json`, MDM, or server-managed). `~/.claude.json` stores MCP configs and per-project trust state. Source: CC-SET.
31. **documented.** Precedence, highest first: managed, then command line `--settings`, then project local, then shared project, then user. Source: CC-SET.
32. **documented.** Lists merge across non-managed scopes. `fallbackModel` and `modelPicker` take the whole value from one source. Source: CC-SET.
33. **documented.** Settings files are strict JSON. A `//` comment or trailing comma gives a Settings Error at startup. A bad single entry gives a Settings Warning and is skipped. Source: CC-SET.
34. **documented.** `CLAUDE_CONFIG_DIR` relocates `~/.claude` (settings, sessions, plugins). Source: CC-SET.
35. **documented.** Edits to permissions, hooks, and `apiKeyHelper` apply to the running session without restart. Source: CC-SET.
36. **documented.** Environment variables are not a precedence level. Each shell variable and settings key pair is decided separately. An `env` block in a settings file follows the normal levels. Source: CC-SET.
37. **documented.** Codex: user config is `~/.codex/config.toml`, project config is `.codex/config.toml` (trusted projects only). Project-local config cannot set `model_provider`, `notify`, `profile`, `otel`, and similar keys. Source: CX-CFG.
38. **inferred.** Synthesis should read only `SYNT_*` for its own settings and API keys, plus the standard variables listed in the design doc. This is an ADR-011 choice, not an official rule. No official page covers it.

## 3. Memory files

39. **documented.** At launch, Claude loads `CLAUDE.md` and `CLAUDE.local.md` from the cwd and every ancestor directory. Subdirectory files load on demand. Source: CC-MEM.
40. **documented.** Concatenation order runs from filesystem root down to cwd, so files nearer the cwd come last. Within a directory, `CLAUDE.local.md` is appended after `CLAUDE.md`. Files are concatenated, not overridden. Source: CC-MEM.
41. **documented.** Locations: `./CLAUDE.md` or `./.claude/CLAUDE.md`; `~/.claude/CLAUDE.md`; managed policy at `/Library/Application Support/ClaudeCode/CLAUDE.md` (macOS), `/etc/claude-code/CLAUDE.md` (Linux and WSL), `C:\Program Files\ClaudeCode\CLAUDE.md` (Windows). Source: CC-MEM.
42. **documented.** `@path` imports resolve relative to the importing file. Maximum recursion is four hops. Spaces are escaped with a backslash. Quoted paths are not imported. Code spans and fenced blocks are skipped. Imports from outside the working directory need a one-time approval in project files. Source: CC-MEM.
43. **documented.** Size guidance: target under 200 lines per CLAUDE.md. This is a recommendation, not a hard limit. Imports do not reduce context cost. Source: CC-MEM.
44. **documented.** Rules in `.claude/rules/*.md` are discovered recursively. The only frontmatter field read is `paths`. Rules without `paths` load at launch. Source: CC-MEM.
45. **documented.** Auto memory loads the first 200 lines or 25KB every session. Source: CC-MEM.
46. **documented.** AGENTS.md: Claude reads `AGENTS.md` only when no `CLAUDE.md` or `CLAUDE.local.md` exists in the cwd or above (the default). When both exist, Claude reads CLAUDE.md, and AGENTS.md only if imported. Direct AGENTS.md support requires v2.1.277 or later. Source: CC-MEM.
47. **documented.** Codex AGENTS.md: global `~/.codex` takes `AGENTS.override.md`, else `AGENTS.md`, first non-empty. Project discovery starts at the project root (typically the git root) and walks down to cwd. Each directory takes at most one file (override, then `AGENTS.md`, then fallbacks). Files are concatenated root-first. The combined limit is `project_doc_max_bytes`, defaulting to 32 KiB. Source: CX-AGM.
48. **documented.** AGENTS.md is a standard "stewarded by the Agentic AI Foundation under the Linux Foundation." "Agents automatically read the nearest file in the directory tree." Explicit chat prompts override. Source: AGM.

## 4. Hooks

49. **documented.** Events: SessionStart, SessionEnd, Setup, UserPromptSubmit, UserPromptExpansion, Stop, StopFailure, PreToolUse, PermissionRequest, PermissionDenied, PostToolUse, PostToolUseFailure, PostToolBatch, SubagentStart, SubagentStop, TaskCreated, TaskCompleted, TeammateIdle, InstructionsLoaded, ConfigChange, CwdChanged, DirectoryAdded, FileChanged, PreCompact, PostCompact, PreModelSwitch, PostModelSwitch, Elicitation, ElicitationResult, Notification, MessageDisplay, WorktreeCreate, WorktreeRemove. Source: CC-HOOK.
50. **documented.** Matcher: `"*"`, `""`, or omitted matches everything. Letters, digits, `_`, `-`, spaces, `,`, and `|` form an exact name or a list (`Edit|Write`). Any other character makes it an unanchored JavaScript regex (`mcp__memory__.*`). Source: CC-HOOK.
51. **documented.** Matchers are silently ignored on UserPromptSubmit, Stop, TaskCreated, TaskCompleted, WorktreeCreate, WorktreeRemove, MessageDisplay, and CwdChanged. Source: CC-HOOK.
52. **documented.** Per-handler `if` uses permission-rule syntax and applies only to tool events. The docs say it is best-effort. Source: CC-HOOK.
53. **documented.** Common stdin fields: `session_id`, `prompt_id`, `transcript_path`, `cwd`, `scratchpad_dir`, `permission_mode`, `effort`, `hook_event_name`. Tool events add `tool_name`, `tool_input`, `tool_use_id`. Source: CC-HOOK.
54. **documented.** Exit 0: stdout is parsed as JSON if it is a single object. Exit 2: blocking on PreToolUse, UserPromptSubmit, PreCompact, ConfigChange, TaskCreated, TaskCompleted, and similar. Stderr becomes the reason. JSON cannot override it. On Stop and SubagentStop, exit 2 keeps Claude from stopping. Exit 1 and other codes are non-blocking. Source: CC-HOOK.
55. **documented.** PreToolUse JSON: `hookSpecificOutput.permissionDecision` is `allow`, `deny`, `ask`, or `defer`, with `permissionDecisionReason` and `updatedInput`. PermissionRequest uses `decision.behavior` (`allow` or `deny`). Source: CC-HOOK.
56. **documented.** Default timeouts: 600s for `command`, `http`, `mcp_tool`; 30s for `prompt`; 60s for `agent`. Shorter defaults apply on UserPromptSubmit, PreModelSwitch, and PostModelSwitch (30s), and MessageDisplay (10s). SessionEnd has a 1.5s budget, raisable to 60s. On PreToolUse a timeout does not block unless `"onFailure": "block"` is set. Source: CC-HOOK.
57. **documented.** All matching hooks for an event run in parallel. Identical handlers across settings files run once. Source: CC-HOOK.
58. **documented.** Hooks run arbitrary shell with user privileges. Project hooks wait for workspace trust. `allowManagedHooksOnly` blocks non-managed hooks. Command hooks are not sandboxed. Source: CC-HOOK, CC-SBX.
59. **documented.** Codex hooks (feature-flagged): command and MCP tool hooks run; prompt and agent handlers are parsed but skipped. `additionalContextLimit` defaults to 2500. Source: CX-CFG.

## 5. Built-in tool contracts

**Read**

60. **documented.** The Read tool takes a path (Claude is instructed to use absolute paths) and returns line-numbered content. Empty files and offsets past EOF return notices. Source: CC-TOOLS.
61. **documented.** One call returns at most 25,000 tokens by default (`CLAUDE_CODE_FILE_READ_MAX_OUTPUT_TOKENS` overrides). A whole file over 256 KB is refused. Larger files are read with `offset` and `limit`. Source: CC-TOOLS.
62. **documented.** Images: PNG, JPEG, GIF, WebP. Images over 128 MB are refused. PDFs: files of 10 pages or fewer are read whole. Longer PDFs use a `pages` range, up to 20 pages per call. Page rendering needs `pdftoppm` (poppler). Source: CC-TOOLS.
63. **documented.** Notebooks: all cells with outputs. Over 256 KB returns an error. Over 100 MB is refused. Read refuses binary extensions such as `.xlsx`, `.parquet`, `.zip`. Source: CC-TOOLS.
64. **not verified.** The commonly cited default of 2000 lines per Read was not found in the fetched text. The documented limits are token- and byte-based (items 61 and 63). Do not hard-code a line count without re-checking.

**Edit and Write**

65. **documented.** Edit: `old_string` must match exactly once. If it occurs more than once, use a longer context or `replace_all: true`. Source: CC-TOOLS.
66. **documented.** Edit requires a prior read of the file in the conversation for `Opus 4.6`, `Haiku 4.5`, and older models. Newer models may edit an unread file when reading would not need a permission prompt. A partial read with a `PARTIAL view` notice does not count. `cat`, `nl`, `head`, `tail`, `sed -n`, `grep`, and `rg` on a single file count as reads. Source: CC-TOOLS.
67. **documented.** Write: overwriting an existing file requires a prior read, except for newer models under the same conditions. Creating new files does not require a read. Before v2.1.228, every model required the read. Source: CC-TOOLS.
68. **inferred.** For Synthesis, enforce read-before-edit and read-before-overwrite unconditionally. Official behaviour varies by model, and a model-independent rule is safer.

**Glob and Grep**

69. **documented.** Glob results are sorted by modification time and capped at 100 files. Glob does not respect `.gitignore` by default. Grep skips gitignored files. Source: CC-TOOLS.
70. **documented.** On macOS, Linux, and WSL, Claude Code omits the Glob and Grep tools by default. Claude uses `find` and `grep` through Bash, which then reach hooks and permission rules as `Bash` calls. Source: CC-TOOLS.
71. **documented.** Grep `count` mode totals every match even when `head_limit` or `offset` truncates the per-file list. An `offset` past the last match returns `No entries at this offset`. Source: CC-TOOLS.
72. **not verified.** The Grep `head_limit` default and the default output mode were not found in the fetched text.

**Bash**

73. **documented.** Default timeout is 120,000 ms (`BASH_DEFAULT_TIMEOUT_MS`). The ceiling is 600,000 ms (`BASH_MAX_TIMEOUT_MS`). Source: CC-TOOLS.
74. **documented.** A foreground command that hits its timeout is moved to the background, unless it starts with `sleep`. `run_in_background: true` starts a background task. Background default is 30 minutes and maximum 2 hours. In `-p` text mode both defaults are 10 minutes. Source: CC-TOOLS, CC-HEAD.
75. **documented.** Inline output is up to about 30,000 characters. Beyond that, output is saved to a file with a 2,000-character preview. Output files are truncated past 64 MiB. Source: CC-TOOLS.
76. **documented.** `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1` and bare mode disable auto-backgrounding. Source: CC-TOOLS, CC-HEAD.

**Subagents**

77. **documented.** Each subagent starts with a fresh context and does not see the parent conversation. Only its final result returns to the parent. Source: CC-SUB.
78. **documented.** Nesting defaults to three layers below the main conversation. Set `CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH` to change it (`"1"` disables nesting). Source: CC-SUB.
79. **documented.** Explore and Plan are read-only (Write and Edit denied). Explore and Plan skip CLAUDE.md files. Non-fork custom subagents load the CLAUDE.md hierarchy. Source: CC-SUB.
80. **documented.** Frontmatter: `name` and `description` required. Optional fields include `tools`, `disallowedTools` (applied first), `model`, `permissionMode`, `maxTurns`, `hooks`, `memory`, `isolation: worktree`. Unknown fields are ignored silently. Source: CC-SUB.
81. **not verified.** A `TodoWrite` tool with that exact name was not confirmed in the fetched pages. The UI has a "task checklist" toggled with Ctrl+T. Verify the tool name before using it in the design.

**WebFetch and WebSearch**

82. **documented.** WebFetch runs a separate model call and returns that answer, not the raw page, for most fetches. Source: CC-SEC.
83. **documented.** Sandbox `allowedDomains` does not limit WebFetch. Source: CC-SBX.

## 6. Sandboxing

84. **documented.** Claude Code's sandbox uses Seatbelt on macOS, bubblewrap (`bwrap`) plus `socat` on Linux, and WSL2. WSL1 is unsupported. Native Windows is not sandboxed. The sandbox is off by default. Enable with `/sandbox` or `sandbox.enabled: true`. Source: CC-SBX.
85. **documented.** Default writes: the working directory, a per-user temp directory, and added directories. Protected paths stay write-denied even inside those areas. Reads are allowed across most of the machine, including `~/.ssh`. Source: CC-SBX.
86. **documented.** Network: `allowedDomains` starts empty. An entry with no port allows every port on that host. Wildcards such as `*.npmjs.org` are supported. `deniedDomains` blocks in every mode. Loopback and metadata addresses are refused. Source: CC-SBX.
87. **documented.** Unlisted hosts follow the mode: bypass allows, Manual and acceptEdits prompt, auto refuses unless classifier approves, dontAsk refuses. Source: CC-SBX.
88. **documented.** By default, if the sandbox cannot start, Claude Code runs commands unsandboxed. `failIfUnavailable: true` exits instead. `allowUnsandboxedCommands: false` ignores the retry parameter. Source: CC-SBX.
89. **documented.** `excludedCommands` run with full access and are matched textually. Every part of a compound must match. Source: CC-SBX.
90. **documented.** The sandbox covers shell commands only. File tools, MCP servers, and hooks run outside it. The proxy does not inspect TLS by default. Non-TCP traffic cannot leave. Source: CC-SBX.
91. **documented.** Codex: Seatbelt via `sandbox-exec` on macOS. Linux uses `bwrap` plus `seccomp` by default. Landlock appears only in the `codex sandbox landlock` alias. Windows uses MXC, with `elevated` and `unelevated` as legacy fallbacks. Source: CX-APP.

## 7. MCP client practice

92. **documented.** Transports: stdio (local), HTTP (recommended for remote), SSE (deprecated; use HTTP), WebSocket via `"type": "ws"` in config. Source: CC-MCP.
93. **documented.** Scopes: local (default, `~/.claude.json`, current project only, not shared), project (`.mcp.json` at project root, shared), user (`~/.claude.json`, all projects). Precedence: Local > Project > User > Plugin > claude.ai connectors. Claude uses the whole winning entry and does not merge fields. Source: CC-MCP.
94. **documented.** In interactive sessions, Claude prompts before using project `.mcp.json` servers. `claude -p` and Agent SDK sessions load them without asking. A cloned repo cannot approve its own servers in an untrusted folder. Block with `disabledMcpjsonServers`, `--strict-mcp-config`, or `--setting-sources`. Source: CC-MCP.
95. **documented.** Tool naming: `mcp__<server>__<tool>` (the pattern appears in hook and permission examples). Plugin servers use `mcp__plugin_<plugin>_<server>__<tool>`, with characters outside `A-Za-z0-9_-` replaced by `_`. Source: CC-MCP, CC-PERM.
96. **documented.** Allow globs work only after a literal `mcp__<server>__` prefix. `mcp__*` as an allow rule is skipped. As a deny rule it removes all MCP tools. Source: CC-PERM.
97. **documented.** Timeouts: `MCP_TIMEOUT` (startup, default 30s in `-p`); `MCP_TOOL_TIMEOUT` (default about 28 hours); per-server `timeout` in ms. Codex defaults: `startup_timeout_sec` 10s, `tool_timeout_sec` 60s. Source: CC-MCP, CC-HEAD, CX-CFG.
98. **documented.** Output: `MAX_MCP_OUTPUT_TOKENS` defaults to 25,000. Claude warns above 10,000 tokens. Successful text over 50,000 characters is saved to a file unless the server raises the limit. Source: CC-MCP.
99. **documented.** `list_changed`: Claude refetches the list in interactive mode. In `-p` only the tool list is refreshed. If a refresh fails, the previous list is kept. Source: CC-MCP.
100. **documented.** Spec: `tools/list` is paginated. Servers that declare `listChanged` SHOULD send `notifications/tools/list_changed`. Source: MCP-SPEC.
101. **documented.** Spec: unknown tool and invalid arguments are JSON-RPC errors (for example `-32602`). Execution failures return `isError: true` in the result. Source: MCP-SPEC.
102. **documented.** Spec: "clients **MUST** consider tool annotations to be untrusted unless they come from trusted servers." Clients SHOULD show inputs before calling, validate results before passing them to the LLM, and implement timeouts. Source: MCP-SPEC.
103. **not verified.** Resource references (`@server:protocol://`), MCP prompts as slash commands, and tool search details were in truncated sections. Re-fetch before designing around them.

## 8. Context management

104. **documented.** When no window is set, auto-compaction runs at the model's context limit. Sonnet 4.6 and Opus 4.6 without extended context compact at the 200K boundary. Source: CC-MODEL.
105. **partial.** Haiku 5.5 on the Anthropic API auto-compacts at about 967K tokens. The general percentage threshold was not in the fetched text. Source: CC-MODEL.
106. **documented.** `CLAUDE_CODE_AUTO_COMPACT_WINDOW` accepts 100,000 to 1,000,000 tokens. Values outside that range warn. `/autocompact 500k` sets the window. Source: CC-MODEL.
107. **documented.** `/compact` accepts focus text, for example `/compact focus on the auth bug fix`. Source: CC-CTX.
108. **documented.** After compaction: system prompt and output style persist. Root CLAUDE.md and unscoped rules are re-read from disk. Auto memory is re-read. Up to five recently modified files are re-read (files over 5,000 tokens become path references). Invoked skills are capped at 5,000 tokens each and 25,000 total. Path-scoped rules and nested CLAUDE.md reload on demand. Background commands keep running. Source: CC-CTX.
109. **documented.** A model switch makes the first request uncached. Each model has its own prompt cache. Source: CC-SET.
110. **documented.** Keep research out of the main context by delegating to subagents, which return only a summary. Source: CC-CTX, CC-SUB.
111. **not verified.** Codex compaction behaviour was not researched.

## 9. TUI conventions (Claude Code)

112. **documented.** Shift+Tab cycles permission modes (item 21). Source: CC-MODES.
113. **documented.** Ctrl+C interrupts a running operation. When idle, the first press clears the input and a second press exits. Source: CC-IM.
114. **documented.** Esc interrupts the turn without submitting the draft. Double Esc with text clears the draft to history. With an empty input, double Esc opens the rewind menu. Source: CC-IM.
115. **documented.** Ctrl+O toggles the transcript viewer, showing tool detail and timestamps. Ctrl+R reverse-searches history. Ctrl+B backgrounds Bash commands and agents. Ctrl+T toggles the task checklist. `/tasks` lists running shells and subagents. Source: CC-IM.
116. **documented.** `!` enters shell mode. `! npm test` runs a command and sends its output to Claude. `respondToBashCommands: false` adds output without a response. Source: CC-IM.
117. **documented.** `@` opens file path autocomplete. `/` opens the command menu. Source: CC-IM.
118. **documented.** Pressing Enter while Claude works queues the message. Ctrl+Enter (v2.1.275+) sends queued items immediately. Up takes queued items back. Source: CC-IM.
119. **not verified.** The status line configuration (`statusLine` setting) was not fetched.

## 10. Headless mode

**Claude Code (`claude -p`)**

120. **documented.** Exit code 0 on success, non-zero on failure. Invalid flags are reported on stderr before the run. A runtime failure is printed as the result on stdout. SIGTERM exits with 143. Source: CC-HEAD.
121. **documented.** `--output-format`: `text` (default), `json` (result, session ID, metadata, `total_cost_usd`), `stream-json` (NDJSON). Streaming partial messages needs `--verbose --include-partial-messages`. `--json-schema` returns `structured_output`. Source: CC-HEAD.
122. **documented.** Permission handling in `-p`: anything needing approval is denied unless an allow rule, hook, `--permission-prompt-tool`, or `canUseTool` callback approves it. From v2.1.302, `-p` runs without a set mode start in Manual and deny prompts. `--permission-prompts none` (v2.1.259+) also tells Claude not to retry denied calls. `permission_denials` appears in the result. Source: CC-HEAD.
123. **documented.** `--allowedTools` uses permission-rule syntax, for example `"Bash(git diff *),Bash(git commit *)"`. A bare `Bash` allow is dropped in auto mode. Source: CC-HEAD.
124. **documented.** `--bare` skips hooks, skills, plugins, MCP servers, auto memory, and CLAUDE.md discovery. It never reads OAuth or keychain credentials. It is the recommended mode for scripts, and is planned to become the `-p` default. Source: CC-HEAD.
125. **documented.** Stdin is capped at 10 MB. Larger input exits with an error. Source: CC-HEAD.
126. **documented.** `system/init` lists MCP server status and plugin errors. `mcp_server_errors` and `plugin_errors` let CI fail on a skipped server. Source: CC-HEAD.
127. **documented.** `--max-budget-usd` stops remaining background work. `--continue` and `--resume <id>` continue sessions. Source: CC-HEAD.

**Codex (`codex exec`)**

128. **documented.** Progress goes to stderr. Only the final message goes to stdout. `--json` emits JSONL with `thread.started`, `turn.started`, `turn.completed`, `turn.failed`, `item.*`, and `error`. `-o` / `--output-last-message` writes the final message to a file. `--output-schema` constrains output. Source: CX-EXEC.
129. **documented.** Exit codes are not documented. A required MCP server that fails to start makes `exec` exit with an error. Source: CX-EXEC.
130. **documented.** `codex exec resume --last` and `codex exec resume <SESSION_ID>` continue sessions. `--ephemeral` skips saving rollout files. Stdin: a prompt argument plus piped stdin adds context. With no prompt, stdin is the prompt. Source: CX-EXEC.
131. **documented.** Do not put `OPENAI_API_KEY` or `CODEX_API_KEY` in job-level variables in workflows that run repo-controlled code. Set it inline for the one invocation that needs it. Source: CX-EXEC.

## 11. Agent loop practice

132. **documented.** Give Claude a check to run (tests, typecheck, lint, screenshot). Ask for evidence: "the command it ran and its output." "'All tests pass' on its own tells you very little." Source: CC-BP.
133. **documented.** Explore, then plan, then implement. Use plan mode (Shift+Tab or `/plan`) for multi-file work. Source: CC-BP, CC-MODES.
134. **documented.** Course-correct with Esc (interrupt), `/rewind`, or `/clear` between unrelated tasks. Source: CC-BP, CC-CTX.
135. **documented.** For reviews, state what to skip. "A reviewer that's asked to find problems will always find *something*." Source: CC-BP.
136. **documented.** Untrusted content: review suggested commands before approval, avoid piping untrusted content straight into Claude, verify changes to critical files, and use VMs for external web interactions. Source: CC-SEC.
137. **documented.** Prompt-injection safeguards: manual permission prompts, network command approval (curl and wget prompt in Manual mode), command-injection detection (unparseable commands prompt), and fail-closed matching in Manual mode. Source: CC-SEC.
138. **documented.** CLAUDE.md is context. Hard blocks belong in PreToolUse hooks or permission deny rules. Source: CC-MEM.
139. **partial.** Error recovery: the `system/api_retry` event carries `error` categories (`rate_limit`, `overloaded`, `server_error`, and others) and `attempt`/`max_retries`. Source: CC-HEAD. No official guidance on agent-level recovery was found.
140. **not verified.** No Codex equivalent of the verification and planning guidance was fetched.

---

## Conformance matrix (version and behaviour)

| Behaviour | Claude Code | Codex | Notes |
|---|---|---|---|
| Approval modes | 6 modes: default, acceptEdits, plan, auto, dontAsk, bypassPermissions | 2 policies (on-request, never) plus granular; `untrusted` retired | Premise listed `untrusted`. Official docs say unsupported. |
| Sandbox | Off by default; `/sandbox` or `sandbox.enabled` | On by default in workspace-write and read-only | Opposite defaults. |
| Linux sandbox | bwrap + socat | bwrap + seccomp | Landlock only in Codex alias. |
| Windows sandbox | Not sandboxed (use WSL2) | MXC / elevated / unelevated | |
| Network default | Empty `allowedDomains` | Off in workspace-write | Both deny by default. |
| Protected paths | Prompt, classifier, deny, or allow depending on mode; bypass allows | Read-only inside writable roots | Codex is enforced; Claude's is mode-dependent. |
| Default in `-p` | Manual; denies prompts without a rule or hook | `codex exec` read-only | |
| Default start mode | auto (v2.1.283+, terminal) | Auto for version-controlled folders | |
| Memory file | CLAUDE.md (AGENTS.md fallback) | AGENTS.md (32 KiB cap) | |
| Hooks | Yes, many events | Feature-flagged; prompt and agent handlers skipped | |
| `--full-auto` | n/a | Removed | |
| `--permission-prompts none` | v2.1.259+ | n/a | |

**Better default for Synthesis (judgement, not official)**
- Adopt Claude's deny, then ask, then allow evaluation with compound-command splitting (items 2, 8).
- Enforce protected-path read-only at the sandbox layer, as Codex does, rather than relying on prompts (items 17, 91).
- Default network to off, and make sandbox failure fail closed in autonomous mode. Claude's default fallback to unsandboxed is weaker (item 88).
- Make PreToolUse safety hooks fail closed. Claude's default lets a timed-out hook pass (item 56).
- Read AGENTS.md as the cross-tool standard, with a size cap like Codex's 32 KiB. Keep CLAUDE.md support if the design needs it.

---

## Conflicts with the request's premise

- **Codex `untrusted` approval:** the request lists it. The official page says it is retired and unsupported (CX-APP, CX-CFG). Do not model it as a live mode.
- **Codex Linux sandbox:** the request says Landlock. The official page says bwrap plus seccomp by default, with Landlock only as an alias (CX-APP). Claude uses bwrap.
- **Codex secret-variable filtering:** the request assumes KEY, SECRET, and TOKEN variables are excluded by default. The config page says `shell_environment_policy.ignore_default_excludes` defaults to `true`, which keeps those variables. Excluding them requires `false` (CX-CFG). Verify before relying on either reading.
- **Codex `--full-auto`:** the request implies it is current. The official page says it is no longer supported (CX-EXEC).
- **Codex protected paths:** the official list also includes `.agents` and `.aws`, not only `.git` and `.codex` (CX-APP).

## Known gaps

- Managed-settings file paths per OS (`managed-settings.json`) were not fetched.
- Claude auto-compaction threshold for models other than Haiku 5.5 was in a truncated section (CC-MODEL).
- The Read default line count and the Grep `head_limit` default were not in the fetched text (items 64, 72).
- Claude MCP resources and prompts, and tool search details, were truncated (item 103).
- The status line spec (CC-IM) and Codex compaction and TUI conventions were not researched.
- The Codex exit-code table is not documented (item 129). The Codex approval-policy default was not on the fetched config page.
- The Claude sandboxing page and the permission-modes page were fetched in full only for the sections shown. Remaining sections were not read.
- Page dates were not shown on any official page, so version currency is inferred.

*Not done: no files were written. This is the report text only; save it to docs/research/official-conformance.md if you want it on disk.*

**Skipped or unchecked:** the Codex TUI and compaction, the Claude status line, and MCP resources and tool search. The "Better default" items are my judgement, not official behaviour.

**Risk:** the Codex and Claude pages may have changed since the cached copies, and the premise conflicts above (`untrusted`, Landlock, the secret-variable default) need a decision before they are written into Synthesis's design doc.