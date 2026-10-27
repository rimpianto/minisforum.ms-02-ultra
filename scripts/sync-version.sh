#!/bin/sh
# Keep the crate version in step with the commit count, so cargo prints
# "Compiling amt-cli v0.1.NNN" instead of a fixed 0.1.0 that says nothing.
#
# The count includes the commit about to be created, hence the +1.
# Run by .git/hooks/pre-commit; by hand: scripts/sync-version.sh
set -e

root=$(git rev-parse --show-toplevel)
cd "$root"

manifest="amt-rs/crates/amt-cli/Cargo.toml"
current=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$manifest" | head -1)

# Up to 0.x the patch number was the commit count. From 1.0.0 on the version
# is decided by whoever writes the code: nothing is touched here.
case "$current" in
    0.*) ;;
    *) exit 0 ;;
esac

count=$(git rev-list --count HEAD)
version="0.1.$((count + 1))"
[ "$current" = "$version" ] && exit 0

# Only the first version line of the [package] block.
tmp="$manifest.tmp"
awk -v v="$version" '
    !done && /^version = "/ { print "version = \"" v "\""; done=1; next }
    { print }
' "$manifest" > "$tmp"
mv "$tmp" "$manifest"
git add "$manifest"
echo "sync-version: $current -> $version"
