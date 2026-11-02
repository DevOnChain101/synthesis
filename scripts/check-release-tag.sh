#!/usr/bin/env bash
# Release tag policy. Run by release.yml in `gate` and again in `release` right before publishing.
# Env: TAG (v1.2.3), REPO (owner/repo), GITHUB_SHA (commit the workflow runs on), GH_TOKEN (for `gh api`).
# Optional: MAINTAINERS (default .github/release-maintainers.txt; one tagger email per line, # comments).
# FAIL CLOSED: a missing or entry-less allowlist fails unless SYNT_ALLOW_ANY_VERIFIED_TAGGER is exactly "true"
# (repository variable, an explicit owner decision: then any GitHub-verified tagger passes).
# Checks: tag name vX.Y.Z[-pre]; commit reachable from main (API compare); annotated tag; refs/tags/$TAG^{commit} == $GITHUB_SHA; API tag object has verified signature, name == $TAG,
# points at $GITHUB_SHA, and is what the API ref resolves to; tagger email in the allowlist when one exists.
set -euo pipefail
: "${TAG:?}" "${REPO:?}" "${GITHUB_SHA:?}"
fail() { echo "::error::$*"; exit 1; }
[[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || fail "tag name '$TAG' is not vX.Y.Z"
# 'behind'/'identical' = the tagged commit is an ancestor of (or is) main. Works on shallow checkouts.
status=$(gh api "repos/$REPO/compare/main...$GITHUB_SHA" --jq .status) || fail "cannot compare $GITHUB_SHA with main"
case "$status" in behind|identical) ;; *) fail "commit $GITHUB_SHA is not on main (compare status: $status)" ;; esac

# Checkout may not persist credentials: pass the token as a header for this one command only (never written to .git/config).
auth=()
if [ -n "${GH_TOKEN:-}" ]; then
  auth=(-c "http.https://github.com/.extraheader=AUTHORIZATION: basic $(printf 'x-access-token:%s' "$GH_TOKEN" | base64 | tr -d '\n')")
fi
git ${auth[@]+"${auth[@]}"} fetch --force --no-tags origin "refs/tags/$TAG:refs/tags/$TAG"   # checkout can turn annotated tags into lightweight ones
[ "$(git cat-file -t "refs/tags/$TAG")" = tag ] || fail "$TAG is a lightweight tag; use git tag -s"
obj=$(git rev-parse "refs/tags/$TAG")
[ "$(git rev-parse "refs/tags/$TAG^{commit}")" = "$GITHUB_SHA" ] || fail "tag $TAG does not point at $GITHUB_SHA (tag moved?)"

IFS=$'\t' read -r verified name otype osha email < <(gh api "repos/$REPO/git/tags/$obj" \
  --jq '[.verification.verified, .tag, .object.type, .object.sha, (.tagger.email // "")] | @tsv')
[ "$verified" = true ] || fail "GitHub does not report a verified signature for tag $TAG (see docs/RELEASE.md)"
[ "$name" = "$TAG" ] || fail "tag object is named '$name', expected '$TAG'"
if [ "$otype" != commit ] || [ "$osha" != "$GITHUB_SHA" ]; then fail "API tag object targets $otype $osha, expected commit $GITHUB_SHA"; fi
[ "$(gh api "repos/$REPO/git/ref/tags/$TAG" --jq .object.sha)" = "$obj" ] || fail "API ref for $TAG does not resolve to tag object $obj"

list=${MAINTAINERS:-.github/release-maintainers.txt}
allowed=""
if [ -f "$list" ]; then allowed=$(sed 's/#.*//; s/[[:space:]]//g; /^$/d' "$list" | tr '[:upper:]' '[:lower:]'); fi
if [ -n "$allowed" ]; then
  printf '%s\n' "$allowed" | grep -Fxq -- "$(printf '%s' "$email" | tr '[:upper:]' '[:lower:]')" \
    || fail "tagger '$email' is not listed in $list"
  echo "tagger is a listed maintainer"
elif [ "${SYNT_ALLOW_ANY_VERIFIED_TAGGER:-}" = true ]; then
  echo "::warning::SYNT_ALLOW_ANY_VERIFIED_TAGGER=true: any GitHub-verified tagger is accepted (no maintainer allowlist)"
else
  fail "$list is missing or has no entries; add maintainer tagger emails, or set the repository variable SYNT_ALLOW_ANY_VERIFIED_TAGGER=true (docs/RELEASE.md)"
fi
echo "release tag policy ok: $TAG -> $GITHUB_SHA"
