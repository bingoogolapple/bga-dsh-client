#!/usr/bin/env bash
set -euo pipefail

tag=$1
directory=$2
repository=${GH_REPO:?GH_REPO is required}
asset_manifest=$(mktemp)
trap 'rm -f "$asset_manifest"' EXIT

# Only the files already checked by the Release workflow reach this directory.
files=("$directory"/*)
for file in "${files[@]}"; do
  digest=$(sha256sum "$file")
  jq -cn --arg name "${file##*/}" --arg digest "sha256:${digest%% *}" \
    --argjson size "$(stat -c %s "$file")" '{name: $name, digest: $digest, size: $size}' >> "$asset_manifest"
done

release=$(gh api --paginate --slurp "repos/$repository/releases?per_page=100" |
  jq -c --arg tag "$tag" '[.[][] | select(.tag_name == $tag)] | first // empty')
if [[ -z "$release" ]]; then
  options=()
  version_without_build=${tag%%+*}
  if [[ "$version_without_build" == *-* ]]; then
    options+=(--prerelease)
  fi
  gh release create "$tag" --verify-tag --draft --generate-notes \
    --notes-file .github/release-notes.md "${options[@]}"
  release_id=$(gh release view "$tag" --json databaseId --jq '.databaseId')
  draft=true
else
  release_id=$(jq -r '.id' <<< "$release")
  draft=$(jq -r '.draft' <<< "$release")
fi

release_assets() {
  gh api --paginate --slurp "repos/$repository/releases/$release_id/assets?per_page=100" |
    jq -c 'add | map({name, digest, size, state})'
}

assets=$(release_assets)
if [[ "$draft" == true ]]; then
  # A failed upload leaves a draft. On retry keep matching files, including
  # after a network failure just after the server accepted an upload.
  changed=$(jq -nr --slurpfile expected "$asset_manifest" --argjson actual "$assets" '
    if any($actual[]; .name as $name | all($expected[]; .name != $name)) then
      error("Draft contains unexpected assets")
    else
      $expected[] as $file | select(all($actual[]; . != ($file + {state: "uploaded"}))) | $file.name
    end
  ')
  if [[ -n "$changed" ]]; then
    upload=()
    while IFS= read -r name; do
      upload+=("$directory/$name")
    done <<< "$changed"
    gh release upload "$tag" --clobber "${upload[@]}"
    assets=$(release_assets)
  fi
fi

jq -en --slurpfile expected "$asset_manifest" --argjson actual "$assets" '
  ($actual | sort_by(.name)) == ($expected | map(. + {state: "uploaded"}) | sort_by(.name))
' > /dev/null || { echo "Release assets do not match the verified files." >&2; exit 1; }

if [[ "$draft" == true ]]; then
  gh release edit "$tag" --draft=false
else
  echo "Release $tag already contains the verified files."
fi
