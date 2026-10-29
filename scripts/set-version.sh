#!/usr/bin/env bash
# Set the crate version in Cargo.toml ([package] only) and Cargo.lock.
#
#   scripts/set-version.sh 0.0.3
#
# Used by the release job in .github/workflows/ci.yml so the tag and
# Cargo.toml always agree. Safe to run locally.

set -euo pipefail

version="${1:?usage: set-version.sh <major.minor.patch>}"
if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "set-version: '$version' is not major.minor.patch" >&2
    exit 2
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
name="$(sed -n '/^\[package\]/,/^\[/s/^name = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -n1)"

# Cargo.toml: the first `version =` inside [package].
awk -v v="$version" '
    /^\[/ { in_pkg = ($0 == "[package]") }
    in_pkg && !done && /^version = "/ { print "version = \"" v "\""; done = 1; next }
    { print }
    END { if (!done) exit 1 }
' "$root/Cargo.toml" > "$root/Cargo.toml.tmp"
mv "$root/Cargo.toml.tmp" "$root/Cargo.toml"

# Cargo.lock: the version line of this crate's [[package]] entry.
if [ -f "$root/Cargo.lock" ]; then
    awk -v n="$name" -v v="$version" '
        $0 == "name = \"" n "\"" { print; want = 1; next }
        want && /^version = "/ { print "version = \"" v "\""; want = 0; done = 1; next }
        { print }
        END { if (!done) exit 1 }
    ' "$root/Cargo.lock" > "$root/Cargo.lock.tmp"
    mv "$root/Cargo.lock.tmp" "$root/Cargo.lock"
fi

echo "set-version: $name -> $version"
