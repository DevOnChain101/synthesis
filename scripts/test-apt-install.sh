#!/usr/bin/env bash
# Tests for apt-install.sh with fake sudo/apt-get/sleep on PATH.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
w=$(mktemp -d); trap 'rm -rf "$w"' EXIT
mkdir "$w/bin"
printf '#!/bin/sh\nexec "$@"\n' >"$w/bin/sudo"
printf '#!/bin/sh\nexit 0\n' >"$w/bin/sleep"
# Behaviour via env: STUB_FAILS (install failures before success), STUB_MSG (stderr text on failure).
cat >"$w/bin/apt-get" <<'AG'
#!/bin/sh
case " $* " in *" install "*) ;; *) exit 0 ;; esac
c=$(cat "$STUB_COUNT" 2>/dev/null || echo 0); echo $((c + 1)) >"$STUB_COUNT"
if [ "$c" -lt "${STUB_FAILS:-0}" ]; then echo "${STUB_MSG:-E: Temporary failure resolving}" >&2; exit 100; fi
AG
printf '#!/bin/sh\nexit 0\n' >"$w/bin/sed"
chmod +x "$w"/bin/*
fails=0
run() { # name expect(0|1) installs-expected [env...]
  local name=$1 want=$2 installs=$3; shift 3
  local rc=0; rm -f "$w/count"
  env PATH="$w/bin:$PATH" STUB_COUNT="$w/count" "$@" sh "$here/apt-install.sh" --no-install-recommends foo >"$w/out" 2>&1 || rc=$?
  local got; got=$(cat "$w/count" 2>/dev/null || echo 0)
  if [ "$rc" -eq "$want" ] && [ "$got" -eq "$installs" ]; then echo "ok   $name"; else echo "FAIL $name (rc=$rc installs=$got)"; cat "$w/out"; fails=1; fi
}
run "succeeds first try"          0 1 STUB_FAILS=0
run "retries twice then succeeds" 0 3 STUB_FAILS=2
run "gives up after 5 attempts"   1 5 STUB_FAILS=99
run "unknown package fails fast"  1 1 STUB_FAILS=99 "STUB_MSG=E: Unable to locate package foo"
exit "$fails"
