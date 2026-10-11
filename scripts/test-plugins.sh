#!/usr/bin/env bash
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
plugins=scripts/plugins.sh
failures=0

pass() {
  printf 'ok    %s\n' "$*"
}

fail() {
  printf 'FAIL  %s\n' "$*"
  failures=$((failures + 1))
}

accepts() {
  local tag=$1 expected=$2 output
  if output=$("$plugins" resolve-tag "$tag" 2>&1); then
    if [[ $output == "$expected" ]]; then
      pass "accepts $tag"
    else
      fail "resolve-tag $tag printed:"$'\n'"$output"$'\n'"expected:"$'\n'"$expected"
    fi
  else
    fail "rejects $tag: $output"
  fi
}

rejects() {
  local tag=$1 reason=$2 output
  if output=$("$plugins" resolve-tag "$tag" 2>&1); then
    fail "accepts $tag ($reason)"
  elif [[ $output != *error* ]]; then
    fail "rejects $tag without an error message: $output"
  else
    pass "rejects $tag ($reason)"
  fi
}

listing=$("$plugins" list)
while IFS=$'\t' read -r id version package dir smoke; do
  prerelease=false
  if [[ ${version%%+*} == *-* ]]; then
    prerelease=true
  fi
  accepts "$id-v$version" "$(printf 'id=%s\nversion=%s\npackage=%s\nprerelease=%s\nsmoke=%s' \
    "$id" "$version" "$package" "$prerelease" "$smoke")"
  rejects "$id-v999.0.0" "version differs from $dir/Cargo.toml"
  rejects "$id-v$version-rc.1" "prerelease of a released version"
  rejects "$id-v$version+build.1" "build metadata the crate does not carry"
  rejects "$id-$version" "no v before the version"
  rejects "$id-V$version" "capital V"
  rejects "v$version" "no plugin id"
  rejects "$id-v${version%.*}" "two version components"
  rejects "$id-v0$version" "leading zero"
  rejects "$id-v$version-" "empty prerelease"
  rejects "$id-v$version " "trailing space"
  rejects "x$id-v$version" "unknown id"
  rejects "$id" "bare id"
  rejects "" "empty tag"
done <<<"$listing"

if ((failures > 0)); then
  printf '%d failure(s)\n' "$failures"
  exit 1
fi
