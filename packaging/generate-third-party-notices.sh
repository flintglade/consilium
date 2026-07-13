#!/usr/bin/env bash
set -euo pipefail

export LC_ALL=C
cd "$(dirname "${BASH_SOURCE[0]}")/.."

mode="${1:-generate}"
notices="THIRD_PARTY_NOTICES.txt"
fallbacks="packaging/license-fallbacks.json"
supplements="packaging/attribution-supplements.json"
release_targets=(
  x86_64-unknown-linux-gnu
  x86_64-pc-windows-msvc
)

fail() {
  printf 'third-party notice generation failed: %s\n' "$*" >&2
  exit 1
}

case "$mode" in
  generate | --check) ;;
  *) fail "unknown mode: $mode" ;;
esac

for command in cargo grep iconv jq realpath sha256sum; do
  command -v "$command" >/dev/null || fail "$command is required"
done
[[ -s Cargo.lock ]] || fail "Cargo.lock is missing or empty"
[[ -s "$fallbacks" ]] || fail "$fallbacks is missing or empty"
[[ -s "$supplements" ]] || fail "$supplements is missing or empty"

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT
metadata="$work_dir/cargo-metadata.json"
package_metadata="$work_dir/packages.tsv"
release_keys="$work_dir/release-keys"
release_packages="$work_dir/release-packages.tsv"
materials="$work_dir/materials.tsv"
missing_keys="$work_dir/missing-keys"
attribution_needed_keys="$work_dir/attribution-needed-keys"
generated="$work_dir/THIRD_PARTY_NOTICES.txt"
field_separator=$'\x1f'

# --locked makes a changed or incomplete lockfile an error. Cargo metadata
# returns the complete graph, including target-specific packages.
cargo metadata --locked --format-version 1 >"$metadata"

unresolved="$({
  jq -r '
    .workspace_members as $workspace
    | .packages[]
    | select(.id as $id | ($workspace | index($id) | not))
    | select(
        (.name | type != "string" or length == 0) or
        (.version | type != "string" or length == 0) or
        (.license == null or .license == "") or
        (.source == null or .source == "") or
        (.manifest_path | type != "string" or length == 0) or
        (.authors | type != "array") or
        any(.authors[]; type != "string") or
        (.repository != null and (.repository | type != "string"))
      )
    | "\(.name // "<missing-name>") \(.version // "<missing-version>")"
  ' "$metadata"
} || fail "could not inspect Cargo package metadata")"
if [[ -n "$unresolved" ]]; then
  printf 'Packages with unresolved name, version, license, source, or manifest metadata:\n%s\n' \
    "$unresolved" >&2
  fail "refusing to invent third-party metadata"
fi

