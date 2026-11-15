# Sandbox hardening (design, owner-approved)

Status: egress control (section 1), the Linux read allow-list (section 2), resource limits (section 3) and the sandbox for stdio MCP servers (section 4) are implemented; the browser part of section 4 is NOT built (what is true instead is stated there). Section 5 (a native Windows AppContainer backend, under an ADR-010 exception) is implemented but has only ever run on CI. Sections 2 and 3 were written on macOS: the Linux paths compile for Linux and are covered by unit tests plus the Linux CI jobs, nothing else. Code: `crates/synthesis-egress` (proxy), `crates/synthesis-platform/src/net.rs` (blocked addresses), `crates/synthesis-sandbox` (plumbing), `crates/synthesis-app/src/egress.rs` (start, host list). Background: `docs/specs/sandbox.md`, `docs/SECURITY.md`, `docs/UNATTENDED.md`.

## Owner decisions

1. `--unattended` MAY use `--network hosts` (strictly tighter than `allow`). It still refuses `--network allow` and `loopback`.
2. The whole of `/etc` stays read-only readable for now (a minimal `/etc` set is a later step).
3. `readScope: host` is forbidden under `--unattended`.
4. Default limits: rlimits on (core 0, fsize 4 GiB); memory 8 GiB and tasks 1024 when cgroups exist; disk quota 20 GiB with a 1 GiB floor. A missing cgroup does NOT block an unattended run (a notice says so).
5. A managed host list overrides the user's list (it is not merged).
6. Native Windows: WSL2 stays the supported route for autonomous runs. The owner approved (2026-11-15) an AppContainer backend as an exception to ADR-010 (`unsafe` Win32 calls in one separate crate, `synthesis-sandbox-win`; the core stays Rust). It is implemented (section 5), `Weak`, and never run on a real Windows machine. Without the helper `synthesis-sandbox-exec.exe` next to `synt.exe`, or where an AppContainer cannot be created, native Windows behaves as before: no backend, `enforce` refuses, `best-effort` and `off` run unconfined.

## 1. Egress

Problem: `--network allow` restricts nothing, and the former `AllowHosts` policy was rejected by every backend (no OS primitive filters by host name).

Design: a per-run egress proxy inside the `synt` process is the ONLY way out of the sandbox. The sandbox cannot reach any address except the proxy; the proxy enforces the host list. `NetworkPolicy::Proxied(EgressEndpoint)` replaces `AllowHosts`; the sandbox crate does plumbing only.

### The proxy (`synthesis-egress`)

HTTP `CONNECT` only (the seam for absolute-form forwarding, needed by the browser, is `head::read_head`). Per connection, in this order:

1. Take a connection slot in the accept loop, before any task is spawned (max 64 concurrent; more are refused with 503 and `limit`). A transient accept error (EMFILE) pauses 50 ms and retries; 100 in a row end the listener with a `notice`.
2. Read the request head: 8 KiB cap, 10 s deadline (`bad_request` otherwise).
3. TCP listeners: `Proxy-Authorization: Basic synt:<random token>` compared in constant time (`auth`). Unix-socket listeners rely on the 0700 run directory.
4. Parse `host:port` (port required), normalise the host: lowercase, one trailing dot stripped, non-ASCII refused (callers send punycode), IP literals parsed.
5. Check the allowlist BEFORE any DNS lookup: an unlisted name is never resolved (`unlisted`).
6. A name is resolved through an injectable resolver; the connection is denied when ANY returned address is blocked (`private`; list in `synthesis-platform::net`: RFC1918, CGNAT, link-local including 169.254.169.254, loopback, ULA, NAT64, 6to4, Teredo, IPv4-mapped IPv6, documentation and reserved ranges). A hostname never reaches a private address. A private or loopback destination is reachable only through an allowlist entry that names that exact IP literal and port.
7. Dial the validated IP itself (never a second lookup, so a DNS answer cannot change between check and connect); failure is `resolve`.
8. Reply 200, copy both ways with a 5 minute idle timeout and a 1 hour maximum lifetime, count bytes.

