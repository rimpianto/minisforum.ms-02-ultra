#!/bin/sh
# Allinea la versione del crate al conteggio dei commit, così cargo stampa
# "Compiling amt-cli v0.1.NNN" e non un 0.1.0 fisso che non dice niente.
#
# Il conteggio include il commit che si sta per creare, quindi si aggiunge 1.
# Lo lancia .git/hooks/pre-commit; a mano: scripts/sync-version.sh
set -e

root=$(git rev-parse --show-toplevel)
cd "$root"

manifest="amt-rs/crates/amt-cli/Cargo.toml"
current=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$manifest" | head -1)

# Fino alla 0.x la patch era il numero di commit. Da 1.0.0 in poi la versione
# la decide chi scrive il codice: qui non si tocca niente.
case "$current" in
    0.*) ;;
    *) exit 0 ;;
esac

count=$(git rev-list --count HEAD)
version="0.1.$((count + 1))"
[ "$current" = "$version" ] && exit 0

# Solo la prima riga version del blocco [package].
tmp="$manifest.tmp"
awk -v v="$version" '
    !done && /^version = "/ { print "version = \"" v "\""; done=1; next }
    { print }
' "$manifest" > "$tmp"
mv "$tmp" "$manifest"
git add "$manifest"
echo "sync-version: $current -> $version"
