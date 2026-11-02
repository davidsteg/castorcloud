#!/usr/bin/env bash
# Deletes branches whose work is finished, so the repository keeps only main
# and branches still in use (docs/ci.md, "Branch cleanup").
#
# A branch is deleted only when all of these hold:
#   - it is not the default branch and not protected;
#   - no open pull request uses it, as head or as base;
#   - a closed (merged or abandoned) pull request from this repository had it
#     as head, and the branch still points at that pull request's last head
#     commit. Its commits therefore stay reachable from refs/pull/<n>/head,
#     and "Restore branch" on the pull request brings it back. A branch that
#     got new commits after its pull request closed is kept.
#   - or, with or without a pull request: every commit on it is already on
#     the default branch, so deleting it loses nothing.
# Any other branch (unmerged work without a pull request) is only listed.
#
# DRY_RUN=true lists what would be deleted without deleting anything.
set -euo pipefail

repo="${GITHUB_REPOSITORY:?}"
dry_run="${DRY_RUN:-false}"

default=$(gh api "repos/${repo}" --jq '.default_branch')
branches=$(gh api --paginate "repos/${repo}/branches?per_page=100" \
  --jq '.[] | [.name, .commit.sha, (.protected|tostring)] | @tsv')
pulls=$(gh api --paginate "repos/${repo}/pulls?state=all&per_page=100" \
  --jq ".[] | select(.head.repo.full_name == \"${repo}\") | [.state, .head.ref, .head.sha, .base.ref] | @tsv")

# Branches an open pull request uses, and branch<TAB>sha of closed heads.
in_use=$(awk -F'\t' '$1 == "open" { print $2; print $4 }' <<<"$pulls" | sort -u)
closed=$(awk -F'\t' '$1 == "closed" { print $2 "\t" $3 }' <<<"$pulls" | sort -u)

# True when commit $1 is already on the default branch.
contained() {
  local status
  status=$(gh api "repos/${repo}/compare/${default}...${1}" --jq '.status' 2>/dev/null) || return 1
  [ "$status" = "identical" ] || [ "$status" = "behind" ]
}

deleted=0 kept=0 failed=0
summary() { [ -n "${GITHUB_STEP_SUMMARY:-}" ] && echo "$1" >> "$GITHUB_STEP_SUMMARY" || true; }
summary "| Branch | Action |"
summary "| --- | --- |"

while IFS=$'\t' read -r name sha protected; do
  [ -n "$name" ] || continue
  reason=""
  if [ "$name" = "$default" ]; then
    continue
  elif [ "$protected" = "true" ]; then
    reason="protected"
  elif grep -qxF -- "$name" <<<"$in_use"; then
    reason="open pull request"
  elif grep -qxF -- "${name}"$'\t'"${sha}" <<<"$closed" || contained "$sha"; then
    if [ "$dry_run" = "true" ]; then
      echo "would delete ${name}"; summary "| \`${name}\` | would delete |"
      deleted=$((deleted + 1)); continue
    fi
    # Percent-encode the name for the URL, keeping '/' as the refs API wants.
    ref=$(jq -rn --arg n "$name" '$n | @uri | gsub("%2F"; "/")')
    if gh api -X DELETE "repos/${repo}/git/refs/heads/${ref}" --silent; then
      echo "deleted ${name}"; summary "| \`${name}\` | deleted |"
      deleted=$((deleted + 1))
    else
      echo "::warning::could not delete ${name}"; summary "| \`${name}\` | delete failed |"
      failed=$((failed + 1))
    fi
    continue
  elif cut -f1 <<<"$closed" | grep -qxF -- "$name"; then
    reason="new commits since its pull request closed"
  else
    reason="unmerged commits, no pull request"
  fi
  echo "kept ${name} (${reason})"; summary "| \`${name}\` | kept: ${reason} |"
  kept=$((kept + 1))
done <<<"$branches"

echo "deleted=${deleted} kept=${kept} failed=${failed} dry_run=${dry_run}"
[ "$failed" -eq 0 ]
