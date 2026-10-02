#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
task_dir=$(mktemp -d)
trap 'rm -rf "$task_dir"' EXIT HUP INT TERM
mkdir "$task_dir/keys"
printf 'version = 1\nkeys = []\n' > "$task_dir/keys/keys.toml"
chmod 600 "$task_dir/keys/keys.toml"
for template in config/*.example.toml; do
    cp "$template" "$task_dir/"
    cargo run --locked -q -- check --config "$task_dir/${template##*/}"
done