Allowlist entries: `host` (port 443), `host:port`, `*.suffix` (matches on a label boundary, never the bare suffix), `ip:port` / `[v6]:port`. Ports are ASCII digits only (`+443` is refused), `user@host`, `h:443:80` and `1.2.3.4.` are refused. A wildcard on a public suffix (`*.github.io`, `*.co.uk`, `*.herokuapp.com`, `*.pages.dev`, `*.s3.amazonaws.com`, ...) is refused: those domains belong to unrelated tenants. The list is a small built-in one (`PUBLIC_SUFFIXES` in `allow.rs`), not the full Public Suffix List: it only guards a typo, and a new dependency plus data that goes stale is not worth that; extend it when a platform is missing. Entries that name a private, loopback or link-local address are listed in a start notice.

Events (docs/specs/events.md): `egress.allowed {host, port, ip}` at the 200 response; `egress.closed {host, port, up, down, ms}` when the tunnel ends, also on shutdown (flushed from a drop guard); `egress.denied {host, port, reason}` with reason `unlisted | private | resolve | bad_request | auth | limit`. Denials are limited to 20 and allowed/closed to 200 per 10 s; what a window dropped is reported once as `egress.suppressed {count}`. `host` is ASCII-escaped (control, bidi and format characters cannot spoof a log line). `Proxy-Authorization` is never logged.

### Per backend

- Linux bubblewrap (guarantee `Weak`, see Residual risk below): the fresh network namespace stays (`--unshare-net`). The host proxy listens on a unix socket in a 0700 run directory that is bound read-only into the sandbox. The helper `synthesis-sandbox-exec --egress-bridge <socket> 3128 -- <cmd>` runs inside the namespace as the parent of the command and forwards `127.0.0.1:3128` to the socket with plain threads (no `unsafe`), at most 128 forwards at once, a 5 minute read timeout per side (the connect to the local socket cannot stall beyond its backlog). In `Proxied` mode EVERY bubblewrap run is also started behind the seccomp helper (`--no-vsock`: AF_VSOCK, io_uring and the x32 ABI denied), not only on WSL. Without a trusted helper the command is refused.
- macOS Seatbelt: `(deny network*)` then `(allow network-outbound (remote ip "localhost:<port>"))` with the rendered integer port only. The proxy binds `127.0.0.1:0` and requires the Basic credential.
- Landlock and the Windows AppContainer: `Proxied` reports `Guarantee::None`, so the command is refused (fail closed).

Environment of the command (`build_env`): every inherited proxy variable (any name ending in `_proxy`, either case) is removed, then `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` (and lowercase) are set to the proxy, `NO_PROXY=localhost,127.0.0.1`, `NODE_USE_ENV_PROXY=1`. A program that ignores the variables and connects directly simply fails: the sandbox has no other route, and no DNS (the proxy resolves).

### CLI and settings

`--network hosts` with repeatable `--allow-host H`. Settings key `synt.sandbox.network.allowedHosts` is read from USER and MANAGED settings only; a project copy is ignored with a diagnostic (a repository can only tighten, and an allowlist is not a tightening of `deny`). A managed list is final: `--allow-host` is then refused. Otherwise the list is the user's plus `--allow-host`. `--unattended` takes the list from user or managed settings ONLY and refuses `--allow-host`: a command line an agent or script builds must not choose where an unattended run may connect. Deny stays the default. `hosts` needs `--sandbox enforce` and at least one host, and is refused for now when `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` is set in the environment (chaining through an upstream proxy is not built). The start notice lists the hosts and warns that an allowlist limits destinations, not exfiltration to an allowed host that accepts uploads.

### Residual risk: host pathname unix sockets (Linux)

Superseded in part by section 2: the paragraph below describes the state BEFORE the read allow-list (the root bound read-only). Now only a pathname socket inside a directory the policy grants is reachable; the cap at `Weak` stays until that has been checked on a real host.

Under `Proxied`, bubblewrap keeps the network namespace, which hides abstract unix sockets and every TCP/UDP route except the bridge. It does NOT hide pathname unix sockets on the host filesystem: the root is bound read-only (`--ro-bind / /`), and a read-only bind does not stop `connect()`. Masked: `/tmp` (fresh tmpfs), `/run` and `/var/run` (tmpfs: docker, podman, containerd, snapd, systemd, `$XDG_RUNTIME_DIR`, gpg-agent in the runtime dir), and the standard policy's denied paths (`~/.ssh`, `~/.gnupg`, `~/.docker`, `~/.colima`, `~/.rd`, `~/.lima`, `~/.local/share/containers`, `~/.config/containers`, the directory of `$SSH_AUTH_SOCK`, tmux). Reachable: a pathname socket anywhere else (a service socket in `$HOME` or the workspace's parent, `/var/lib/...`). A reachable socket is a way around the proxy and the host list. Therefore the Linux guarantee for `Proxied` is capped at `Weak`. `crates/synthesis-sandbox/tests/egress.rs` pins the current state: hidden under `/tmp`, in a denied dir and in a dir that is not granted; reachable inside a granted dir (the workspace). macOS Seatbelt denies `AF_UNIX` connects outright.

