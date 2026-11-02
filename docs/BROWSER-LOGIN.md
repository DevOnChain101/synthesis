# Browser login and secret fill

How the agent gets into a site that needs a login without ever holding the credential. Status: implemented, unit and real-Chrome (headless) tested; the headed window itself is unverified on a real desktop (see "Not covered / unverified").

## Decision (ADR-style)

**Context.** The browser runs headless, so a person cannot type into a login form, and the model must never see passwords, cookies or tokens (ADR-011: API keys the user manages; Rust never writes credentials; page text is untrusted data).

**Decision (owner, approved).** Two mechanisms, both ending with credentials that never enter the model context, transcript, events, logs, tool output, session database or artifacts:

- **A. Headed login hand-off** (`browser_login`). A VISIBLE Chrome opens on a dedicated persistent profile; the person logs in (password, 2FA, CAPTCHA are theirs); the browser closes; later headless browsers reuse the profile (`browser_navigate` with `profile`).
- **B. Secret fill by reference** (`browser_fill_secret`). The person declares named secrets in their USER settings; the model passes only the name and an element ref; Rust reads the value and types it through CDP.

**Rejected.** Reading the person's real Chrome profile or cookies (cross-profile theft risk, locks, encryption keys). Letting the model pass a profile path or a literal password. Returning cookies to the model. Solving CAPTCHA or 2FA automatically. Subscription/OAuth login reuse (ADR-011).

## Tools

| Tool | Input | Result |
|---|---|---|
| `browser_login` | `origin` (https, or http on localhost/127.0.0.1/[::1]; no path, query or userinfo), `profile?` (`[a-z0-9-]`, 1-48; default from the host, `app.example.com` -> `app-example-com`) | `{ok, origin, profile, cookie_count_for_origin}` only |
| `browser_fill_secret` | `ref` (from `browser_snapshot`), `secret` (a name) | `{ok, filled: true, secret}` only |
| `browser_navigate` | gains optional `profile` (first browser call of a session only) | unchanged |

`browser_login` flow: validate origin and profile name -> check a display exists -> permission engine asks the person (always) -> create `<SYNTHESIS_HOME>/browser-profiles/<name>` (mode 0700, symlinks refused, exclusive lock) -> launch a visible Chrome on it and navigate to the origin -> emit `browser.login_required {origin, profile}` -> wait for the person -> on confirmation close Chrome gracefully (so cookies reach disk), count the cookies for the origin, record the site's registrable domain in `<profile>/synthesis-profile.json` -> return the count. Timeout (default 10 minutes), cancel (Esc / Ctrl-C) and any error kill the whole browser process tree (process group on POSIX, job object on Windows) and return an error.

Using a profile: `browser_navigate {url, profile}` as the FIRST browser call of a session starts the session's headless browser on that profile (default browser context, so the cookies apply). Top-level navigation is then limited to the domains in the profile record plus any the person added in user settings (`synthesis.browser.profiles.<name>.allow_domains`). Subresources are not limited by this list (pages load their CDNs); the normal URL policy still applies to them. A second browser on the same profile (another session, or the login window) fails with "already open".

## Who answers

| Front | Approval of `browser_login` | "Done" signal | Approval of first secret use |
|---|---|---|---|
| `synth run` (headless) | `[y/N]` on the terminal (`/dev/tty`, `CONIN$` on Windows); no terminal = denied, as every other ask | Enter on the terminal, or `synth browser done <profile>` from another terminal (creates a signal file) | `[y/N]` on the terminal; no terminal = refused |
| A front with dialogs (TUI, desktop) | its `Approver` (the harness always asks) | implement `synthesis_tools::BrowserHost::login`: show "Log in to <origin> in the opened browser, then confirm" | `BrowserHost::confirm_secret(name, origin)` |

Instruction in headless mode (stderr): `synth: log in to <origin> in the browser window that just opened ... Press Enter here, or run synth browser done <profile>`. A run with neither a terminal nor anyone to run `synth browser done` ends at the timeout and kills the browser.

