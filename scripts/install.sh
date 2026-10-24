#!/bin/sh
# Build tunemend and install it with the production skill under a prefix.
# Usage: scripts/install.sh <prefix>
set -eu
prefix=${1:?usage: scripts/install.sh <prefix>}
root=$(cd "$(dirname "$0")/.." && pwd)
cargo install --locked --quiet --path "$root" --root "$prefix" --bin tunemend
mkdir -p "$prefix/share/tunemend/skills"
rm -rf "$prefix/share/tunemend/skills/tunemend"
cp -R "$root/skills/tunemend" "$prefix/share/tunemend/skills/"
"$prefix/bin/tunemend" --version
echo "skill: $prefix/share/tunemend/skills/tunemend"