### Limits (honest)

- The proxy sees the `CONNECT` host name only. It does not inspect TLS: SNI and the `Host` header inside the tunnel can name a different site on the same allowed address (domain fronting through a CDN); allow only hosts you trust not to front for others.
- On macOS the proxy credential is in the command's environment (the proxy URL `http://synt:<token>@127.0.0.1:<port>`); the token is registered as a session secret so events, transcript and database mask it, but a program the command starts can read it. The Seatbelt rule says `localhost:<port>` because the grammar accepts only `*` or `localhost` as host (measured: `127.0.0.1` is a profile error), so `::1` on that port is also reachable; the proxy listens on 127.0.0.1 only.

- Allowed hosts can still receive anything (pastebins, object storage, an attacker's account on an allowed SaaS). Wildcards widen this.
- Only `CONNECT` (TLS or plain tunnels): the proxy does not see inside, so it cannot restrict URL paths or methods.
- Seatbelt is `Weak` and deprecated; the credential is in the command's environment; a local process of the same user could use the port with it.
- DNS rebinding is closed for the proxy (validated IP is dialled); WebFetch has its own resolver guard (same address list).
- Tests: pure and tokio tests on all OSes in `synthesis-egress`; real bubblewrap (`crates/synthesis-sandbox/tests/egress.rs`, `SYNT_REQUIRE_BWRAP=1` in CI) and real Seatbelt (`tests/seatbelt.rs`) end-to-end runs.

### Residual: another session's `sessions/<id>/lock` (open item, not fixed)

A sandboxed command can open `~/.synt/sessions/<other id>/lock` read-only and `flock` it, which makes that session look dead-but-held to `synt stop`/liveness checks (denial of service only; no data). `sessions/` has to stay readable because the command may read the session's own spilled tool outputs, and `deny_paths` are subtrees, so the narrow rule (deny read of `sessions/*/lock`, any id) cannot be expressed by the current policy. Per backend: Seatbelt could add a `(regex #"^<home>/sessions/[^/]+/lock$")` deny (a new policy field, macOS only); bubblewrap mounts the filesystem read-only already (a read-only fd still locks) and can only hide lock files that exist at launch (`nonexistent deny_paths ... are not protected`); Landlock grants subtrees and cannot carve a child out. A deny that exists on one backend only would leave the same hole on the others, so it was left until the Linux read allow-list (section 2) existed. That is now true on bubblewrap (the synt home is not mounted unless granted), but exposing `sessions/` per session (own directory only) on every backend is not done.

## 2. Linux read allow-list (built, bubblewrap and WSL)

`BwrapBackend::args_on` no longer binds `/`. The root is an empty tmpfs; read-only binds (a symlink such as `/bin -> usr/bin` is recreated as a symlink) are made for:

- `SYSTEM_READ`: `/usr /bin /sbin /lib /lib32 /lib64 /etc /sys /opt` (those that exist), plus `/proc` and `/dev` as before (`--proc`, `--dev`), and fresh tmpfs on `/tmp`, `/run`, a real `/var/run`;
- the policy's `readable_roots` that exist (for the standard policy: toolchain dirs under `$HOME` such as `~/.cargo`, `~/.rustup`, `~/.nvm`, `~/.local`, PATH install prefixes, Linuxbrew, plus every `--sandbox-read`), except roots under `~/.config` other than `~/.config/git` (named exception: git's own configuration);
- `~/.gitconfig` (named exception: git needs it);
- the writable roots, the private tmp dir, then `readonly_paths`, then `deny_paths` (tmpfs or `/dev/null`) exactly as before.

Not mounted, hence default-denied: other users' homes and `/root`, `/home` outside the roots above, `~/.config` (except `~/.config/git`), `~/.ssh`, `~/.aws`, `~/.gnupg`, the synt home and its state (also still in `deny_paths`: a second layer), `/var`, `/mnt`, `/media`, `/srv`, `/boot` and any other top-level directory. Landlock was already an allow-list and is unchanged (its `~/.config` root is still granted: the standard policy offers it; narrowing it is not part of this step).

