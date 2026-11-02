# Browser login and secret fill

How the agent gets into a site that needs a login without ever holding the credential. Status: implemented, unit and real-Chrome (headless) tested; the headed window itself is unverified on a real desktop (see "Not covered / unverified").

## Decision (ADR-style)

**Context.** The browser runs headless, so a person cannot type into a login form, and the model must never see passwords, cookies or tokens (ADR-011: API keys the user manages; Rust never writes credentials; page text is untrusted data).

**Decision (owner, approved).** Two mechanisms, aimed at credentials that do not enter the model context, transcript, events, logs, tool output, session database or artifacts. What actually holds (see "What is and is not protected" below): the tools never return a credential, and the harness masks the exact values it knows; it does not hold against a page that echoes a value in a form it cannot recognise, against a compromised bound site, or against the agent's own commands when the sandbox is off:

- **A. Headed login hand-off** (`browser_login`). A VISIBLE Chrome opens on a dedicated persistent profile; the person logs in (password, 2FA, CAPTCHA are theirs); the browser closes; later headless browsers reuse the profile (`browser_navigate` with `profile`).
- **B. Secret fill by reference** (`browser_fill_secret`). The person declares named secrets in their USER settings; the model passes only the name and an element ref; Rust reads the value and types it through CDP.

**Rejected.** Reading the person's real Chrome profile or cookies (cross-profile theft risk, locks, encryption keys). Letting the model pass a profile path or a literal password. Returning cookies to the model. Solving CAPTCHA or 2FA automatically. Subscription/OAuth login reuse (ADR-011).

## Tools

| Tool | Input | Result |
|---|---|---|
| `browser_login` | `origin` (https, or http on localhost/127.0.0.1/[::1]; no path, query or userinfo), `profile?` (`[a-z0-9-]`, 1-48; default from the host, `app.example.com` -> `app-example-com`) | `{ok, origin, profile, cookie_count_for_origin}` only |
| `browser_fill_secret` | `ref` (from `browser_snapshot`), `secret` (a name) | `{ok, filled: true, secret}` only |
| `browser_navigate` | gains optional `profile` (first browser call of a session only) | unchanged |

`browser_login` flow: validate origin and profile name -> require `--sandbox enforce` (or `synt.browser.allow_unsandboxed`) -> check a display exists -> permission engine asks the person (always) -> create `<SYNT_HOME>/browser-profiles/<name>` (mode 0700, symlinks refused, exclusive lock) -> launch a visible Chrome on it and navigate to the origin -> emit `browser.login_required {origin, profile}` -> wait for the person -> on confirmation close Chrome gracefully (so cookies reach disk), count the cookies for the origin, record the site's registrable domain (public-suffix list) and, for a loopback origin, its port in `<profile>/synt-profile.json` (atomic write) ONLY when the origin has at least one cookie (otherwise `ok:false`, nothing recorded) -> return the count. The login window runs with Chrome's password manager, autofill and save prompts switched off in the profile's Preferences. Timeout (default 10 minutes), cancel (Esc / Ctrl-C) and any error kill the whole browser process tree (process group on POSIX, job object on Windows) and return an error.

Using a profile: `browser_navigate {url, profile}` as the FIRST browser call of a session starts the session's headless browser on that profile (default browser context, so the cookies apply). The person is asked once per session, with the profile name and the domains it holds a login for, outside the permission engine (no mode and no allow rule skips it; `synt.browser.approved.profiles` pre-approves named profiles for runs nobody can answer). Persistent profiles and the login window are refused when the DevTools transport is TCP (Windows) until the pipe transport exists. Top-level navigation is then limited to the domains in the profile record plus any the person added in user settings (`synt.browser.profiles.<name>.allow_domains`). Subresources are not limited by this list (pages load their CDNs); the normal URL policy still applies to them. A second browser on the same profile (another session, or the login window) fails with "already open".

## Who answers

| Front | Approval of `browser_login` | Ending the login wait | Approval of a saved login / first secret use |
|---|---|---|---|
| `synt run` (headless) | `[y/N]` on the terminal (`/dev/tty`, `CONIN$` on Windows); no terminal = denied, as every other ask | Enter on the terminal, or `synt browser done <profile>` in another terminal, which asks for the one-time code that is printed on the waiting terminal (never in argv, events or stderr logs) | `[y/N]` on the terminal; no terminal = refused, unless pre-approved in user settings |
| The classic TUI (`synt`, `synt tui`) | its permission dialog (the permission mode cannot answer it) | the dialog "Log in to ORIGIN in the opened browser, then confirm": Enter = done, Esc = cancel; the one-time code for `synt browser done` is shown in the dialog only | the same dialog style: "Browse with saved login ..." / "Type stored secret ... into ORIGIN?" with the name and origin, never the value (`synthesis_app::login::DialogHost`) |
| Another front with dialogs | its `Approver` | implement `synthesis_tools::BrowserHost::login` | `BrowserHost::confirm_profile(name, domains)` / `confirm_secret(name, origin)` |

