#!/usr/bin/env bash
# Fails when package-lock.json brings in a high or critical npm advisory that
# the base branch doesn't already have. Existing advisories are listed in the
# job summary but don't block, so one old finding can't stall every PR.
#   audit-new.sh <base-ref>     (compare against a branch, on pull requests)
#   audit-new.sh                (report only)
set -euo pipefail

advisories() { # <dir> -> "severity<TAB>package<TAB>title<TAB>url" per high/critical advisory
  (cd "$1" && npm audit --package-lock-only --json || true) | jq -r '
    [.vulnerabilities[]?.via[]? | objects | select(.severity == "high" or .severity == "critical")]
    | unique_by(.url)[] | [.severity, .name, .title, .url] | @tsv' | sort
}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir "$work/head"
cp package.json package-lock.json "$work/head/"
advisories "$work/head" > "$work/head.tsv"

{
  echo "## npm audit (high and critical)"
  if [[ -s "$work/head.tsv" ]]; then
    echo "| Severity | Package | Advisory |"
    echo "| --- | --- | --- |"
    while IFS=$'\t' read -r sev pkg title url; do echo "| $sev | $pkg | [$title]($url) |"; done < "$work/head.tsv"
  else
    echo "No high or critical advisories."
  fi
} >> "${GITHUB_STEP_SUMMARY:-/dev/stdout}"

[[ $# -eq 0 ]] && exit 0

mkdir "$work/base"
git show "$1:package.json" > "$work/base/package.json"
git show "$1:package-lock.json" > "$work/base/package-lock.json"
advisories "$work/base" > "$work/base.tsv"

new=$(comm -23 <(cut -f4 "$work/head.tsv" | sort) <(cut -f4 "$work/base.tsv" | sort))
if [[ -n "$new" ]]; then
  echo "::error::This change adds high or critical npm advisories:"
  grep -F "$new" "$work/head.tsv"
  exit 1
fi
echo "No new high or critical advisories compared with $1."
