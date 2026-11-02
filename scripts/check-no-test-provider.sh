#!/usr/bin/env bash
# Fails if the given synt binary was built with the `test-provider` feature (scripted fake model).
# Note: `--help` text and the "needs a build with the test-provider feature" error exist in release
# builds too, so we look for code that only exists in fake.rs and for behaviour.
set -euo pipefail
bin=${1:?usage: check-no-test-provider.sh <path-to-synthesis>}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

if grep -aq "fake script" "$bin"; then
  echo "FAIL: $bin contains fake.rs strings (test-provider feature compiled in)" >&2
  exit 1
fi

echo '[]' >"$tmp/s.json"
set +e
out=$("$bin" run --cwd "$tmp" --provider "fake:$tmp/s.json" --prompt x 2>&1)
code=$?
set -e
if [ "$code" -eq 0 ] || printf '%s' "$out" | grep -q "fake script"; then
  echo "FAIL: --provider fake: is accepted by $bin (exit $code)" >&2
  printf '%s\n' "$out" >&2
  exit 1
fi
echo "OK: $bin has no test-provider (exit $code: $(printf '%s' "$out" | head -n1))"
