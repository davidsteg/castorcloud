#!/usr/bin/env bash
# Reads VERSION (the one place the version lives) and prints tag=v<VERSION>
# to $GITHUB_OUTPUT. Pull requests must raise it above the base branch's;
# a push to main fails if v<VERSION> is already released; a release uses
# its own tag.
set -euo pipefail

semver='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
version=$(tr -d '[:space:]' < VERSION)
if ! [[ $version =~ $semver ]]; then
  echo "::error file=VERSION::VERSION must be MAJOR.MINOR.PATCH, got '${version}'."
  exit 1
fi
tag="v${version}"

# True when $1 is a higher version than $2.
newer() { [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -n1)" = "$1" ]; }

case "${EVENT}" in
  pull_request)
    base=$(gh api "repos/${GITHUB_REPOSITORY}/contents/VERSION?ref=${BASE_REF}" \
      -H 'Accept: application/vnd.github.raw' 2>/dev/null | tr -d '[:space:]' || true)
    if [ -n "$base" ] && ! newer "$version" "$base"; then
      echo "::error file=VERSION::Raise VERSION above ${base} (${BASE_REF}); every merge to main is released."
      exit 1
    fi
    ;;
  push)
    if gh api "repos/${GITHUB_REPOSITORY}/git/ref/tags/${tag}" >/dev/null 2>&1; then
      echo "::error file=VERSION::${tag} is already released; raise VERSION."
      exit 1
    fi
    ;;
  release)
    tag="${GITHUB_REF_NAME}"
    ;;
esac

echo "Version: ${tag}"
echo "tag=${tag}" >> "$GITHUB_OUTPUT"
