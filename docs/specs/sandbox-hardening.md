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

1. Take a slot (max 64 concurrent; more get 503 and `limit`).
2. Read the request head: 8 KiB cap, 10 s deadline (`bad_request` otherwise).
3. TCP listeners: `Proxy-Authorization: Basic synt:<random token>` compared in constant time (`auth`). Unix-socket listeners rely on the 0700 run directory.
4. Parse `host:port` (port required), normalise the host: lowercase, one trailing dot stripped, non-ASCII refused (callers send punycode), IP literals parsed.
5. Check the allowlist BEFORE any DNS lookup: an unlisted name is never resolved (`unlisted`).
6. A name is resolved through an injectable resolver; the connection is denied when ANY returned address is blocked (`private`; list in `synthesis-platform::net`: RFC1918, CGNAT, link-local including 169.254.169.254, loopback, ULA, NAT64, 6to4, Teredo, IPv4-mapped IPv6, documentation and reserved ranges). A hostname never reaches a private address. A private or loopback destination is reachable only through an allowlist entry that names that exact IP literal and port.
7. Dial the validated IP itself (never a second lookup, so a DNS answer cannot change between check and connect); failure is `resolve`.
8. Reply 200, copy both ways with a 5 minute idle timeout, count bytes.

Allowlist entries: `host` (port 443), `host:port`, `*.suffix` (matches on a label boundary, never the bare suffix, at least two labels), `ip:port` / `[v6]:port`.

Events (docs/specs/events.md): `egress.allowed {host, port, ip, up, down, ms}` when a connection closes; `egress.denied {host, port, reason}` with reason `unlisted | private | resolve | bad_request | auth | limit`, rate limited to 20 per 10 s. `Proxy-Authorization` is never logged.

### Per backend

- Linux bubblewrap: the fresh network namespace stays (`--unshare-net`). The host proxy listens on a unix socket in a 0700 run directory that is bound read-only into the sandbox. The helper `synthesis-sandbox-exec --egress-bridge <socket> 3128 -- <cmd>` runs inside the namespace as the parent of the command and forwards `127.0.0.1:3128` to the socket with plain threads (no `unsafe`). In `Proxied` mode EVERY bubblewrap run is also started behind the seccomp helper (`--no-vsock`: AF_VSOCK, io_uring and the x32 ABI denied), not only on WSL. Without a trusted helper the command is refused.
- macOS Seatbelt: `(deny network*)` then `(allow network-outbound (remote ip "localhost:<port>"))` with the rendered integer port only. The proxy binds `127.0.0.1:0` and requires the Basic credential.
- Landlock and Windows: `Proxied` reports `Guarantee::None`, so the command is refused (fail closed).

Environment of the command (`build_env`): every inherited proxy variable (any name ending in `_proxy`, either case) is removed, then `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` (and lowercase) are set to the proxy, `NO_PROXY=localhost,127.0.0.1`, `NODE_USE_ENV_PROXY=1`. A program that ignores the variables and connects directly simply fails: the sandbox has no other route, and no DNS (the proxy resolves).

### CLI and settings

`--network hosts` with repeatable `--allow-host H`. Settings key `synt.sandbox.network.allowedHosts` is read from USER and MANAGED settings only; a project copy is ignored with a diagnostic (a repository can only tighten, and an allowlist is not a tightening of `deny`). A managed list is final: `--allow-host` is then refused. Otherwise the list is the user's plus `--allow-host`. Deny stays the default. `hosts` needs `--sandbox enforce` and at least one host, and is refused for now when `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` is set in the environment (chaining through an upstream proxy is not built). The start notice lists the hosts and warns that an allowlist limits destinations, not exfiltration to an allowed host that accepts uploads.

### Limits (honest)

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
