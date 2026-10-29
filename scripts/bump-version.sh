#!/usr/bin/env bash
# Bump the crate version for a PR: patch, minor or major.
#
#   scripts/bump-version.sh patch     # or: make bump-patch
#
# Bumps from whichever is higher, Cargo.toml or the latest vX.Y.Z tag, so a
# Cargo.toml that fell behind the tags catches up. CI requires every PR into
# main to carry a version above the latest tag; the merge is tagged with it.

set -euo pipefail

part="${1:?usage: bump-version.sh patch|minor|major}"
case "$part" in
    patch|minor|major) ;;
    *) echo "bump-version: expected patch, minor or major, got '$part'" >&2; exit 2 ;;
esac
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

current="$(sed -n '/^\[package\]/,/^\[/s/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -n1)"
git -C "$root" fetch --quiet --tags 2>/dev/null || echo "bump-version: could not fetch tags; using local tags" >&2
latest_tag="$(git -C "$root" tag --list 'v[0-9]*.[0-9]*.[0-9]*' --sort=-v:refname | head -n1)"
base="$(printf '%s\n%s\n' "$current" "${latest_tag#v}" | sed '/^$/d' | sort -V | tail -n1)"

IFS=. read -r major minor patch <<< "$base"
case "$part" in
    patch) next="$major.$minor.$((patch + 1))" ;;
    minor) next="$major.$((minor + 1)).0" ;;
    major) next="$((major + 1)).0.0" ;;
esac

echo "bump-version: Cargo.toml $current, latest tag ${latest_tag:-none} -> $next"
"$root/scripts/set-version.sh" "$next"
