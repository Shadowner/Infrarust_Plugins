#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
dist=dist
wasm_target=wasm32-wasip2
component_header=0061736d0d000100
id_pattern='^[a-z0-9][a-z0-9_-]{0,63}$'
semver_pattern='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$'
toolchain=
plugins=
target_dir=
infrarust_url=
infrarust_ref=
infrarust_repository=

usage() {
  cat <<'EOF'
Usage: scripts/plugins.sh <command> [arguments]

  config               Infrarust repository, ref and toolchain from Cargo.toml, as key=value lines
  list                 the plugins: id, version, package, crate directory, smoke workspace
  matrix               the plugins and the plugins with a smoke workspace, as key=JSON lines
  check                check-ids, fmt, clippy and test
  check-ids [id...]    each plugin's metadata id is the id of its #[plugin(id = "...")]
  fmt                  cargo fmt --check on the workspace and on every smoke workspace
  clippy               clippy on the workspace, native and wasm32-wasip2, warnings denied
  test [id...]         native tests of the workspace, or of the given plugins
  build [id...]        build the components into dist/<id>.wasm and dist/<id>.wasm.sha256
  smoke [--prebuilt] [id...]
                       build, then run each smoke workspace against dist/<id>.wasm;
                       --prebuilt uses dist/<id>.wasm as it is
  resolve-tag <tag>    the plugin a <id>-v<version> tag releases, as key=value lines
  notes <id>           release notes for dist/<id>.wasm
  lock                 update Cargo.lock of the workspace and of every smoke workspace, after a
                       version bump or an Infrarust ref change

PLUGINS_TOOLCHAIN overrides the toolchain named in Cargo.toml.
EOF
}

report() {
  if [[ ${GITHUB_ACTIONS:-} == true ]]; then
    printf '::error::%s\n' "$*" >&2
  else
    printf 'error: %s\n' "$*" >&2
  fi
}

die() {
  report "$@"
  exit 1
}

