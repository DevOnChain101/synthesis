# Sandbox hardening (design, owner-approved)

Status: egress control is implemented (this document, section 1). Sections 2 to 5 are approved design, built in later PRs. Code: `crates/synthesis-egress` (proxy), `crates/synthesis-platform/src/net.rs` (blocked addresses), `crates/synthesis-sandbox` (plumbing), `crates/synthesis-app/src/egress.rs` (start, host list). Background: `docs/specs/sandbox.md`, `docs/SECURITY.md`, `docs/UNATTENDED.md`.

## Owner decisions

1. `--unattended` MAY use `--network hosts` (strictly tighter than `allow`). It still refuses `--network allow` and `loopback`.
2. The whole of `/etc` stays read-only readable for now; the read allow-list (section 2) comes in a later PR.
3. `readScope: host` is forbidden under `--unattended`.
4. Default limits: rlimits on (core 0, fsize 4 GiB); memory 8 GiB and tasks 1024 when cgroups exist; disk quota 20 GiB with a 1 GiB floor. A missing cgroup does NOT block an unattended run (a notice says so).
5. A managed host list overrides the user's list (it is not merged).
6. Native Windows keeps refusing unless `--sandbox off` is explicit. WSL2 is the supported route for autonomous runs. An AppContainer backend comes later, behind an ADR-010 exception (a separate helper crate, because the workspace forbids `unsafe`); that needs owner approval.

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
- Landlock and Windows: `Proxied` reports `Guarantee::None`, so the command is refused (fail closed).

Environment of the command (`build_env`): every inherited proxy variable (any name ending in `_proxy`, either case) is removed, then `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` (and lowercase) are set to the proxy, `NO_PROXY=localhost,127.0.0.1`, `NODE_USE_ENV_PROXY=1`. A program that ignores the variables and connects directly simply fails: the sandbox has no other route, and no DNS (the proxy resolves).

### CLI and settings

`--network hosts` with repeatable `--allow-host H`. Settings key `synt.sandbox.network.allowedHosts` is read from USER and MANAGED settings only; a project copy is ignored with a diagnostic (a repository can only tighten, and an allowlist is not a tightening of `deny`). A managed list is final: `--allow-host` is then refused. Otherwise the list is the user's plus `--allow-host`. `--unattended` takes the list from user or managed settings ONLY and refuses `--allow-host`: a command line an agent or script builds must not choose where an unattended run may connect. Deny stays the default. `hosts` needs `--sandbox enforce` and at least one host, and is refused for now when `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` is set in the environment (chaining through an upstream proxy is not built). The start notice lists the hosts and warns that an allowlist limits destinations, not exfiltration to an allowed host that accepts uploads.

### Residual risk: host pathname unix sockets (Linux)

Under `Proxied`, bubblewrap keeps the network namespace, which hides abstract unix sockets and every TCP/UDP route except the bridge. It does NOT hide pathname unix sockets on the host filesystem: the root is bound read-only (`--ro-bind / /`), and a read-only bind does not stop `connect()`. Masked: `/tmp` (fresh tmpfs), `/run` and `/var/run` (tmpfs: docker, podman, containerd, snapd, systemd, `$XDG_RUNTIME_DIR`, gpg-agent in the runtime dir), and the standard policy's denied paths (`~/.ssh`, `~/.gnupg`, `~/.docker`, `~/.colima`, `~/.rd`, `~/.lima`, `~/.local/share/containers`, `~/.config/containers`, the directory of `$SSH_AUTH_SOCK`, tmux). Reachable: a pathname socket anywhere else (a service socket in `$HOME` or the workspace's parent, `/var/lib/...`). A reachable socket is a way around the proxy and the host list. Therefore the Linux guarantee for `Proxied` is capped at `Weak` until the read allow-list (section 2) hides the rest of the filesystem. `crates/synthesis-sandbox/tests/egress.rs` pins this: hidden under `/tmp` and in a denied dir, reachable elsewhere (that last assertion should start failing when the allow-list lands; then raise the guarantee). macOS Seatbelt denies `AF_UNIX` connects outright.

### Limits (honest)

- The proxy sees the `CONNECT` host name only. It does not inspect TLS: SNI and the `Host` header inside the tunnel can name a different site on the same allowed address (domain fronting through a CDN); allow only hosts you trust not to front for others.
- On macOS the proxy credential is in the command's environment (the proxy URL `http://synt:<token>@127.0.0.1:<port>`); the token is registered as a session secret so events, transcript and database mask it, but a program the command starts can read it. The Seatbelt rule says `localhost:<port>` because the grammar accepts only `*` or `localhost` as host (measured: `127.0.0.1` is a profile error), so `::1` on that port is also reachable; the proxy listens on 127.0.0.1 only.

- Allowed hosts can still receive anything (pastebins, object storage, an attacker's account on an allowed SaaS). Wildcards widen this.
- Only `CONNECT` (TLS or plain tunnels): the proxy does not see inside, so it cannot restrict URL paths or methods.
- Seatbelt is `Weak` and deprecated; the credential is in the command's environment; a local process of the same user could use the port with it.
- DNS rebinding is closed for the proxy (validated IP is dialled); WebFetch has its own resolver guard (same address list).
- Tests: pure and tokio tests on all OSes in `synthesis-egress`; real bubblewrap (`crates/synthesis-sandbox/tests/egress.rs`, `SYNT_REQUIRE_BWRAP=1` in CI) and real Seatbelt (`tests/seatbelt.rs`) end-to-end runs.

## 2. Linux read allow-list (not built)

Today bubblewrap mounts the whole filesystem read-only and hides only `deny_paths`. Target: bind only an allow-list (`/usr`, `/bin`, `/lib*`, a minimal `/etc` set, `/proc`, `/dev`, the workspace, `readable_roots`). Decision 2: until then all of `/etc` is readable. Decision 3: `readScope: host` (the opt-out that exposes the host filesystem) is forbidden under `--unattended`.

## 3. Resource limits (not built)

Defaults for sandboxed commands: `RLIMIT_CORE` 0, `RLIMIT_FSIZE` 4 GiB; with cgroups v2 memory 8 GiB and tasks 1024; a disk quota of 20 GiB for the workspace and temp dir with a 1 GiB floor. No cgroup means no memory/tasks limit and a start notice, never a refusal of an unattended run (decision 4). Applied by the helper before exec (`setrlimit` through a safe wrapper crate, no `unsafe` here).

## 4. MCP and browser sandbox (not built)

User-owned stdio MCP servers and the browser worker run outside the OS sandbox today. Plan: MCP stdio servers run through the same `wrap` with a policy of their own (network `hosts` with the server's declared hosts, private temp, no workspace write unless declared); the browser worker keeps Chrome's own sandbox and uses the egress proxy through the absolute-form seam (section 1) so its navigation policy and the proxy agree. Both stay opt-in until measured.

## 5. Windows

Decision 6. Native Windows has no backend that can confine commands; `Proxied` reports no guarantee there (the command is refused), and an unattended run does not start without an explicit `--sandbox off`. WSL2 (bubblewrap plus seccomp, drives hidden) is the supported route for autonomous runs and is what the desktop uses. A native AppContainer backend would need `unsafe` Win32 calls, which ADR-010 forbids in the workspace: it would live in a separate helper crate under an explicit exception that the owner must approve first.
