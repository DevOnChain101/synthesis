#!/bin/sh
# Retrying apt install for CI runners (transient mirror/DNS outages). Usage: apt-install.sh [--no-install-recommends] pkg...
set -eu
flags=""
if [ "${1:-}" = "--no-install-recommends" ]; then flags=$1; shift; fi
[ "$#" -gt 0 ] || { echo "usage: apt-install.sh [--no-install-recommends] pkg..." >&2; exit 2; }
log=$(mktemp); trap 'rm -f "$log"' EXIT
n=1
while :; do
  if [ "$n" -ge 3 ]; then
    sudo sed -i 's|http://azure\.archive\.ubuntu\.com/ubuntu/|http://archive.ubuntu.com/ubuntu/|g' /etc/apt/sources.list /etc/apt/sources.list.d/*.sources 2>/dev/null || true
  fi
  # update can exit 0 with failed indexes, so update+install form one attempt
  # shellcheck disable=SC2086
  if sudo apt-get -o Acquire::Retries=5 -o Acquire::http::Timeout=15 update >"$log" 2>&1 \
    && sudo apt-get -o Acquire::Retries=5 -o Acquire::http::Timeout=15 install -y $flags "$@" >>"$log" 2>&1; then
    cat "$log"; exit 0
  fi
  cat "$log"
  if grep -q 'Unable to locate package' "$log"; then echo "apt-install: unknown package, not retrying" >&2; exit 1; fi
  echo "apt-install: attempt $n/5 failed" >&2
  [ "$n" -lt 5 ] || exit 1
  sleep $((15 * n))
  n=$((n + 1))
done
