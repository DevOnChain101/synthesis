#!/usr/bin/env bash
# Compact digest of a CI step log. GitHub's log viewer shows only the last ~40 lines of a long step, so
# this prints the failures (and optionally the environment) as the FINAL lines of the step and mirrors
# them into $GITHUB_STEP_SUMMARY. Always exits 0: the step that produced the log decides pass/fail.
#   scripts/ci-digest.sh <log-file> [title]
# Env: DIGEST_ENV=1   add a one-line environment dump (OS, node, python, chrome, sandbox availability)
#      DIGEST_MAX=60  cap on detail lines
set -uo pipefail
log=${1:?usage: ci-digest.sh <log-file> [title]}
title=${2:-$log}
max=${DIGEST_MAX:-60}

env_dump() {
  local py chrome synt=target/debug/synt
  py=$(python3 --version 2>&1 || python --version 2>&1 || echo none)
  echo "env: os=$(uname -s) arch=$(uname -m) runner=${RUNNER_OS:-local} node=$(node --version 2>/dev/null || echo none) python=${py%%$'\n'*} git=$(git --version 2>/dev/null || echo none)"
  if [ -n "${SYNT_BROWSER_PATH:-}" ]; then
    # `chrome.exe --version` launches the browser on Windows and never returns: read the file version.
    if [ "${RUNNER_OS:-}" = Windows ]; then
      chrome=$(powershell -NoProfile -Command "(Get-Item -LiteralPath '$SYNT_BROWSER_PATH').VersionInfo.ProductVersion" 2>&1 | head -n 1)
    else
      chrome=$("$SYNT_BROWSER_PATH" --version 2>&1 | head -n 1)
    fi
    echo "chrome: $SYNT_BROWSER_PATH (${chrome:-no version output})"
  fi
  if [ "$(uname -s)" = Linux ]; then
    echo "linux: apparmor_restrict_unprivileged_userns=$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null || echo n/a)" \
      "max_user_namespaces=$(cat /proc/sys/user/max_user_namespaces 2>/dev/null || echo n/a)" \
      "lsm=$(cat /sys/kernel/security/lsm 2>/dev/null || echo n/a)" "bwrap=$(command -v bwrap || echo none)"
  fi
  if [ ! -x "$synt" ] && [ -x "$synt.exe" ]; then synt=$synt.exe; fi
  if [ -x "$synt" ]; then
    echo "synt doctor (sandbox):"
    "$synt" doctor --cwd "${RUNNER_TEMP:-${TMPDIR:-/tmp}}" 2>&1 | grep -E 'default mode: enforce|enforce_ready|^  selected |available:' | sed 's/^/  /'
  fi
}

# CARGO_TERM_COLOR=always puts escape codes inside "... FAILED"; work on a plain copy.
if [ -f "$log" ]; then
  plain=$(mktemp)
  trap 'rm -f "$plain"' EXIT
  LC_ALL=C sed $'s/\033\\[[0-9;]*m//g' "$log" >"$plain"
  log=$plain
fi

digest() {
  echo "== CI digest: $title =="
  if [ ! -f "$log" ]; then
    echo "no log file $log (the step did not get that far)"
  else
    if grep -q '^test result' "$log"; then
      grep -E '^test result' "$log" | awk '{p+=$4;f+=$6;i+=$8} END{printf "cargo test totals: passed=%d failed=%d ignored=%d\n",p,f,i}'
    fi
    local names
    names=$(grep -E '^test .* \.\.\. FAILED|^not ok |^✖ ' "$log" | sort -u | head -n 30)
    if [ -n "$names" ]; then
      echo "failing tests:"
      printf '%s\n' "$names" | sed 's/^/  /'
    fi
    local detail=""
    # Passing tests may panic on purpose, so only show details when something actually failed.
    if [ -n "$names" ] || grep -qE '^test result: FAILED|^error(\[|:)|^::error|FAILURE LOG' "$log"; then
      detail=$(grep -nE -A4 'panicked at|^error(\[|:)|^::error|^Error:|FAILURE LOG|launch failed' "$log" | head -n "$max")
    fi
    if [ -n "$detail" ]; then
      echo "first failure details (capped at $max lines):"
      printf '%s\n' "$detail"
    elif [ -z "$names" ]; then
      echo "no failures found in $title"
    fi
  fi
  if [ "${DIGEST_ENV:-}" = 1 ]; then env_dump; fi
}

out=$(digest)
printf '%s\n' "$out"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  # shellcheck disable=SC2016 # the backticks are literal markdown fences
  printf '```\n%s\n```\n' "$out" >>"$GITHUB_STEP_SUMMARY"
fi
exit 0
