# Sandbox hardening (design, owner-approved)

Status: egress control (section 1), the Linux read allow-list (section 2) and resource limits (section 3) are implemented. Sections 4 and 5 are approved design, built in later PRs. Sections 2 and 3 were written on macOS: the Linux paths compile for Linux and are covered by unit tests plus the Linux CI jobs, nothing else. Code: `crates/synthesis-egress` (proxy), `crates/synthesis-platform/src/net.rs` (blocked addresses), `crates/synthesis-sandbox` (plumbing), `crates/synthesis-app/src/egress.rs` (start, host list). Background: `docs/specs/sandbox.md`, `docs/SECURITY.md`, `docs/UNATTENDED.md`.

## Owner decisions

1. `--unattended` MAY use `--network hosts` (strictly tighter than `allow`). It still refuses `--network allow` and `loopback`.
2. The whole of `/etc` stays read-only readable for now (a minimal `/etc` set is a later step).
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

- All of `/etc`, `/sys` and `/opt` remain readable (decision 2), as does `~/.local` (pip/uv/pipx data, but also other tools' data under `~/.local/share`).
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
| memory 8 GiB, tasks 1024 | on | cgroup v2 `memory.max` (+ `memory.swap.max` 0), `pids.max` in a child `synt-sandbox-<parent pid>` of the helper's own cgroup, only when `memory` and `pids` are in that cgroup's `subtree_control` and it is writable | Linux, best effort; else NOT enforced |
| disk growth 20 GiB (min 1 GiB) | on | watchdog thread in `synthesis-terminal` (`disk.rs`) | every backend, sandboxed commands only |

How it is applied: `synthesis-sandbox-exec --limits <spec> [--no-vsock] -- <program> ...` (`limits_arg` / `parse_limits_arg`, strict; a bad spec exits 126 and runs nothing), through `rustix::process::setrlimit` (no `unsafe` here). A value above the current hard limit is lowered to it. The helper runs in front of bubblewrap when it is found next to `synt` (always for Landlock, which needs it anyway); it is REQUIRED for `--network hosts` and on WSL. Without it a plain bubblewrap command runs without limits and `synt doctor` says `rlimits: NOT enforced`. rlimits and the cgroup are inherited by every descendant.

Honest limits:

- No cgroup means no memory/tasks cap. `synt run` prints a notice and `synt doctor` a `NOT enforced` line; an unattended run still starts (decision 4). A container or a login shell without systemd delegation typically has no cgroup.
- The cgroup is per run (named after the helper's parent pid), not per command: concurrent commands share one memory and tasks budget, and the empty directory stays behind after the run (nothing is left to remove it after exec).
- The disk watchdog measures growth of the writable roots and tmp dir since the command started (apparent file sizes, net of deletions) every 5 s or four times a walk, whichever is longer; a fast writer overshoots the quota by what it writes in that time, and a walk stops after 2 million entries (a larger tree is under-counted). The kill is reported like an output-limit kill (`output_limit_exceeded`) with a line in the command's output. A command writing outside those directories (where the sandbox allows it, e.g. `/dev/shm`) is not counted.
- Seatbelt (macOS) and Windows get none of the rlimits or the cgroup.
- None of this has run on a real Linux host except through CI; `limits.rs` checks the rlimits through `/proc/self/limits`, the cgroup path is only reported.

## 4. MCP and browser sandbox (not built)

User-owned stdio MCP servers and the browser worker run outside the OS sandbox today. Plan: MCP stdio servers run through the same `wrap` with a policy of their own (network `hosts` with the server's declared hosts, private temp, no workspace write unless declared); the browser worker keeps Chrome's own sandbox and uses the egress proxy through the absolute-form seam (section 1) so its navigation policy and the proxy agree. Both stay opt-in until measured.

## 5. Windows

Decision 6. Native Windows has no backend that can confine commands; `Proxied` reports no guarantee there (the command is refused), and an unattended run does not start without an explicit `--sandbox off`. WSL2 (bubblewrap plus seccomp, drives hidden) is the supported route for autonomous runs and is what the desktop uses. A native AppContainer backend would need `unsafe` Win32 calls, which ADR-010 forbids in the workspace: it would live in a separate helper crate under an explicit exception that the owner must approve first.