No display: on Linux without `DISPLAY` and `WAYLAND_DISPLAY` (SSH, CI, containers) `browser_login` returns an error that names option B. The check reads those two variables through the injectable `Env` only. macOS and Windows are assumed to have a desktop (an SSH session into a Mac is not detected: the window opens on the Mac's screen).

## Settings (USER settings only: `<SYNTHESIS_HOME>/settings.json`)

```json
{
  "synthesis": {
    "browser": {
      "secrets": {
        "acme-password": {
          "source": "{env:SYNTHESIS_ACME_PASSWORD}",
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
| `secrets.<name>.source` | `{env:SYNTHESIS_X}`: a `SYNTHESIS_*` variable whose name contains KEY, TOKEN, SECRET, PASSWORD, CREDENTIAL, AUTH, PRIVATE, SESSION or COOKIE as a segment (so `Env::child_env` never hands it to commands the agent runs). Read from the process environment through `Env` at call time. `{file:/abs/path}`: absolute, a regular file, not a symlink, on unix mode must not allow group/other access (`chmod 600`), at most 4096 bytes, one trailing newline removed. Missing or empty = error naming the source, never the value. |
| `secrets.<name>.origins` | Non-empty list of exact origins (scheme + host + port; https, or http on loopback; no path). |
| `secrets.<name>.field_kind` | `password` (only into `type=password`), `username` or `totp` (never into `type=password`). Omitted: any text field. |
| `profiles.<name>.allow_domains` | Extra domains (and subdomains) the headless browser may navigate to on that profile. |
| `login_timeout_sec` | 10 to 3600, default 600. |

The `synthesis.browser` block is read from the user's own settings file only. In project, local, managed and `--settings` files it is ignored with the diagnostic `synthesis.browser is read from user settings only, ignored` (an untrusted project drops all `synthesis` keys). An invalid secret entry is skipped with a warning that names the entry, never a value.

## Rules of `browser_fill_secret`

1. The secret must exist in user settings; unknown names are refused.
2. The current page's origin is read from the page at call time (so after redirects) and must equal one of the secret's origins exactly.
3. The target element is measured in its own frame: its frame origin must equal the page origin (cross-origin iframes refused), it must be an `input` (text, password, email, search, tel, url, number) or `textarea`, visible, enabled and not read-only, and match `field_kind`.
4. The value is resolved BEFORE the person is asked, so a missing variable fails without a prompt.
5. First use per session of each (secret, origin) asks the person with the secret NAME and the origin (never the value). This is separate from the permission engine, so no mode (auto, bypass) and no allow rule skips it.
6. After focusing, the origin and focus are checked again immediately before `Input.insertText`; any change refuses with nothing typed.
7. The value is registered for masking before it is typed. From then on `redact_text` replaces it (raw and form-encoded) in every snapshot, console, network, tab and URL text the browser returns (`WebFetch` does not use this path). Exact values of all declared secrets that resolve at start are also added to the event sink, session store, task database and memory secret list, so they are masked in events, transcripts and the session database.

## Threat model

| Attack | Mitigation |
|---|---|
| A page (or prompt injection in it) asks the model to fill the password on its own, look-alike or redirected domain | Exact origin binding, re-read from the page at fill time (after redirects); the model only names a secret, so it cannot choose a destination the user did not bind |
| Attacker iframe of another origin inside the bound page | Frame origin of the element must equal the top origin; the AX tree used for refs does not expose cross-origin iframe fields in current Chrome (observed, see below) |
| Page navigates between the check and the keystrokes | Second origin and focus check right before `Input.insertText`; failure types nothing |
| Hidden or off-screen field used to harvest the value | Visible, enabled, editable required; password secrets only into `type=password` |
| Value reaches the model through a page echo, console or network log | `mask_secret` on every capture path; tested with a page that echoes the value into text, console and a request URL |
| Value reaches events, transcript, database | Declared secret values added to the event sink / store / task DB secret list at start; tool result never carries it; `redact_input` keeps the input to `{ref, secret name}` |
| Project or managed settings define or widen a secret | `synthesis.browser` read from user settings only; diagnostic otherwise |
| Env source that every command inherits | The variable name must be secret-shaped `SYNTHESIS_*`, which `Env::child_env` drops |
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

## Headless, CI, WSL, SSH

- **CI and containers:** no display; use B. Declare the secret in the runner's user settings, supply the value as `SYNTHESIS_<NAME>_PASSWORD` (a CI secret) or a `0600` file, and bind it to the staging origin.
- **SSH-only Linux:** same as CI. `ssh -X` / a forwarded `DISPLAY` makes A work (the window opens on your screen).
- **WSL:** works with WSLg (`WAYLAND_DISPLAY` is set). Without it, use B. Chrome must be installed inside WSL or pointed to with `SYNTHESIS_BROWSER_PATH`.
- **macOS / Windows desktop:** A works as is. On a remote-desktop-less Windows service session there is no window: use B.
- **Docker as root:** Chrome's sandbox may not start; the existing `no_sandbox` option applies to both modes (not recommended for untrusted sites).

## Not covered / unverified

- The headed window was not exercised on a real desktop in the implementation environment; the profile, cookie persistence, lock, kill and fill paths are tested against real headless Chrome, the headed-only part is the missing `--headless` flag and the skipped viewport emulation.
- Screenshots: a `username` or `totp` value typed into a visible field can appear in a screenshot (password fields render dots). PNG pixels are not masked. Prefer `field_kind: password` secrets for anything sensitive, or avoid `browser_screenshot` after filling.
- Page JavaScript can read what is typed into the field (the same as when a person types it). The origin binding limits this to the site the person bound the secret to; it does not protect against a compromised bound site.
- Chrome's own password manager may offer to save the password in the login window; it stores into the dedicated profile only. The profile directory holds session cookies (like any browser profile): protect it as you protect your home directory; `chmod 0700` is set on unix, nothing on Windows beyond the user profile ACLs.
- The domain record uses the last two labels (three for `co.uk` style suffixes), not the public-suffix list; an exotic suffix may be one label too wide or too narrow.
- A loopback login (`http://localhost:...`) opens the private-network gate for that window; for remote origins it stays closed.
- Cross-origin iframe refusal (rule 3) is covered by a unit test of the decision function; in real Chrome the cross-origin iframe field is not in the accessibility tree the refs come from, so an end-to-end refusal could not be provoked.
- One browser per session: a session whose headless browser already runs cannot also open the login window for the same profile (locked); end the session or use a new one.
- Windows ConPTY/`/dev/tty` equivalents (`CONIN$`) and Linux/Windows behaviour are untested here; CI covers the unit tests on the three OSes, the browser job covers real Chrome on Linux.
