#!/usr/bin/env bash
# Codex device-code login inside the running Kanata container.
# Credentials stay in the container's dedicated Codex volume.
# usage: scripts/codex-login.sh [login|status|logout]
set -euo pipefail
cd "$(dirname -- "${BASH_SOURCE[0]}")/.."

action=${1:-login}
case $action in
  login | status | logout) ;;
  *) echo "usage: $0 [login|status|logout]" >&2; exit 2 ;;
esac
docker compose ps --status running --services | grep -qx kanata ||
  { echo 'error: Kanata is not running; start it with: docker compose up -d' >&2; exit 1; }

run() { docker compose exec "$@" kanata /usr/local/bin/kanata auth codex "$action" --config /etc/kanata/config.toml; }
if [[ $action == login ]]; then
  cat <<'EOF'
Before continuing, enable "device code authorization for Codex" in ChatGPT:
  Settings -> Security (personal account) or workspace permissions (admin).
Then open the URL below, sign in, and enter the one-time code. Ctrl-C cancels.

EOF
  run
  action=status
fi
run -T
