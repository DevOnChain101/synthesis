---
name: synthesis-config
description: Create or convert Synthesis skills, hooks, guards, agents, and model routing from Claude Code, Codex, or another coding CLI. Use when authoring or importing Synthesis configuration; not for ordinary application code.
allowed-tools: [Read, Glob, Grep, Write, Edit, Bash]
---

# Synthesis configuration

Create native resources under `~/.synthesis` for user scope or `.synthesis` for project scope. Treat source CLI configuration as read-only. Never copy credentials, tokens, secret headers, OAuth state, or secret-looking environment values.

For Claude Code or Codex conversion, parse source formats and map paths, secrets, models, hooks, and schemas deterministically first. The optional LLM step may rewrite instruction prose only. It must receive bounded input, have no tools or write access, and return `convertedText`, `unresolvedRefs`, and `assumptions`. Validate that response; if no model is configured or validation fails, keep the normalized source text and report a warning.

Use these native locations:

- `skills/<name>/SKILL.md`: YAML frontmatter with `name` and a discriminating `description`, followed by concise instructions.
- `agents/<name>.md`: frontmatter may contain `name`, `description`, `tools`, `model`, `profile`, `reasoning`, and `required-capabilities`; the body is the system prompt.
- `settings.json`: `hooks` and permission guards. Preserve Synthesis event names and validate every command/path.
- `synthesis.json` (user scope) or `.synthesis/config.json` (project scope): providers, profiles, MCP, and routing. Keep credentials as `{env:NAME}` references or use `synt auth login`.

Agents should normally use `profile: inherit`. Use `fast`, `balanced`, or `deep` profiles for portable intent and a concrete `model` only when the user explicitly requires one. Named agents use their own `model`/`profile`; there is no global sub-agent profile. Direct `@agent` flags may override that definition, while model-generated Task fields cannot. A provider describes transport separately from the advertised model:

```json
{
  "providers": {
    "gateway": {
      "type": "openai",
      "baseURL": "https://gateway.example/v1",
      "models": {
        "gemini-2.5-pro": {
          "vendor": "google",
          "family": "gemini",
          "capabilities": { "toolCalling": true, "vision": true, "reasoning": true }
        }
      }
    }
  },
  "profiles": {
    "deep": { "candidates": ["gateway/gemini-2.5-pro"] }
  }
}
```

A profile may list candidates from several providers. The prefix before the first `/` is the provider key, so candidate model IDs may themselves contain slashes. Only candidates on the same configured provider as the parent session are eligible; this applies to OpenAI, Anthropic, and arbitrary custom provider IDs. Candidates must satisfy `required-capabilities`. When metadata is available, higher `quality`/`qualityScore`/`qualityRank` and stronger `reasoning`/`thinking`/`effort_tiers` rank ahead of configuration order; order is the deterministic tiebreaker. If no candidate matches a known parent provider, the sub-agent inherits the parent. If the parent provider is unknown, Synthesis tries the first eligible `balanced` candidate, otherwise inherits.

When converting, parse the source format into meaning first, then render Synthesis-native files. Rewrite hard-coded source paths to the new resource directory or a workspace-relative path. Remove source-product identity claims and vendor-specific operational instructions. Preserve unknown fields in an import report instead of guessing. Do not create or edit `AGENTS.md`; only `/init` owns it.

Before writing, stage converted files outside the destination, validate the staged files, review warnings and conflicts, then commit the staged files atomically. Do not overwrite existing resources unless the user explicitly approves it.