load_config() {
  if [[ -n $toolchain ]]; then
    return
  fi
  local values
  values=$(python3 -I - Cargo.toml <<'EOF'
import sys
import tomllib

with open(sys.argv[1], "rb") as manifest:
    table = tomllib.load(manifest).get("workspace", {}).get("metadata", {}).get("infrarust", {})
for key in ("repository", "ref", "toolchain"):
    value = table.get(key)
    if not isinstance(value, str) or not value or any(c.isspace() for c in value):
        sys.exit(f"[workspace.metadata.infrarust] {key} must be a non-empty string without spaces")
    print(value)
EOF
  ) || die "cannot read [workspace.metadata.infrarust] from Cargo.toml"
  { read -r infrarust_url; read -r infrarust_ref; read -r toolchain; } <<<"$values"
  [[ $infrarust_url =~ ^https://github\.com/([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)$ ]] \
    || die "[workspace.metadata.infrarust] repository must be https://github.com/<owner>/<name>, got $infrarust_url"
  infrarust_repository=${BASH_REMATCH[1]%.git}
  [[ $infrarust_ref =~ ^[A-Za-z0-9._/-]+$ ]] || die "[workspace.metadata.infrarust] ref has unexpected characters: $infrarust_ref"
  toolchain=${PLUGINS_TOOLCHAIN:-$toolchain}
  [[ $toolchain =~ ^[A-Za-z0-9._-]+$ ]] || die "toolchain has unexpected characters: $toolchain"
}

cargo_tc() {
  cargo "+$toolchain" "$@"
}

load_plugins() {
  if [[ -n $plugins ]]; then
    return
  fi
  load_config
  local metadata
  metadata=$(cargo_tc metadata --format-version 1 --no-deps --manifest-path Cargo.toml) || die "cargo metadata failed"
  target_dir=$(jq -r '.target_directory' <<<"$metadata")
  plugins=$(jq -c '
    .workspace_root as $root
    | [.packages[]
       | select(.metadata["infrarust-plugin"] != null)
       | .metadata["infrarust-plugin"] as $meta
       | (.manifest_path | rtrimstr("/Cargo.toml") | ltrimstr($root + "/")) as $dir
       | {
           id: $meta.id,
           version: .version,
           package: .name,
           description: (.description // ""),
           repository: (.repository // ""),
           dir: $dir,
           lib: ([.targets[] | select(.kind | index("cdylib")) | .name | gsub("-"; "_")] | first),
           permissions: ($meta.permissions // []),
           smoke: (if ($meta.smoke | type) == "object" then $meta.smoke.path else $meta.smoke end),
           component_env: (if ($meta.smoke | type) == "object" then $meta.smoke["component-env"] else null end),
           sdk: ([.dependencies[] | select(.name == "infrarust-plugin-sdk") | .path] | first)
         }
       | .smoke |= (if type == "string" then $dir + "/" + . else . end)]
    | sort_by(.id)' <<<"$metadata")
  validate_plugins
}

validate_plugins() {
  [[ $(jq 'length' <<<"$plugins") -gt 0 ]] \
    || die "no workspace member has a [package.metadata.infrarust-plugin] table"
  local problems
  problems=$(jq -r --arg id_pattern "$id_pattern" '
    (.[] | select(if (.id | type) == "string" then (.id | test($id_pattern) | not) else true end)
       | "\(.package): metadata id must be 1 to 64 of a-z, 0-9, - and _, starting with a letter or a digit"),
    (map(select((.id | type) == "string")) | group_by(.id)[] | select(length > 1)
       | "\(map(.package) | join(", ")) share the id \(.[0].id)"),
    (.[] | select(.lib == null) | "\(.package): no cdylib target"),
    (.[] | select(if (.permissions | type) == "array" then any(.permissions[]; type != "string") else true end)
       | "\(.package): permissions must be a list of strings"),
    (.[] | select((.smoke == null and .component_env != null) or (.smoke != null and ((.smoke | type) != "string" or (.smoke | test("^[A-Za-z0-9_][A-Za-z0-9_./-]*$") | not) or (.smoke | test("(^|/)\\.\\.(/|$)")))))
       | "\(.package): smoke must be {path = \"<directory inside the crate>\", component-env = \"<VARIABLE>\"}"),
    (.[] | select(.component_env != null and ((.component_env | type) != "string" or (.component_env | test("^[A-Z_][A-Z0-9_]*$") | not)))
       | "\(.package): smoke component-env must be an environment variable name")
  ' <<<"$plugins")
  if [[ -n $problems ]]; then
    while IFS= read -r problem; do report "$problem"; done <<<"$problems"
    exit 1
  fi
  local smoke
  while IFS= read -r smoke; do
    if [[ ! -f $smoke/Cargo.toml ]]; then
      die "smoke workspace $smoke has no Cargo.toml"
    fi
  done < <(jq -r '.[] | .smoke // empty' <<<"$plugins")
}

field() {
  jq -r --arg id "$1" --arg key "$2" '.[] | select(.id == $id) | .[$key] // empty' <<<"$plugins"
}

all_ids() {
  jq -r '.[].id' <<<"$plugins"
}

selected_ids() {
  if (($# == 0)); then
    all_ids
    return
  fi
  local id
  for id in "$@"; do
    jq -e --arg id "$id" 'any(.[]; .id == $id)' <<<"$plugins" >/dev/null \
      || die "no plugin has the id $id; known ids: $(all_ids | paste -sd ' ')"
    printf '%s\n' "$id"
  done
}

checksum() {
  if command -v sha256sum >/dev/null; then
    sha256sum -- "$1"
  else
    shasum -a 256 -- "$1"
  fi
}

declared_ids() {
  find "$1" -type f -name '*.rs' -print0 | sort -z | xargs -0r perl -0777 -ne '
    while (/#\[\s*(?:\w+\s*::\s*)*plugin\s*\((.*?)\)\s*\]\s*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?impl\b/sg) {
      my $args = $1;
      print $args =~ /(?:^|[\s,])id\s*=\s*"([^"]*)"/ ? "$1\n" : "-\n";
    }'
}

cmd_config() {
  load_config
  printf 'infrarust-repository=%s\ninfrarust-ref=%s\ntoolchain=%s\n' \
    "$infrarust_repository" "$infrarust_ref" "$toolchain"
}

cmd_list() {
  load_plugins
  jq -r '.[] | [.id, .version, .package, .dir, (.smoke // "")] | @tsv' <<<"$plugins"
}

cmd_matrix() {
  load_plugins
  printf 'plugins=%s\n' "$(jq -c 'map({id, version, package})' <<<"$plugins")"
  printf 'smoke=%s\n' "$(jq -c 'map(select(.smoke != null) | {id, version, smoke})' <<<"$plugins")"
}

cmd_check_ids() {
  load_plugins
  local ids id dir found failed=0
  ids=$(selected_ids "$@")
  while IFS= read -r id; do
    dir=$(field "$id" dir)
    found=$(declared_ids "$dir/src")
    if [[ -z $found ]]; then
      report "$dir/src has no #[plugin(id = \"...\")]; the metadata id is $id"
      failed=1
    elif [[ $found == *$'\n'* ]]; then
      report "$dir/src has more than one #[plugin(...)]"
      failed=1
    elif [[ $found == - ]]; then
      report "the #[plugin(...)] in $dir/src sets no id; the metadata id is $id"
      failed=1
    elif [[ $found != "$id" ]]; then
      report "$dir/src declares #[plugin(id = \"$found\")] but $dir/Cargo.toml declares id = \"$id\""
      failed=1
    else
      printf '%s: id %s matches\n' "$dir" "$id"
    fi
  done <<<"$ids"
  return "$failed"
}

cmd_fmt() {
  load_plugins
  cargo_tc fmt --all --check
  local smoke
  while IFS= read -r smoke; do
    (cd "$smoke" && cargo_tc fmt --all --check)
  done < <(jq -r '.[] | .smoke // empty' <<<"$plugins")
}

cmd_clippy() {
  load_config
  cargo_tc clippy --locked --workspace --all-targets -- -D warnings
  cargo_tc clippy --locked --workspace --all-targets --target "$wasm_target" -- -D warnings
}

cmd_test() {
  load_plugins
  if (($# == 0)); then
    cargo_tc test --locked --workspace
    return
  fi
  local ids id packages=()
  ids=$(selected_ids "$@")
  while IFS= read -r id; do
    packages+=(-p "$(field "$id" package)")
  done <<<"$ids"
  cargo_tc test --locked "${packages[@]}"
}

cmd_check() {
  cmd_check_ids
  cmd_fmt
  cmd_clippy
  cmd_test
}

cmd_build() {
  load_plugins
  local ids id packages=() built header
  ids=$(selected_ids "$@")
  while IFS= read -r id; do
    packages+=(-p "$(field "$id" package)")
  done <<<"$ids"
  cargo_tc build --locked --release --target "$wasm_target" "${packages[@]}"
  mkdir -p "$dist"
  while IFS= read -r id; do
    built=$target_dir/$wasm_target/release/$(field "$id" lib).wasm
    [[ -f $built ]] || die "cargo did not produce $built"
    header=$(od -An -tx1 -N8 -- "$built" | tr -d ' \n')
    [[ $header == "$component_header" ]] || die "$built is not a WebAssembly component (header $header)"
    cp -- "$built" "$dist/$id.wasm"
    (cd "$dist" && checksum "$id.wasm" >"$id.wasm.sha256")
    printf '%s/%s.wasm  sha256 %s\n' "$dist" "$id" "$(cut -d' ' -f1 "$dist/$id.wasm.sha256")"
  done <<<"$ids"
}

cmd_smoke() {
  local prebuilt=false
  if [[ ${1:-} == --prebuilt ]]; then
    prebuilt=true
    shift
  fi
  load_plugins
  local ids id smoke variable component
  ids=$(selected_ids "$@")
  while IFS= read -r id; do
    smoke=$(field "$id" smoke)
    if [[ -z $smoke ]]; then
      printf '%s has no smoke workspace\n' "$id"
      continue
    fi
    variable=$(field "$id" component_env)
    if [[ -z $variable ]]; then
      (cd "$smoke" && cargo_tc test --locked)
      continue
    fi
    if [[ $prebuilt == false ]]; then
      cmd_build "$id"
    fi
    component=$root/$dist/$id.wasm
    [[ -f $component ]] || die "$dist/$id.wasm is missing"
    (cd "$dist" && checksum "$id.wasm" | cmp -s - "$id.wasm.sha256") \
      || die "$dist/$id.wasm does not match $dist/$id.wasm.sha256"
    printf 'smoke %s against %s\n' "$id" "$component"
    (cd "$smoke" && env "$variable=$component" cargo "+$toolchain" test --locked)
  done <<<"$ids"
}

cmd_resolve_tag() {
  (($# == 1)) || die "usage: resolve-tag <id>-v<version>"
  local tag=$1
  load_plugins
  local id matches=()
  while IFS= read -r id; do
    if [[ $tag == "$id"-v* && ${tag#"$id"-v} =~ $semver_pattern ]]; then
      matches+=("$id")
    fi
  done < <(all_ids)
  ((${#matches[@]} > 0)) \
    || die "tag $tag is not <id>-v<semver> for a plugin of this workspace; ids: $(all_ids | paste -sd ' ')"
  ((${#matches[@]} == 1)) || die "tag $tag matches several plugins: ${matches[*]}"
  id=${matches[0]}
  local version=${tag#"$id"-v} expected
  expected=$(field "$id" version)
  [[ $version == "$expected" ]] \
    || die "tag $tag releases $id $version but $(field "$id" dir)/Cargo.toml has version $expected"
  local prerelease=false
  if [[ ${version%%+*} == *-* ]]; then
    prerelease=true
  fi
  printf 'id=%s\nversion=%s\npackage=%s\nprerelease=%s\nsmoke=%s\n' \
    "$id" "$version" "$(field "$id" package)" "$prerelease" "$(field "$id" smoke)"
}

cmd_notes() {
  (($# == 1)) || die "usage: notes <id>"
  load_plugins
  local id
  id=$(selected_ids "$1")
  local version dir description sdk world contract commit repository sha permissions_toml permissions_md
  version=$(field "$id" version)
  dir=$(field "$id" dir)
  description=$(field "$id" description)
  sdk=$(field "$id" sdk)
  [[ -f $dist/$id.wasm && -f $dist/$id.wasm.sha256 ]] || die "build $id first: $dist/$id.wasm is missing"
  (cd "$dist" && checksum "$id.wasm" | cmp -s - "$id.wasm.sha256") \
    || die "$dist/$id.wasm does not match $dist/$id.wasm.sha256"
  sha=$(cut -d' ' -f1 "$dist/$id.wasm.sha256")
  [[ -n $sdk ]] || die "$dir/Cargo.toml has no path dependency on infrarust-plugin-sdk"
  world=$sdk/wit/world.wit
  [[ -f $world ]] || die "$world is missing"
  contract=$(sed -nE 's/^[[:space:]]*package[[:space:]]+(infrarust:plugin@[0-9]+\.[0-9]+\.[0-9]+)[[:space:]]*;.*$/\1/p' "$world" | head -n 1)
  [[ -n $contract ]] || die "$world has no package infrarust:plugin@<version> line"
  grep -qaF "infrarust:plugin/guest@${contract#*@}" "$dist/$id.wasm" \
    || die "$dist/$id.wasm does not export infrarust:plugin/guest@${contract#*@} although $world declares $contract"
  commit=$(git -C "$sdk" rev-parse HEAD 2>/dev/null) || die "cannot read the Infrarust commit from $sdk"
  if [[ -n ${GITHUB_REPOSITORY:-} ]]; then
    repository=${GITHUB_SERVER_URL:-https://github.com}/$GITHUB_REPOSITORY
  else
    repository=$(field "$id" repository)
  fi
  [[ -n $repository ]] || die "set GITHUB_REPOSITORY or the package repository of $id"
  permissions_toml=$(jq -r --arg id "$id" '.[] | select(.id == $id) | .permissions | map("\"" + . + "\"") | join(", ")' <<<"$plugins")
  permissions_md=$(jq -r --arg id "$id" '.[] | select(.id == $id) | .permissions | map("`" + . + "`") | join(", ")' <<<"$plugins")
  local tag=$id-v$version
  local download=$repository/releases/download/$tag
  local host_contract=${contract#*@}
  host_contract=${host_contract%.*}.x

  if [[ -n $description ]]; then
    printf '%s\n\n' "$description"
  fi
  printf '| | |\n|---|---|\n'
  printf '| Plugin id | `%s` |\n' "$id"
  printf '| Version | `%s` |\n' "$version"
  printf '| Plugin contract | `%s`, loaded by Infrarust hosts on contract `%s` |\n' "$contract" "$host_contract"
  printf '| Built against | [%s](%s) `%s` at [`%s`](%s/tree/%s) |\n' \
    "$infrarust_repository" "$infrarust_url" "$infrarust_ref" "${commit:0:12}" "$infrarust_url" "$commit"
  printf '| Permissions | %s |\n' "${permissions_md:-none beyond the baseline}"
  printf '| SHA-256 of `%s.wasm` | `%s` |\n' "$id" "$sha"
  printf '\n## Install\n\n'
  printf '```sh\n'
  printf 'curl -fLO %s/%s.wasm\n' "$download" "$id"
  printf 'curl -fLO %s/%s.wasm.sha256\n' "$download" "$id"
  printf 'sha256sum -c %s.wasm.sha256\n' "$id"
  printf '```\n\n'
  printf 'Copy `%s.wasm` to the top level of the Infrarust `plugins_dir`, replacing any older `%s.wasm`: ' "$id" "$id"
  printf 'two files that report the id `%s` are both refused.' "$id"
  if [[ -n $permissions_toml ]]; then
    printf ' Grant its permissions in `infrarust.toml`:\n\n'
    printf '```toml\n[plugins.%s]\npermissions = [%s]\n```\n' "$id" "$permissions_toml"
  else
    printf '\n'
  fi
  printf '\nConfiguration and limits: [%s/README.md](%s/blob/%s/%s/README.md).\n' "$dir" "$repository" "$tag" "$dir"
}

cmd_lock() {
  load_plugins
  cargo_tc metadata --format-version 1 >/dev/null
  local smoke
  while IFS= read -r smoke; do
    (cd "$smoke" && cargo_tc metadata --format-version 1 >/dev/null)
  done < <(jq -r '.[] | .smoke // empty' <<<"$plugins")
}

main() {
  if (($# == 0)); then
    usage >&2
    exit 2
  fi
  local command=$1
  shift
  case $command in
    config) cmd_config "$@" ;;
    list) cmd_list "$@" ;;
    matrix) cmd_matrix "$@" ;;
    check) cmd_check "$@" ;;
    check-ids) cmd_check_ids "$@" ;;
    fmt) cmd_fmt "$@" ;;
    clippy) cmd_clippy "$@" ;;
    test) cmd_test "$@" ;;
    build) cmd_build "$@" ;;
    smoke) cmd_smoke "$@" ;;
    resolve-tag) cmd_resolve_tag "$@" ;;
    notes) cmd_notes "$@" ;;
    lock) cmd_lock "$@" ;;
    -h | --help | help) usage ;;
    *)
      usage >&2
      die "unknown command $command"
      ;;
  esac
}

cd -- "$root"
main "$@"