invalid_fields="$({
  jq -r '
    .workspace_members as $workspace
    | .packages[]
    | select(.id as $id | ($workspace | index($id) | not))
    | select([.name, .version, .license, .source, .manifest_path,
              (.license_file // ""),
              (.authors | join("; ")), (.repository // "")]
        | any(test("[[:cntrl:]]")))
    | "\(.name) \(.version)"
  ' "$metadata"
} || fail "could not validate Cargo package metadata")"
if [[ -n "$invalid_fields" ]]; then
  printf 'Packages with control characters in notice metadata:\n%s\n' \
    "$invalid_fields" >&2
  fail "notice metadata must be one line per field"
fi

jq -r '
  .workspace_members as $workspace
  | .packages[]
  | select(.id as $id | ($workspace | index($id) | not))
  | ["\(.name)@\(.version)", .name, .version, .license, .source,
      .manifest_path, (.license_file // ""), (.authors | join("; ")),
      (.repository // "")]
  | join("\u001f")
' "$metadata" | sort -t "$field_separator" -k1,1 >"$package_metadata"

package_count="$(wc -l <"$package_metadata")"
[[ "$package_count" =~ ^[1-9][0-9]*$ ]] || fail "resolved package list is empty"
duplicate_package_keys="$(cut -d "$field_separator" -f1 "$package_metadata" | uniq -d)"
if [[ -n "$duplicate_package_keys" ]]; then
  printf 'Packages that cannot be represented unambiguously by name and version:\n%s\n' \
    "$duplicate_package_keys" >&2
  fail "duplicate package name/version pairs from different Cargo sources require generator support"
fi

# These are the two native targets for the desktop executable distributed by
# this repository. The full locked inventory above still covers every
# workspace package, including dependencies used only by the terminal app.
# cargo tree evaluates target cfg expressions and the active feature graph, so
# unrelated Android, Apple, WASM, and GNU-Windows lockfile entries are listed in
# the inventory but do not need binary payload notices in this release.
for target in "${release_targets[@]}"; do
  cargo tree \
    --locked \
    --package grok-chat-desktop \
    --target "$target" \
    --edges normal,build \
    --prefix none \
    --format '{p}'
done |
  sed -En 's/^([^ ]+) v([^ ]+).*/\1@\2/p' |
  sort -u >"$release_keys"

join -t "$field_separator" "$release_keys" "$package_metadata" >"$release_packages"
release_package_count="$(wc -l <"$release_packages")"
[[ "$release_package_count" =~ ^[1-9][0-9]*$ ]] ||
  fail "release target package list is empty"

is_notice_material() {
  local base="${1##*/}"
  base="${base,,}"
  case "$base" in
    license | license[-._]* | licence | licence[-._]* | \
      copying | copying[-._]* | notice | notice[-._]* | \
      copyright | copyright[-._]* | unlicense | unlicense[-._]*) ;;
    *) return 1 ;;
  esac
  case "$base" in
    *.c | *.cc | *.cpp | *.cxx | *.h | *.hpp | *.js | *.json | *.lock | \
      *.py | *.rs | *.toml | *.ts | *.xml | *.yaml | *.yml) return 1 ;;
  esac
  return 0
}

validate_text_material() {
  local actual="$1"
  local label="$2"
  [[ -s "$actual" ]] || fail "$label is missing or empty: $actual"
  iconv -f UTF-8 -t UTF-8 "$actual" >/dev/null 2>&1 ||
    fail "$label is not valid UTF-8: $actual"
}

append_material() {
  local key="$1"
  local label="$2"
  local actual="$3"
  local origin="$4"
  local expected_hash="${5:-}"
  validate_text_material "$actual" "$key material"
  local actual_hash
  actual_hash="$(sha256sum "$actual" | awk '{print $1}')"
  if [[ -n "$expected_hash" && "$actual_hash" != "$expected_hash" ]]; then
    fail "$key material hash is $actual_hash, expected $expected_hash ($label)"
  fi
  [[ ! "$label" =~ [[:cntrl:]] && ! "$origin" =~ [[:cntrl:]] ]] ||
    fail "$key material metadata contains a control character"
  printf '%s\t%s\t%s\t%s\t%s\n' \
    "$key" "$label" "$actual" "$origin" "$actual_hash" >>"$materials"
}

material_has_concrete_copyright() {
  local actual="$1"
  grep -Eai \
    '^[[:space:]]*([#/;*.-]+[[:space:]]*)?Copyright(s)?([[:space:]]|\(c\)|©)' \
    "$actual" |
    grep -Eaiv \
      '\[yyyy\]|\{yyyy\}|<year>|\[year\]|name of copyright owner|name of author' \
      >/dev/null
}

material_has_upstream_attribution() {
  local actual="$1"
  material_has_concrete_copyright "$actual" ||
    grep -Eaq '^[[:space:]]*authors[[:space:]]*=' "$actual"
}

: >"$materials"
: >"$missing_keys"

while IFS="$field_separator" read -r key name version license source manifest license_file _ _; do
  root="${manifest%/Cargo.toml}"
  [[ -d "$root" ]] || fail "$key source directory is missing: $root"
  package_materials=0

  while IFS= read -r -d '' actual; do
    is_notice_material "$actual" || continue
    relative="${actual#"$root"/}"
    append_material \
      "$key" \
      "$relative" \
      "$actual" \
      "locked Cargo source archive path: $relative"
    package_materials=$((package_materials + 1))
  done < <(find "$root" -type f -print0 | sort -z)

  if [[ -n "$license_file" ]]; then
    if [[ "$license_file" = /* ]]; then
      declared_license_file="$license_file"
    else
      declared_license_file="$root/$license_file"
    fi
    [[ -f "$declared_license_file" ]] ||
      fail "$key declared license_file is missing: $declared_license_file"
    relative="${declared_license_file#"$root"/}"
    append_material \
      "$key" \
      "$relative" \
      "$declared_license_file" \
      "locked Cargo package license_file: $relative"
    package_materials=$((package_materials + 1))
  fi

  if ((package_materials == 0)); then
    printf '%s\n' "$key" >>"$missing_keys"
  fi
done <"$release_packages"

sort -u -o "$missing_keys" "$missing_keys"
jq -e '
  .schema == 1 and
  (.entries | type == "array" and length > 0) and
  all(.entries[];
    (.package | type == "string" and length > 0 and (test("[[:cntrl:]]") | not)) and
    (.version | type == "string" and length > 0 and (test("[[:cntrl:]]") | not)) and
    (.license | type == "string" and length > 0 and (test("[[:cntrl:]]") | not)) and
    (.materials | type == "array" and length > 0) and
    all(.materials[];
      (.type == "vendored" or .type == "locked-package") and
      (.path | type == "string" and length > 0 and (test("[[:cntrl:]]") | not)) and
      (.sha256 | test("^[0-9a-f]{64}$")) and
      (.source | type == "string" and length > 0 and (test("[[:cntrl:]]") | not)) and
      (if .type == "vendored"
       then (.source | test("^https://raw\\.githubusercontent\\.com/[^/]+/[^/]+/[0-9a-f]{40}/"))
       else
         ((.package | type == "string" and length > 0 and (test("[[:cntrl:]]") | not)) and
          (.version | type == "string" and length > 0 and (test("[[:cntrl:]]") | not)))
       end)
    )
  )
' "$fallbacks" >/dev/null || fail "$fallbacks has invalid structure"

jq -r '.entries[] | "\(.package)@\(.version)"' "$fallbacks" |
  sort >"$work_dir/fallback-keys"
if [[ "$(uniq -d "$work_dir/fallback-keys" | wc -l)" != 0 ]]; then
  fail "$fallbacks contains duplicate package entries"
fi
if ! cmp --silent "$missing_keys" "$work_dir/fallback-keys"; then
  diff -u "$missing_keys" "$work_dir/fallback-keys" >&2 || true
  fail "reviewed fallbacks do not exactly cover release packages without source-archive notices"
fi

while IFS= read -r key; do
  package_row="$(awk -v FS="$field_separator" -v key="$key" '$1 == key { print; exit }' "$release_packages")"
  [[ -n "$package_row" ]] || fail "fallback package is not in the release graph: $key"
  IFS="$field_separator" read -r _ name version license _ _ _ _ _ <<<"$package_row"
  fallback_license="$(jq -r --arg name "$name" --arg version "$version" '
    .entries[] | select(.package == $name and .version == $version) | .license
  ' "$fallbacks")"
  [[ "$fallback_license" == "$license" ]] ||
    fail "$key fallback license is $fallback_license, expected locked metadata $license"

  while IFS=$'\x1f' read -r type source_package source_version path expected_hash origin; do
    case "$type" in
      vendored)
        [[ "$path" == packaging/license-fallbacks/* && "$path" != *'..'* ]] ||
          fail "$key vendored fallback path is outside packaging/license-fallbacks: $path"
        actual="$(realpath -e "$path")"
        fallback_root="$(realpath -e packaging/license-fallbacks)"
        [[ "$actual" == "$fallback_root"/* ]] ||
          fail "$key vendored fallback resolves outside packaging/license-fallbacks"
        label="reviewed upstream material: ${path#packaging/license-fallbacks/}"
        ;;
      locked-package)
        source_key="$source_package@$source_version"
        source_row="$(awk -v FS="$field_separator" -v key="$source_key" '$1 == key { print; exit }' "$package_metadata")"
        [[ -n "$source_row" ]] ||
          fail "$key fallback refers to a package absent from Cargo.lock: $source_key"
        IFS="$field_separator" read -r _ _ _ _ _ source_manifest _ _ _ <<<"$source_row"
        source_root="$(realpath -e "${source_manifest%/Cargo.toml}")"
        [[ "$path" != /* && "$path" != *'..'* ]] ||
          fail "$key locked-package fallback path is invalid: $path"
        actual="$(realpath -e "$source_root/$path")"
        [[ "$actual" == "$source_root"/* ]] ||
          fail "$key locked-package fallback resolves outside $source_key"
        label="$source_key / $path"
        ;;
      *) fail "$key has unknown fallback type: $type" ;;
    esac
    append_material "$key" "$label" "$actual" "$origin" "$expected_hash"
  done < <(jq -r --arg name "$name" --arg version "$version" '
    .entries[]
    | select(.package == $name and .version == $version)
    | .materials[]
    | [.type, (.package // ""), (.version // ""), .path, .sha256, .source]
    | join("\u001f")
  ' "$fallbacks")
done <"$missing_keys"

# An MIT permission template is not a substitute for its accompanying upstream
# attribution. Identify exact-MIT packages (plus the legacy MIT/Apache form in
# this graph) whose published archive provides no concrete copyright line, then
# require the reviewed supplement set to cover that list exactly. Supplements
# use either an immutable upstream COPYRIGHT file or exact authorship/source
# metadata from the locked package itself; no copyright owner is inferred.
: >"$attribution_needed_keys"
while IFS="$field_separator" read -r key _ _ license _ _ _ _ _; do
  [[ "$license" == 'MIT' || "$license" == 'MIT/Apache-2.0' ]] || continue
  has_concrete_copyright=0
  while IFS=$'\t' read -r _ _ actual _ _; do
    if material_has_concrete_copyright "$actual"; then
      has_concrete_copyright=1
      break
    fi
  done < <(awk -F '\t' -v key="$key" '$1 == key' "$materials")
  if ((has_concrete_copyright == 0)); then
    printf '%s\n' "$key" >>"$attribution_needed_keys"
  fi
done <"$release_packages"
sort -u -o "$attribution_needed_keys" "$attribution_needed_keys"

jq -e '
  .schema == 1 and
  (.entries | type == "array" and length > 0) and
  all(.entries[];
    (.package | type == "string" and length > 0 and (test("[[:cntrl:]]") | not)) and
    (.version | type == "string" and length > 0 and (test("[[:cntrl:]]") | not)) and
    (.license | type == "string" and length > 0 and (test("[[:cntrl:]]") | not)) and
    (.materials | type == "array" and length > 0) and
    all(.materials[];
      (.type == "vendored" or .type == "locked-package") and
      (.path | type == "string" and length > 0 and (test("[[:cntrl:]]") | not)) and
      (.sha256 | test("^[0-9a-f]{64}$")) and
      (.source | type == "string" and length > 0 and (test("[[:cntrl:]]") | not)) and
      (if .type == "vendored"
       then (.source | test("^https://raw\\.githubusercontent\\.com/[^/]+/[^/]+/[0-9a-f]{40}/"))
       else
         ((.package | type == "string" and length > 0 and (test("[[:cntrl:]]") | not)) and
          (.version | type == "string" and length > 0 and (test("[[:cntrl:]]") | not)))
       end)
    )
  )
' "$supplements" >/dev/null || fail "$supplements has invalid structure"

jq -r '.entries[] | "\(.package)@\(.version)"' "$supplements" |
  sort >"$work_dir/supplement-keys"
if [[ "$(uniq -d "$work_dir/supplement-keys" | wc -l)" != 0 ]]; then
  fail "$supplements contains duplicate package entries"
fi
if ! cmp --silent "$attribution_needed_keys" "$work_dir/supplement-keys"; then
  diff -u "$attribution_needed_keys" "$work_dir/supplement-keys" >&2 || true
  fail "reviewed attribution supplements do not exactly cover generic MIT materials"
fi

while IFS= read -r key; do
  package_row="$(awk -v FS="$field_separator" -v key="$key" '$1 == key { print; exit }' "$release_packages")"
  [[ -n "$package_row" ]] || fail "attribution supplement package is not in the release graph: $key"
  IFS="$field_separator" read -r _ name version license _ _ _ _ _ <<<"$package_row"
  supplement_license="$(jq -r --arg name "$name" --arg version "$version" '
    .entries[] | select(.package == $name and .version == $version) | .license
  ' "$supplements")"
  [[ "$supplement_license" == "$license" ]] ||
    fail "$key attribution supplement license is $supplement_license, expected $license"

  while IFS=$'\x1f' read -r type source_package source_version path expected_hash origin; do
    case "$type" in
      vendored)
        [[ "$path" == packaging/attribution-supplements/* && "$path" != *'..'* ]] ||
          fail "$key vendored supplement path is outside packaging/attribution-supplements: $path"
        actual="$(realpath -e "$path")"
        supplement_root="$(realpath -e packaging/attribution-supplements)"
        [[ "$actual" == "$supplement_root"/* ]] ||
          fail "$key vendored supplement resolves outside packaging/attribution-supplements"
        label="reviewed attribution supplement: ${path#packaging/attribution-supplements/}"
        ;;
      locked-package)
        source_key="$source_package@$source_version"
        source_row="$(awk -v FS="$field_separator" -v key="$source_key" '$1 == key { print; exit }' "$package_metadata")"
        [[ -n "$source_row" ]] ||
          fail "$key supplement refers to a package absent from Cargo.lock: $source_key"
        IFS="$field_separator" read -r _ _ _ _ _ source_manifest _ _ _ <<<"$source_row"
        source_root="$(realpath -e "${source_manifest%/Cargo.toml}")"
        [[ "$path" != /* && "$path" != *'..'* ]] ||
          fail "$key locked-package supplement path is invalid: $path"
        actual="$(realpath -e "$source_root/$path")"
        [[ "$actual" == "$source_root"/* ]] ||
          fail "$key locked-package supplement resolves outside $source_key"
        label="reviewed attribution supplement: $source_key / $path"
        ;;
      *) fail "$key has unknown attribution supplement type: $type" ;;
    esac
    append_material "$key" "$label" "$actual" "$origin" "$expected_hash"
  done < <(jq -r --arg name "$name" --arg version "$version" '
    .entries[]
    | select(.package == $name and .version == $version)
    | .materials[]
    | [.type, (.package // ""), (.version // ""), .path, .sha256, .source]
    | join("\u001f")
  ' "$supplements")
done <"$attribution_needed_keys"

while IFS= read -r key; do
  has_upstream_attribution=0
  while IFS=$'\t' read -r _ _ actual _ _; do
    if material_has_upstream_attribution "$actual"; then
      has_upstream_attribution=1
      break
    fi
  done < <(awk -F '\t' -v key="$key" '$1 == key' "$materials")
  ((has_upstream_attribution == 1)) ||
    fail "$key reviewed supplement contains no concrete upstream attribution"
done <"$attribution_needed_keys"

sort -t $'\t' -k1,1 -k2,2 -k5,5 -u -o "$materials" "$materials"
cut -f1 "$materials" | sort -u >"$work_dir/material-package-keys"
cut -d "$field_separator" -f1 "$release_packages" >"$work_dir/expected-material-package-keys"
if ! cmp --silent \
  "$work_dir/expected-material-package-keys" \
  "$work_dir/material-package-keys"; then
  diff -u \
    "$work_dir/expected-material-package-keys" \
    "$work_dir/material-package-keys" >&2 || true
  fail "not every release package has reviewed license/notice material"
fi

material_count="$(wc -l <"$materials")"
fallback_count="$(wc -l <"$missing_keys")"
supplement_count="$(wc -l <"$attribution_needed_keys")"
lock_hash="$(sha256sum Cargo.lock | awk '{print $1}')"

{
  printf '%s\n' \
    'Consilium third-party Rust dependency notices' \
    '================================================' \
    '' \
    'This deterministic inventory is generated from the complete dependency' \
    'graph resolved by the repository Cargo.lock. License expressions, package' \
    'authors, repositories, and Cargo source identifiers are reproduced exactly;' \
    'they are not rewritten or interpreted by Flintglade.' \
    '' \
    'The second section carries the exact license, copyright, attribution, and' \
    'notice files found in the locked crate archives used by the distributed' \
    'Linux x86_64 and Windows x86_64 binaries. Packages that omitted those files' \
    'from their crate archive use the small, checksum-pinned reviewed fallback' \
    'set in packaging/license-fallbacks.json. Exact MIT materials that omit a' \
    'concrete attribution are completed by packaging/attribution-supplements.json.' \
    'Generation stops on missing or changed metadata, files, hashes, coverage,' \
    'or UTF-8 text.' \
    '' \
    'This generated file records upstream terms and notices; it does not replace' \
    'the upstream license terms or make a legal conclusion about them.' \
    '' \
    "Cargo.lock SHA-256: $lock_hash" \
    "Complete locked third-party inventory: $package_count packages" \
    "Native release-target dependency set: $release_package_count packages" \
    "Included release-target materials: $material_count files" \
    "Checksum-pinned fallback packages: $fallback_count" \
    "Reviewed attribution supplement packages: $supplement_count" \
    "Release targets: ${release_targets[*]}" \
    'Regenerate: packaging/generate-third-party-notices.sh' \
    'Verify:     packaging/generate-third-party-notices.sh --check' \
    '' \
    'COMPLETE LOCKED DEPENDENCY INVENTORY' \
    '===================================='
  jq -r '
    .workspace_members as $workspace
    | [.packages[]
       | select(.id as $id | ($workspace | index($id) | not))
       | {name, version, license, source, authors, repository}]
    | sort_by(.name, .version, .source)
    | .[]
    | "\nPackage: \(.name) \(.version)\nDeclared license: \(.license)\nCargo source: \(.source)"
      + (if (.authors | length) > 0
         then "\nCargo package authors: \(.authors | join("; "))"
         else "" end)
      + (if .repository != null
         then "\nUpstream repository: \(.repository)"
         else "" end)
      + (if .source == "registry+https://github.com/rust-lang/crates.io-index"
         then "\nSource package: https://crates.io/crates/\(.name)/\(.version)"
            + "\nSource archive: https://crates.io/api/v1/crates/\(.name)/\(.version)/download"
         else "" end)
  ' "$metadata"

  printf '%s\n' \
    '' \
    '' \
    'NATIVE RELEASE-TARGET LICENSE AND NOTICE MATERIALS' \
    '=================================================='

  while IFS="$field_separator" read -r key name version license source _ _ authors repository; do
    printf '\nPackage: %s %s\nDeclared license: %s\nCargo source: %s\n' \
      "$name" "$version" "$license" "$source"
    [[ -z "$authors" ]] || printf 'Cargo package authors: %s\n' "$authors"
    [[ -z "$repository" ]] || printf 'Upstream repository: %s\n' "$repository"
    if [[ "$source" == 'registry+https://github.com/rust-lang/crates.io-index' ]]; then
      printf 'Source package: https://crates.io/crates/%s/%s\n' "$name" "$version"
      printf 'Source archive: https://crates.io/api/v1/crates/%s/%s/download\n' \
        "$name" "$version"
    fi
    while IFS=$'\t' read -r _ label actual origin hash; do
      printf '\nMaterial: %s\nMaterial origin: %s\nSHA-256: %s\n' \
        "$label" "$origin" "$hash"
      printf '%s\n' '----- BEGIN UPSTREAM MATERIAL -----'
      cat "$actual"
      last_byte="$(tail -c 1 "$actual" | od -An -t u1 | tr -d ' ')"
      [[ "$last_byte" == 10 ]] || printf '\n'
      printf '%s\n' '----- END UPSTREAM MATERIAL -----'
    done < <(awk -F '\t' -v key="$key" '$1 == key' "$materials")
  done <"$release_packages"
} >"$generated"

[[ -s "$generated" ]] || fail "generated notice file is empty"

if [[ "$mode" == "--check" ]]; then
  [[ -f "$notices" ]] || fail "$notices is missing; run this script without --check"
  if ! cmp --silent "$notices" "$generated"; then
    diff -u "$notices" "$generated" >&2 || true
    fail "$notices is stale; regenerate it from the locked graph"
  fi
  printf '%s is current (%s locked packages, %s release packages, %s materials)\n' \
    "$notices" "$package_count" "$release_package_count" "$material_count"
else
  mv "$generated" "$notices"
  printf 'generated %s (%s locked packages, %s release packages, %s materials)\n' \
    "$notices" "$package_count" "$release_package_count" "$material_count"
fi
