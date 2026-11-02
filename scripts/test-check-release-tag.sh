#!/usr/bin/env bash
# Tests for check-release-tag.sh: local origin repo + stub gh. Needs git; unsigned tags are fine (gh is stubbed).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
w=$(mktemp -d); trap 'rm -rf "$w"' EXIT
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
git init -q -b main "$w/origin"; cd "$w/origin"
git -c user.name=t -c user.email=t@example.com commit -q --allow-empty -m c
git -c user.name=t -c user.email=Dev@Example.com tag -a v1.0.0 -m v1.0.0
git clone -q "$w/origin" "$w/work"
sha=$(git rev-parse HEAD); obj=$(git rev-parse v1.0.0)
mkdir "$w/bin"
# Stub behaviour via env: STUB_VERIFIED, STUB_STATUS (compare), STUB_EMAIL.
cat >"$w/bin/gh" <<GH
#!/bin/sh
case "\$2" in
  repos/*/compare/*) echo "\${STUB_STATUS:-behind}" ;;
  repos/*/git/tags/*) printf '%s\tv1.0.0\tcommit\t$sha\t%s\n' "\${STUB_VERIFIED:-true}" "\${STUB_EMAIL:-dev@example.com}" ;;
  repos/*/git/ref/tags/*) echo $obj ;;
esac
GH
chmod +x "$w/bin/gh"

fails=0
run() { # name expect(0|1) [env...]
  local name=$1 want=$2; shift 2
  local rc=0
  (cd "$w/work" && env PATH="$w/bin:$PATH" TAG=v1.0.0 REPO=o/r GITHUB_SHA="$sha" "$@" bash "$here/check-release-tag.sh") >"$w/out" 2>&1 || rc=$?
  if { [ "$rc" -eq 0 ] && [ "$want" = 0 ]; } || { [ "$rc" -ne 0 ] && [ "$want" = 1 ]; }; then echo "ok   $name"; else echo "FAIL $name (rc=$rc)"; cat "$w/out"; fails=1; fi
}
printf '# only comments\n\n' >"$w/empty.txt"
printf 'x@example.com\n# c\n DEV@example.com \n' >"$w/listed.txt"
printf 'x@example.com\n' >"$w/other.txt"
run "empty list fails"        1 MAINTAINERS="$w/empty.txt"
run "missing file fails"      1 MAINTAINERS="$w/nope.txt"
run "flag true passes"        0 MAINTAINERS="$w/empty.txt" SYNT_ALLOW_ANY_VERIFIED_TAGGER=true
run "flag missing file passes" 0 MAINTAINERS="$w/nope.txt" SYNT_ALLOW_ANY_VERIFIED_TAGGER=true
run "flag not exactly true fails" 1 MAINTAINERS="$w/empty.txt" SYNT_ALLOW_ANY_VERIFIED_TAGGER=1
run "listed email passes"     0 MAINTAINERS="$w/listed.txt"
run "unlisted email fails"    1 MAINTAINERS="$w/other.txt"
run "unlisted beats flag"     1 MAINTAINERS="$w/other.txt" SYNT_ALLOW_ANY_VERIFIED_TAGGER=true
run "GH_TOKEN header fetch ok" 0 MAINTAINERS="$w/listed.txt" GH_TOKEN=dummy
run "unsigned tag (not verified) fails" 1 MAINTAINERS="$w/listed.txt" STUB_VERIFIED=false
run "unsigned beats flag"     1 MAINTAINERS="$w/empty.txt" SYNT_ALLOW_ANY_VERIFIED_TAGGER=true STUB_VERIFIED=false
run "wrong tagger fails"      1 MAINTAINERS="$w/listed.txt" STUB_EMAIL=evil@example.com
run "not on main fails"       1 MAINTAINERS="$w/listed.txt" STUB_STATUS=diverged
run "ahead of main fails"     1 MAINTAINERS="$w/listed.txt" STUB_STATUS=ahead
run "bad tag name fails"      1 MAINTAINERS="$w/listed.txt" TAG=release-1
run "bad tag name 2 fails"    1 MAINTAINERS="$w/listed.txt" TAG=v1.0
(cd "$w/origin" && git tag v1.0.1 && cd "$w/work" && git fetch -q --tags)
run "lightweight tag fails"   1 MAINTAINERS="$w/listed.txt" TAG=v1.0.1
exit $fails