Terminal handling: ONE thread reads the terminal and prompts are serialized, so two prompts can never answer each other; typed-ahead input is flushed (`tcflush`) before each prompt; a cancelled wait leaves no second reader behind. The waiting login writes a `synt-login-waiting` marker in the profile; `synt browser done` refuses without it, never follows symlinks (`O_NOFOLLOW`), and the wait only accepts a file whose content is the code. A wrong code is ignored (the file is discarded, the marker and the wait stay) and counted; the fifth wrong code cancels the login with a clear message. The code is printed on the terminal only: with no terminal that can be written to, the login does not start (there is no stderr fallback). Anything printed on a terminal stays in tmux/screen scrollback and in a terminal recorder; the code is one-time and dies with the wait, but do not run a login inside a shared or logged multiplexer session. A sandboxed command cannot open `/dev/tty` (no controlling terminal) and the sandbox hides the profile directory, so it can neither read the scrollback through the terminal nor write a right code; that is read from the design, not tested against a recorder. The profile directory is in the protected paths. Commands the agent runs start in their own session (`setsid`), so they have no controlling terminal: `/dev/tty` cannot be opened by them. TIOCSTI: with no controlling terminal a command cannot inject into the person's terminal through `/dev/tty`; the macOS Seatbelt profile allows writing only the literal `/dev/tty` (and `/dev/null`), not `/dev/ttysNNN`, but a TIOCSTI ioctl could not be exercised from safe Rust, so that part is read from the profile, not tested. Linux 6.2+ disables TIOCSTI by default (`dev.tty.legacy_tiocsti`).