Known limits, unchanged or new:

- All of `/etc`, `/sys` and `/opt` remain readable (decision 2; `/sys` is read-only, a real-bubblewrap test checks that the cgroup tree there cannot be written), as does `~/.local` (pip/uv/pipx data, but also shell history of fish/atuin and other tools' data under `~/.local/share`), and `~/.gitconfig` (it can contain tokens inlined in URLs or helper settings).
- A tool that keeps files outside this list stops working until the user grants the directory with `--sandbox-read` (e.g. a toolchain under `/nix`, `/var/lib`, `/srv`).
- `readScope: host` (decision 3) is not built; nothing offers a way to bind the whole filesystem any more, so there is nothing to forbid yet.
- The sandboxed command still sees every pathname unix socket inside a directory the policy grants (see the residual risk in section 1).
- This is a mount-namespace allow-list, not a security boundary: filesystem stays `Weak`, `Proxied` stays `Weak`. Raising either needs a reviewer and a run on a real Linux host.

Tests: unit tests on the argument list (every OS, `linux.rs`), real bubblewrap tests (`crates/synthesis-sandbox/tests/egress.rs`, the `real` module in `linux.rs`), the red-team case `read-home-outside-allowlist` (work dir outside `/tmp`: bubblewrap puts a tmpfs on `/tmp`, so a file under it would be invisible for the wrong reason) with its positive control.

## 3. Resource limits (built; Linux only, plus the disk watchdog everywhere)

`SandboxPolicy::limits` (`ResourceLimits`), defaults from decision 4. Settings: `synt.sandbox.limits.{fileSizeMiB, memoryMiB, diskQuotaMiB, tasks, openFiles, processes, cpuSeconds}`, whole numbers, `0` = off, read from user and managed settings only (a project copy is ignored with a diagnostic). Interpreted in `synthesis-app` (`resource_limits`); `diskQuotaMiB` is raised to the 1 GiB floor.

| Limit | Default | Mechanism | Enforced where |
|---|---|---|---|
| core dumps | 0, always | `RLIMIT_CORE` | Linux, helper |
| file size | 4 GiB | `RLIMIT_FSIZE` | Linux, helper |
| open files, CPU seconds | off | `RLIMIT_NOFILE`, `RLIMIT_CPU` | Linux, helper |
| processes | off | `RLIMIT_NPROC`: it counts ALL of the user's processes, so a low value breaks `fork` for the sandbox whenever the user has that many; opt-in only | Linux, helper |
| memory 8 GiB, tasks 1024 | on | cgroup v2 `memory.max` (+ `memory.swap.max` 0), `pids.max` in a child `synt-sandbox-<parent pid>` of the helper's own cgroup, only when `memory` and `pids` are in that cgroup's `subtree_control` and it is writable (`status()` probes this by making and removing a child; a cgroup created but not filled or joined is removed, and the helper prints a notice) | Linux, best effort; else NOT enforced |
| disk growth 20 GiB (min 1 GiB) | on | watchdog thread in `synthesis-terminal` (`disk.rs`) | every backend, sandboxed commands only |

How it is applied: `synthesis-sandbox-exec --limits <spec> [--no-vsock] -- <program> ...` (`limits_arg` / `parse_limits_arg`, strict; a bad spec exits 126 and runs nothing), through `rustix::process::setrlimit` (no `unsafe` here). A value above the current hard limit is lowered to it. The helper runs in front of bubblewrap when it is found next to `synt` (always for Landlock, which needs it anyway); it is REQUIRED for `--network hosts` and on WSL. Without it a plain bubblewrap command runs without limits and `synt doctor` says `rlimits: NOT enforced`. rlimits and the cgroup are inherited by every descendant.

Honest limits:

- No cgroup means no memory/tasks cap. `synt run` prints a notice and `synt doctor` a `NOT enforced` line; an unattended run still starts (decision 4).
- The cgroup is per run (named after the helper's parent pid), not per command: concurrent commands share one memory and tasks budget, and the empty directory stays behind after the run (nothing is left to remove it after exec).
- A bubblewrap command whose helper lies under a writable root runs WITHOUT limits; a warning is printed once.
- The disk watchdog measures growth of the writable roots and tmp dir since the command started (apparent file sizes, net of deletions) every 5 s or four times a walk, whichever is longer; a fast writer overshoots the quota by what it writes in that time, and a walk that reaches 2 million entries is discarded (the quota is then NOT enforced; one warning). Deleted-but-open files are not seen. The kill is reported like an output-limit kill (`output_limit_exceeded`) with a line in the command's output. A command writing outside those directories (where the sandbox allows it, e.g. `/dev/shm`) is not counted.
- Seatbelt (macOS) gets none of the rlimits or the cgroup. The Windows AppContainer enforces `memory` and `tasks` through a Job Object (whole job) and no rlimits.
- None of this has run on a real Linux host except through CI; `limits.rs` checks the rlimits through `/proc/self/limits`, the cgroup path is only reported.

## 4. MCP and browser sandbox

### Stdio MCP servers (implemented)

`synthesis-mcp` spawns a stdio server through `McpOptions.confine` (`Confine`, supplied by `synthesis-app`: `mcp::confine`). The launch command (scrubbed env, `clear_env`) goes through the SAME `SandboxBackend::wrap` as the shell, with the run's command policy (deny paths, Linux read allow-list and toolchain roots, rlimits and cgroup limits from `synt.sandbox.limits.*`) changed in four ways: the workspace is READ-ONLY (it moves from `writable_roots` to `readable_roots`; `additionalDirectories` are not granted), the only writable places are the server's own private temp dir (`<artifacts>/mcp-tmp/<hash of name>`, 0700, `TMPDIR` forced) and the directories it declares, `readonly_paths` (`.git/hooks`, `.git/config`, `.synt`, `AGENTS.md`...) are kept only where a declared writable root contains or lies inside one, so a `writable` over the workspace or `.git` does not reopen them, and the network is the server's. The per-server egress proxies of `network: hosts` are started up front when the run is prepared (the launch itself happens inside the async connect, where a proxy cannot be started); a proxy that fails to start fails that server closed. MCP env scrubbing is unchanged: the command still starts from `Env::child_env` plus the server's own `env` map. A server whose launch cannot be confined is NOT started (fail closed; the status error says why and how to opt out), except in `--sandbox best-effort`, where it runs unconfined and a `notice` event (level warning) says so. A server that fails to start while sandboxed gets the knobs (`network`, `writable`, `sandbox`) named in its error.

Per-server settings, `synt.mcp.servers.<name>` in the USER or MANAGED `settings.json` (the managed entry replaces the user's):

| key | values | meaning |
|---|---|---|
| `network` | `none` (default), `hosts`, `full` | `none` = `NetworkPolicy::Deny`; `hosts` = a per-server egress proxy (section 1) limited to `hosts`; `full` = `Allow`, no restriction |
| `hosts` | `["host", "host:port", ...]` | allow-list for `hosts` (same syntax and rules as `--allow-host`); empty = refused |
| `writable` | `["/abs/dir", ...]` | extra writable directories, checked like `--sandbox-read` (no filesystem root, no `$HOME`, nothing that overlaps a denied path) |
| `sandbox` | `false` | explicit opt-out: this server runs outside the OS sandbox with your privileges; `synt run` prints a notice naming it. A MANAGED `synt.mcp.requireSandbox: true` forbids it for every server |

Relaxing (`network` other than `none`, `hosts`, `writable`, `sandbox: false`) is read from user and managed settings only. A trusted project `.synt/settings.json` can only tighten (`network: "none"` on a server the user relaxed). A server defined by a repository (scope `project`) ignores every user relaxation, even for the same name. An unknown `network` value means `none`. Default `none` is the safest setting; a server that needs the network (most hosted-API servers, `npx` downloading packages) fails to connect or start until the user sets `hosts` or `full`; servers that cache under `$HOME` (for example `mcp-remote`'s `~/.mcp-auth`) need `writable` or `sandbox: false`.

What is NOT enforced:
- Native Windows without a usable AppContainer (helper missing, AppContainer not creatable): no backend confines anything, so servers run as before (with `enforce` the run is refused earlier). `--sandbox off` also leaves them unconfined. Startup prints a `runs OUTSIDE the OS sandbox` notice per server in these cases. With the AppContainer, servers go through the same `wrap` as commands (section 5): `network: none` and `full` work, `hosts` is refused.
- HTTP, SSE and WebSocket servers are remote: nothing local to confine; their URL checks (`check_remote_url`) are unchanged.
- `network: hosts` needs bubblewrap (Linux, WSL2) or Seatbelt (macOS) and the helper on Linux; elsewhere the launch is refused. A host proxy configured in `HTTP(S)_PROXY` is not chained through.
- The weaknesses of the backend apply (Seatbelt and bubblewrap filesystem are Weak, section 2 and docs/SECURITY.md). A server cannot write the workspace, but it can read all of it, including `.env` files, and it is still a program that may be handed secrets by its own `env` map.
- The browser credential gate (`browser_login`, saved logins refused while a stdio server is configured, `synt.browser.allow_with_mcp`) is unchanged: it still counts every stdio server, whether sandboxed or not.
- Not verified on a real Linux or Windows host beyond CI. macOS Seatbelt was exercised locally by `crates/synthesis-app/tests/mcp_sandbox.rs`.

### Browser worker (NOT built; stated, not claimed)

Chrome runs outside the command sandbox and outside the egress proxy. It keeps Chrome's own sandbox, the `UrlPolicy` (private and metadata addresses, rebinding check), a scrubbed environment and its own temp or persistent profile directory. Putting it under `wrap` was judged unsafe to ship without a real machine to measure on:
- Chrome's own sandbox cannot nest inside Seatbelt (macOS: `sandbox_init` fails in the renderer) and needs unprivileged user namespaces or its own seccomp inside bubblewrap; the only way to make it start is `--no-sandbox`, which would trade a browser-grade sandbox for a weaker one around the process that renders hostile pages.
- Routing through the egress proxy needs absolute-form forwarding (section 1 seam) and proxy credentials, which Chrome does not accept on the command line (they would need CDP `Fetch.authRequired` handling), and a loopback allowance for its CDP port on the TCP transport.

Until that is built and measured, `synt doctor` prints `browser: NOT in the command sandbox and NOT routed through the egress proxy`, and docs/SECURITY.md says the same. Open work, in order: absolute-form forwarding plus `--proxy-server` with `Fetch.authRequired`; then a Linux Landlock or bubblewrap wrapper tested with Chrome's own sandbox kept on; a macOS wrapper is likely not possible without `--no-sandbox`.

## 5. Windows (AppContainer, ADR-010 exception)

Decision 6 and the exception. ADR-010 keeps the core in Rust and forbids `unsafe` in the workspace (`unsafe_code = "forbid"`). An AppContainer process can only be started through Win32 calls (`CreateAppContainerProfile`, `CreateProcessW` with `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`, DACL edits, Job Objects), and no maintained safe wrapper exists. On 2026-11-15 the owner approved the exception: Windows API calls through `windows-sys` in small audited `unsafe` blocks, in ONE crate, `crates/synthesis-sandbox-win` (its own `[lints]`: `unsafe_code = "deny"`, `imp.rs` is the only module that opts in with `#![allow(unsafe_code)]`, every block has a `SAFETY:` note; nothing else in the workspace may use `unsafe`). The Rust core is unchanged; no helper is written in another language.

### How it runs

`AppContainerBackend` (`crates/synthesis-sandbox/src/windows.rs`, safe code) is chosen on Windows when `synthesis-sandbox-exec.exe` sits next to `synt.exe` and a real probe (create and delete an AppContainer profile) works. `wrap` is pure: it only builds `synthesis-sandbox-exec --appcontainer <spec json>` (strict, versioned, `deny_unknown_fields`) from the policy; the same hook serves commands and stdio MCP servers. The helper (`synthesis_sandbox_win::run`) then, in order:

1. creates a throw-away AppContainer profile with a unique name (so a unique SID); no capabilities, unless the network is `Allow` (`internetClient`, `internetClientServer`, `privateNetworkClientServer`);
2. changes the DACLs (`SetNamedSecurityInfoW`, which also propagates to existing children), CUTS first: `readonly_paths` inside a writable root and `deny_paths` inside any granted root are cut off from the grants of their parents (the DACL is protected, inherited ACEs are kept as explicit ones, the container SID gets nothing, or only read for a read-only path). THEN the grants for the container SID: read and execute on `readable_roots` that exist; read, write, execute and delete the object (not change permissions or owner, not delete a directory's children through the directory) on `writable_roots` and the private temp dir. There are NO deny ACEs: on CI a deny ACE for the container SID (even `FILE_ALL_ACCESS`, set before or after the allow) did not stop a read, while the absence of an ACE did, so protection is done by cutting inheritance. The original DACL of every cut path is kept and put back at exit (unprotected, original ACEs), the explicit ACEs of the SID are removed from the grant roots;
3. creates a Job Object: kill on close, die on unhandled exception, `ActiveProcessLimit` (= `limits.tasks`, or 1 when `allow_exec` is false), `JobMemoryLimit` (= `limits.memory`, the whole job), and UI restrictions (desktop, display settings, exit windows, global atoms, user handles, clipboard, system parameters);
4. starts the program SUSPENDED inside the AppContainer with an explicit environment block (the launcher's environment plus the missing profile variables; the launcher's own environment is not modified), `CREATE_NO_WINDOW` (a hidden console of its own, so it cannot write input into the user's shell console; NOT probed on CI), with only the three standard handles inherited (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`), assigns it to the job (a failure kills it instead of resuming: fail closed), resumes, waits;
5. closes the job (kills leftovers), undoes every change in reverse order (removing its ACEs, restoring cut DACLs exactly, protected flag included), deletes the profile and the journal; the exit code is the program's. Exit 126: nothing was run (a policy path that does not exist, a bad spec, an API failure).

Everything inherits: descendants of the program are in the same container and job. `TEMP`/`TMP` (and `TMPDIR`) are set to the policy's private temp dir; the system may give the container its own temp folder instead (inside the profile, deleted with it); both are writable and neither is the user's real temp dir. The launcher also puts `LOCALAPPDATA`, `APPDATA`, `USERPROFILE`, `ProgramData`, `SystemDrive`, `SystemRoot` of the `synt` process in its own environment when the scrubbed environment lacks them: `CreateProcess` fails with `ERROR_ENVVAR_NOT_FOUND` otherwise (found on CI). The program inherits them: paths of the profile, not secrets.

### What is enforced (all of it `Weak`, see the last subsection)

- Files: an AppContainer reaches only what carries an ACE for its SID or for ALL APPLICATION PACKAGES (Windows, Program Files, ...). Your profile, other drives' user data and `%TEMP%` are unreachable unless granted, so the credential stores in `deny_paths` are unreachable by default and the explicit denies matter only inside a granted root.
- Network: no capabilities. Measured on CI: a connect to a service on the HOST's loopback fails, remote addresses fail, and a container can still talk to ITSELF over loopback (bind, listen and connect inside the same container work). The loopback exemption that would let it reach host services needs administrator rights and outlives the run, so `Loopback` and `Proxied` are not offered. `Deny` is `Weak`, `Allow` adds the three capabilities. `Loopback` and `Proxied` (`--network hosts`, MCP `network: hosts`) report `None` and `wrap` refuses: the egress proxy cannot be reached from the container.
- Processes: Job Object as above. Windows exit codes pass through (32 bits).

### What is NOT enforced or not proven

- Never run on a real Windows machine by the authors; the evidence is the CI `windows-latest` job running `crates/synthesis-sandbox/tests/appcontainer.rs` (positive controls and denials), which is why both guarantees stay `Weak`. Raising one needs a reviewer and a run on a real host.
- ACL edits are per run and journaled, never meant to be permanent, but a launcher killed outright (a timeout kill is `TerminateProcess`) cannot undo them: cut paths stay cut (with explicit copies of their ACEs), ACEs of a dead SID stay on the grant roots and the profile leaks. Each run writes a journal (profile name, SID, per path the original DACL as SDDL including the protected flag) under `$SYNT_HOME/sandbox-journal` BEFORE changing the path; the NEXT sandboxed command, under a setup mutex, finds journals whose launcher's run mutex is free (the launcher is dead), restores the DACLs, removes the dead SID's ACEs and deletes the profile. Until then the directories keep the modified ACLs: this is not harmless. Ctrl-C and console close terminate the job so the normal cleanup runs. The journal directory is refused as a grant (and so is anything containing it).
- Granting a large tree is slow: `SetNamedSecurityInfoW` walks every existing child, on grant and again on revoke (a toolchain directory with 100k files costs seconds).
- A protected path that does not exist at start (`.synt/` not created yet) cannot be cut and can be created by the command; the same limit as bubblewrap's nonexistent `deny_paths`. The children of `.git` cannot be renamed away one by one and `.git` itself cannot be renamed or deleted (tested); a guard only exists for paths that exist at start. A rename of a cut path from OUTSIDE the container while it runs makes the exit cleanup fail to find it (a message on stderr, the original DACL is then not restored).
- ALL APPLICATION PACKAGES stays enabled (an LPAC opt-out breaks most programs), so everything every container may read is readable, including Windows and Program Files and anything else carrying that ACE or `Everyone`. NTFS junctions and symlinks are followed by the file system after the path was granted; paths are canonicalised before granting.
- Only the three standard handles are inherited, but a console or pseudo-console handle is passed as it is; ConPTY runs were not tested.
- Arguments must be UTF-8; `.bat`/`.cmd` programs must be started through `cmd.exe`; a UNC or device path, or a drive root, is refused as a grant.
- The helper is trusted by location only: it must be a regular file next to `synt.exe` and must not lie under a writable root or the temp dir. Windows ACLs on its directory are not checked (unlike Linux ownership and mode).
- The release workflow does not yet ship `synthesis-sandbox-exec.exe` on Windows, so a released build keeps the old behaviour (no backend) until it does.
- Concurrency: the cuts are per path and cannot be shared. A second run that needs a path the first one has cut (or one inside or above it) is REFUSED (exit 126, "another sandboxed command in this workspace ...") before it changes anything; tested (the first run keeps failing to write into `.git/hooks`, ACLs identical afterwards). Runs that need no common cut path run concurrently.
- Grants of the user profile, Windows, Program Files (3 variables), ProgramData, the Synthesis home, the helper's directory, or any ancestor of these, are refused. Grants below them are fine.
- Hard links and junctions: a junction from the workspace to an outside directory gave no access and no ACL change (tested). A hard link shares its file's DACL, so the workspace grant reaches the linked outside file during the run; the CI test asserts only the restore afterwards; whether the linked file is readable during the run is NOT established (the CI log hides passing tests' output).
- rlimits other than memory and the process count, the disk quota being the existing watchdog, and `--network loopback|hosts` are not available. 

### Modes and doctor

`--sandbox enforce` uses the backend and refuses commands if it is missing or cannot apply the policy. `best-effort` does NOT use it (`SandboxBackend::enforce_only`): because it edits ACLs of the user's real directories and has only CI evidence, best-effort keeps the unconfined behaviour and its notice now says the AppContainer is available under `--sandbox enforce`; the same holds for stdio MCP servers. `off` is unchanged. When `--sandbox` is not given the default on Windows stays `best-effort` (owner decision D10), i.e. unconfined. A user/managed setting that would enable it in best-effort (for example `synt.sandbox.windows.appContainer`) is NOT built. `--unattended` on native Windows still starts only with `enforce` (which now can succeed) or an explicit `off`. The orchestrator's refusal of writable outcome attempts on native Windows stays: it was decided on the absence of a verified backend, and a `Weak` backend never run on a real machine is not that. `synt doctor` prints `appcontainer available: true|false` with the reason (helper missing, profile creation failed), the per-mode network support, and the notes of the selected backend (what is and is not enforced); `enforce_ready` is `yes` only when the standard policy wraps.

### Tests

Pure, every OS: path simplification (`\\?\`, UNC, device paths), case folding, component-boundary containment, argument quoting, grant planning and ordering (Windows paths, `plan` module), wrap shape and refusals, strict spec decoding. Real, Windows CI only (`tests/appcontainer.rs`): container starts; workspace writable (control); a secret outside the grants unreadable while the same read works unsandboxed (control); write outside denied and allowed once granted (control); read root readable not writable; denied and read-only paths inside a writable root (reads, writes, delete, create, rename of a guarded child), each with a control; network: a connect to a listener on the host's loopback fails while it works unsandboxed (control), the program did start in the container, and a remote address is not reached; `Loopback`/`Proxied` refused; `TEMP` is not the user's real temp dir and the policy's temp dir is writable; grandchildren confined (a real child process, control: it writes where granted); process limit with `allow_exec = false` (control: a child runs by default); ACLs restored (the full `icacls /T` of the tree is identical before and after, cuts included, and no container SID is left); fail closed (missing root, bad spec). NOT covered by any test: kill on close after the helper is killed, the memory limit, handle non-inheritance, ConPTY.
