#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
task_dir=$(mktemp -d)
task_name="kanata-smoke-$$"
trap 'docker rm -f "$task_name" >/dev/null 2>&1 || true; rm -rf "$task_dir"' EXIT HUP INT TERM
cp tests/fixtures/deploy/smoke.toml "$task_dir/config.toml"
chmod 755 "$task_dir"
chmod 644 "$task_dir/config.toml"
docker build -t kanata:ci .
docker run -d --network none --name "$task_name" --read-only --cap-drop ALL --security-opt no-new-privileges \
    --mount "type=bind,src=$task_dir/config.toml,dst=/etc/kanata/config.toml,readonly" \
    kanata:ci serve --config /etc/kanata/config.toml --plane private >/dev/null
attempt=0
until docker exec "$task_name" /usr/local/bin/kanata health --config /etc/kanata/config.toml; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 20 ]; then docker logs "$task_name"; exit 1; fi
    sleep 1
done
docker stop -t 35 "$task_name" >/dev/null
test "$(docker inspect --format '{{.State.ExitCode}}' "$task_name")" = 0