No display: on Linux without `DISPLAY` and `WAYLAND_DISPLAY` (SSH, CI, containers) `browser_login` returns an error that names option B. The check reads those two variables through the injectable `Env` only, and a visible browser is started with `DISPLAY`, `WAYLAND_DISPLAY` and `XAUTHORITY` from that same `Env` (owner decision 2026-11-03: these display variables are kept, each serving a feature). macOS and Windows are assumed to have a desktop (an SSH session into a Mac is not detected: the window opens on the Mac's screen).

## Settings (USER settings only: `<SYNT_HOME>/settings.json`)

```json
{
  "synt": {
    "browser": {
      "secrets": {
        "acme-password": {
          "source": "{env:SYNT_ACME_PASSWORD}",
          "origins": ["https://app.acme.com"],
          "field_kind": "password"
        },
        "acme-user": { "source": "{file:/home/me/.config/acme-user}", "origins": ["https://app.acme.com"], "field_kind": "username" }
      },
      "profiles": { "acme": { "allow_domains": ["acme-cdn.com"] } },
      "login_timeout_sec": 600
    }
  }
}
```

| Key | Meaning |
|---|---|
| `secrets.<name>.source` | `{env:SYNT_X}`: a `SYNT_*` variable whose name contains KEY, TOKEN, SECRET, PASSWORD, CREDENTIAL, AUTH, PRIVATE, SESSION or COOKIE as a segment (so `Env::child_env` never hands it to commands the agent runs). Read from the process environment through `Env` at call time. `{file:/abs/path}`: absolute, a regular file, not a symlink, on unix mode must not allow group/other access (`chmod 600`), at most 4096 bytes, one trailing newline removed. Missing or empty = error naming the source, never the value. |
| `secrets.<name>.origins` | Non-empty list of exact origins (scheme + host + port; https, or http on loopback; no path). |
| `secrets.<name>.field_kind` | `password` (only into `type=password`), `username` or `totp` (never into `type=password`). Omitted: any text field. |
| `profiles.<name>.allow_domains` | Extra domains (and subdomains) the headless browser may navigate to on that profile. |
| `login_timeout_sec` | 10 to 3600, default 600. |
| `allow_unsandboxed` | `true` lets the credential tools run when the sandbox is not `enforce`. Default false: they refuse (see "What is and is not protected"). |
| `allow_with_mcp` | `true` lets the credential tools run while stdio MCP servers are configured (see "What is and is not protected"). Default false: they refuse and name the servers. |
| `approved.profiles` | Profile names that need no per-session confirmation (for runs nobody can answer). |
| `approved.secrets` | `[{ "secret": name, "origin": exact origin }]` pairs that need no first-use confirmation; the secret must exist and be bound to that origin. Without a pre-approval a run with no terminal refuses. |

The `synt.browser` block is read from the user's own settings file only. In project, local, managed and `--settings` files it is ignored with the diagnostic `synt.browser is read from user settings only, ignored` (an untrusted project drops all `synt` keys). An invalid secret entry is skipped with a warning that names the entry, never a value.

## Rules of `browser_fill_secret`

1. The secret must exist in user settings; unknown names are refused.
2. The current page's origin is read from the page at call time (so after redirects) and must equal one of the secret's origins exactly.
3. The target element is measured in its own frame: its frame origin must equal the page origin (cross-origin iframes refused), it must be an `input` (text, password, email, search, tel, url, number) or `textarea`, enabled, not read-only, and match `field_kind`. Its form (including `input[form=]` and any `formaction`) must submit to one of the secret's origins. It must be at least 8x8 px, inside the viewport, not covered (`elementFromPoint` returns it), with no ancestor that is hidden, below 10% opacity, clipped (`clip-path`, `clip`) or cutting it off with `overflow`.
4. The value is resolved BEFORE the person is asked, so a missing variable fails without a prompt.
5. First use per session of each (secret, origin) asks the person with the secret NAME and the origin (never the value). This is separate from the permission engine, so no mode (auto, bypass) and no allow rule skips it.
6. Verification and the write are ONE in-page call (`secret::FILL_JS`) in an ISOLATED world of the main frame (`Page.createIsolatedWorld` + `DOM.resolveNode` with that context), so page scripts cannot have replaced the prototypes and DOM getters the check relies on (elements of a child frame cannot be resolved there and fail closed): the value is set through the native setter and announced with `input`/`change` events, so nothing can navigate or restyle between check and write. The capture guard is a set keyed by (tab, document) and is armed BEFORE the call is sent, so a failed follow-up or a cancelled call leaves it armed; it ORs the "too short to mask" flag (a long password typed after a short username does not release it) and a fill in one tab never releases another tab. While armed, `browser_screenshot` is refused (pixels cannot be masked); after a value too short to mask also `browser_snapshot`, `browser_console`, `browser_network`, the tab listing and the text of a same-document navigation. A new document in that tab releases it. If the page breaks the `input`/`change` events after the value was set, the result is `filled: "maybe"` and the guard stays armed.
7. A value of 6 or more bytes is registered for masking before it is typed (at most 64 values, least recently typed dropped first). From then on `redact_text` replaces it, and its form-urlencoded, `encodeURIComponent`, percent-encoded (upper/lower hex), JSON, HTML-entity, URL-path-segment, JSON `\\uXXXX` and base64 (standard/url-safe, padded or not) spellings, plus `base64(a:b)` for two declared secrets (Basic auth), in every snapshot, console, network, tab and URL text the browser returns (`WebFetch` does not use this path). Exact values of all declared secrets that resolve at start are also added to the event sink, session store, task database and memory secret list, so they are masked in events, transcripts and the session database.

## Threat model

| Attack | Mitigation |
|---|---|
| A page (or prompt injection in it) asks the model to fill the password on its own, look-alike or redirected domain | Exact origin binding, re-read from the page at fill time (after redirects); the model only names a secret, so it cannot choose a destination the user did not bind |
| Attacker iframe of another origin inside the bound page | Frame origin of the element must equal the top origin; the AX tree used for refs does not expose cross-origin iframe fields in current Chrome (observed, see below) |
| Page navigates between the check and the keystrokes | Second origin and focus check right before `Input.insertText`; failure types nothing |
| Hidden or off-screen field used to harvest the value | Visible, enabled, editable required; password secrets only into `type=password` |
| Value reaches the model through a page echo, console or network log | `mask_secret` on every capture path; tested with a page that echoes the value into text, console and a request URL |
| Value reaches events, transcript, database | Declared secret values added to the event sink / store / task DB secret list at start; tool result never carries it; `redact_input` keeps the input to `{ref, secret name}` |
| Project or managed settings define or widen a secret | `synt.browser` read from user settings only; diagnostic otherwise |
| Env source that every command inherits | The variable name must be secret-shaped `SYNT_*`, which `Env::child_env` drops |
| File source swapped for a symlink or group-readable file | Symlinks refused; unix mode check; absolute path; size cap |
| Model auto-approves its own login | `browser_login` asks in every mode (default, acceptEdits, plan, auto, bypassPermissions); allow rules and "allow always" do not apply; dontAsk and non-interactive runs deny; deny rules still win |
| A peer session or sub-agent delegates a login | Harness refuses both tools for delegated (peer-origin) work before any prompt; `ctx.sub_agent` is refused inside the tools |
| Login window used to reach internal services | Origin validation (https only, loopback http only, no userinfo, no private or link-local IP literals); the runtime URL policy and DNS check apply to the window |
| Profile path chosen by the model, symlink or traversal | Only a validated name is accepted; the directory is a direct child of `browser-profiles`, created 0700, symlinks refused, canonical parent checked |
| Real Chrome profile or cookies read | Never touched: a dedicated user-data-dir only |
| Two browsers corrupt one profile | Exclusive file lock held while a browser runs on it; Chrome's own singleton as a second line |
| Hung login leaves a browser running | Timeout, cancel and errors kill the process tree; `KillOnDrop` backs it up |
| Stolen profile used by a headless run on other sites | Top-level navigation limited to the domains the person logged in to unless widened in user settings |
| Cookie values returned | The tool returns only a count |
| Agent's own tools read the saved profile (cookies) or a `{file:}` secret | `browser-profiles/` and every `{file:}` secret path join Synthesis's state paths: hidden from sandboxed commands, denied to Read/Grep/Glob, protected from writes; the credential tools refuse unless the sandbox is `enforce` (or `allow_unsandboxed`) |
| Agent approves its own saved-login use | Per-session confirmation of the profile (name + domains) through `BrowserHost`, outside the permission engine; skippable only by the person's `approved.profiles` |
| Stdio MCP servers (outside the sandbox) read profiles or secret files | The credential tools refuse while any enabled stdio MCP server is configured, naming it, unless `synt.browser.allow_with_mcp`; `synt run` and `synt doctor` print the same warning |
| A stored profile outlives an enforce run and a later non-enforce run exposes it | Start-up warning (`login::exposure_warnings`, also for a TUI start-up) when saved logins exist and the sandbox is not enforce; the main agent's Bash is denied any command that names the directory `browser-profiles` as a path component (absolute, `~`, `$SYNT_HOME`, or relative after `cd`), and gets the stricter secret-aware Bash scan (`deny_secret_reads`) whenever a profile or `{file:}` secret exists |
| Short username then long password releases the capture guard; a cancelled fill leaves a value unguarded; a second tab's fill releases the first | Guard set per (tab, document), armed before sending, flags ORed; tested |
| Console, network, tab titles or navigation text echo a short secret | Refused (not redacted) while the guard is armed |
| The person is nagged into approving | Two refusals of the same profile or (secret, origin) in a session end the questions |
| Page script spoofs the checks (prototype pollution) | The check and write run in an isolated world |
| Persistent profile or login window on the TCP transport (Windows) | Refused until the pipe transport exists |
| Chrome stores the typed password (password manager, autofill, save prompt) | Switched off in the profile's Preferences and by `--disable-save-password-bubble` before each launch |
| Screenshot shows a typed value | `browser_screenshot` is refused until the page is a new document; snapshots too after a value too short to mask. The show-password toggle of a page is not supported/handled: do not press it |
| Page changes between check and write (TOCTOU) | Check and write are one in-page call |
| Form submits the value elsewhere (`action`, `formaction`, `form=`) | The form's targets must all be the secret's origins |
| Invisible, covered, clipped or tiny field | Viewport, `elementFromPoint`, ancestor opacity/clip/overflow and a minimum size are checked |
| Stale or foreign terminal input answers a prompt | One serialized terminal reader, `tcflush` before each prompt, commands without a controlling terminal (`setsid`) |
| The agent ends the login wait itself | Marker + one-time code shown only on the terminal; `done` reads the code from stdin, refuses without a waiting login, ignores symlinks; profile dir is protected |
| Project settings redirect the browser binary | With `synt run` the browser is found and started from the process environment snapshot only, never from settings `env` |
| Guessing a short secret through masking | Values under 6 bytes are not masked by content; the capture guard applies instead. Masking is an oracle for guesses of longer values (a page can learn "this string equals the secret" from `[REDACTED:secret]`); the origin binding limits who can try |
| Over-broad domain record, wrong suffix | Registrable domain from the public-suffix list (`psl`), recorded only when the origin has cookies |
| Loopback login opens all private ports | Only that origin's port is opened, and it is the only loopback port the profile may use later |

## Headless, CI, WSL, SSH

- **CI and containers:** no display; use B. Declare the secret in the runner's user settings, supply the value as `SYNT_<NAME>_PASSWORD` (a CI secret) or a `0600` file, and bind it to the staging origin.
- **SSH-only Linux:** same as CI. `ssh -X` / a forwarded `DISPLAY` makes A work (the window opens on your screen).
- **WSL:** works with WSLg (`WAYLAND_DISPLAY` is set). Without it, use B. Chrome must be installed inside WSL or pointed to with `SYNT_BROWSER_PATH`.
- **macOS / Windows desktop:** A works as is. On a remote-desktop-less Windows service session there is no window: use B.
- **Docker as root:** Chrome's sandbox may not start; the existing `no_sandbox` option applies to both modes (not recommended for untrusted sites).

## What is and is not protected

- **Tool outputs, events, transcript, session database, memory:** never carry a credential from the tools; declared secret values of 6+ bytes that resolve at start are masked by exact match in events, the store and the task database. A secret that is unset at start but set later is masked in page captures only, not in those sinks.
- **The agent's own commands and file tools** can read the saved profile and `{file:}` secret files unless the sandbox confines them. That is why the credential tools require `--sandbox enforce` (a working backend: Seatbelt, Landlock/bwrap) or the user setting `allow_unsandboxed: true`. On Windows the default sandbox is best-effort, so the tools refuse there unless that setting is true (and the TCP transport refuses them anyway today).
- **Env-source secrets** live in the environment of `synt` itself: on Linux the same user can read `/proc/<synt pid>/environ`, and `ps eww` shows another process's environment on several systems. A sandboxed command cannot reach the process tree by name only where the sandbox denies it; do not count on that. Prefer `{file:}` secrets with mode 0600, which the sandbox hides, over environment variables for anything that matters.
- **Chrome's profile store:** Chrome encrypts cookies with a key held in the OS keystore on a normal profile. Synthesis starts Chrome with `--use-mock-keychain` and `--password-store=basic` (it must not touch the person's keychain, and a headless service has none), which is a constant key: treat the profile directory as holding the sessions in clear. An OS-keystore-backed profile was not implemented. The directory is 0700 and in the protected paths.
- **Stdio MCP servers** are ordinary child processes of the session started outside the sandbox with the session's environment; they can read the profile directory and secret files with your user's rights. The credential tools therefore refuse while one is configured unless `allow_with_mcp`. Starting MCP servers inside the sandbox looks cheap to build (the MCP manager spawns them with `process-wrap` in one place, `synthesis-mcp/src/manager.rs`, and the sandbox backend already wraps a program, arguments and environment, so a policy that denies `synthesis_state_paths` and allows network and the usual system reads could be applied there, with a per-server opt-out); it is not built, and the first server that needs a denied path would break, so it needs its own decision.
- **Page JavaScript** sees what is typed into its own field, like any site. The origin binding limits this to the bound site.

