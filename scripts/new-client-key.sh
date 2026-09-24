#!/usr/bin/env bash
# Issue a client key using the Kanata image (no Rust toolchain needed).
# The key is written only to OUT (mode 0600); stdout gets the config block to paste.
# usage: scripts/new-client-key.sh ID OUT --chat ALIAS [--chat ALIAS]... [--transcription ALIAS]...
set -euo pipefail

[[ $# -ge 4 ]] || { sed -n 3,4p "$0" | sed 's/^# //' >&2; exit 2; }
id=$1 out=$2
shift 2
[[ ! -e $out ]] || { echo "error: $out already exists" >&2; exit 1; }
dir=$(cd -- "$(dirname -- "$out")" && pwd)
case $dir/ in
  "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"/*)
    echo 'error: write keys outside the repository (e.g. ~/.config/kanata/keys/)' >&2; exit 1 ;;
esac

tmp=$(mktemp -d "$dir/.kanata-key.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
docker run --rm --network none --read-only --user "$(id -u):$(id -g)" -v "$tmp:/out" \
  "${KANATA_IMAGE:-kanata:test}" key new --id "$id" "$@" --key-out /out/key
mv -n "$tmp/key" "$dir/$(basename -- "$out")"
chmod 600 "$dir/$(basename -- "$out")"
cat >&2 <<EOF

Key written to $dir/$(basename -- "$out") (not printed).
Next: paste the block above into config/config.toml, then: docker compose restart kanata
Send the key file's contents to its holder over a private channel.
EOF