## Not covered / unverified

- The headed window was not exercised on a real desktop in the implementation environment; the profile, cookie persistence, lock, kill and fill paths are tested against real headless Chrome, the headed-only part is the missing `--headless` flag, the skipped viewport emulation and the display variables passed to Chrome.
- Screenshots of a `username`/`totp` value are refused after a fill, but a person's own later page content can still show it; use `field_kind: password` for anything sensitive.
- That Chrome honours the Preferences keys (`credentials_enable_service`, `profile.password_manager_enabled`, `autofill.*`) was not observed in a headed window; the file contents are tested, the behaviour is read from Chrome's documented preferences.
- Chrome's own cookie encryption key is the mock-keychain constant (see above).
- Masking limits that remain: a page can still use `[REDACTED:secret]` as an oracle for guesses of a long value; a value echoed reversed, split across nodes or text runs, case-folded, or mixed into another encoding (double encoding, rot13, hashing) is not recognised; Basic auth is covered only for two declared secrets.
- The isolated world was verified in real Chrome by the fill tests; the "maybe typed" path (an exception while dispatching events) cannot be provoked from page script in an isolated world and is covered by reading only.
- A loopback login (`http://localhost:...`) opens that origin's port only.
- Cross-origin iframe refusal is implemented in the page function, but in real Chrome the cross-origin iframe field is not in the accessibility tree the refs come from, so the refusal could not be provoked end to end.
- One browser per profile: a session whose headless browser already runs cannot also open the login window for the same profile (locked).
- `tcflush` and the terminal reader are exercised on macOS only; Windows `CONIN$`/`CONOUT$` and Linux are untested here. TIOCSTI itself was not exercised (see "Terminal handling").
- `setsid` for commands is tested with `ps` on this machine; whether every sandbox backend still lets a command open a terminal device by path is not tested.
- Fills use the native value setter with `input`/`change` events, not key events: sites that require real key events (some custom inputs) will not accept the value. Windows ConPTY and legacy conhost are untouched.
