#!/usr/bin/env bash
# Kanata deployment helper: Docker lifecycle, config checks, Codex sign-in and keys.
# Uses the repo's .env (COMPOSE_FILE, KANATA_CONFIG_FILE, ...) like `docker compose`.
set -euo pipefail
cd "$(dirname -- "${BASH_SOURCE[0]}")/.."

# Image tag: KANATA_IMAGE from the environment or .env, as Compose resolves it.
IMAGE=${KANATA_IMAGE:-$(sed -n 's/^KANATA_IMAGE=//p' .env 2>/dev/null | tail -1)}
IMAGE=${IMAGE:-kanata:test}
# Temp paths are global so EXIT traps can still see them.
tmp=

usage() {
  cat <<'EOF'
Usage: scripts/kanata.sh <command> [args]

Lifecycle
  build                      Build the Kanata image
  check                      Validate the live config with the image (offline)
  up                         Check the config, then start the stack
  down                       Stop and remove the stack
  restart                    Recreate Kanata (both containers with the public profile)
  status                     Show containers and Codex sign-in state
  logs [compose logs args]   Follow Kanata logs (default: -f --tail 100 of both Kanata containers)

Codex
  codex login|status|logout  Device-code sign-in inside the running container

Keys (host `kanata` binary: cargo install --locked --path .; changes apply without restart)
  key <new|list|show|edit|rm|rotate|migrate> [args]
                             Run `kanata key ...` with --config set to the deployed config
  key new ID OUT --chat ALIAS... --expires DAYS
                             Deprecated: kanata key new --id ID --key-out OUT ...
  owner-key rotate --expires DAYS
                             Deprecated: kanata key rotate --owner, key -> KANATA_OWNER_KEY_FILE

Local models
  ollama-context MODEL TOKENS [NAME]
                             Create an Ollama variant of MODEL with a fixed context window
                             (default NAME: MODEL-<N>k) and print the route to add
  fm-serve install|uninstall|status
                             Run Apple's `fm serve` (macOS 27+) on 127.0.0.1 as a login agent
EOF
}

die() { echo "error: $*" >&2; exit 1; }

# Deployed paths exactly as Compose mounts them (honours .env): config, keys dir, state dir.
config= keys_dir= state_dir=
deploy_paths() {
  local model out
  [[ -z $config ]] || return 0
  model=$(docker compose config --format json) || die "cannot read the Compose model; check .env and COMPOSE_FILE"
  out=$(python3 -c '
import json, sys
mounts = {v.get("target"): v.get("source") for v in json.loads(sys.argv[1])["services"]["kanata"]["volumes"]}
state = mounts.get("/etc/kanata/state") or ""
print(mounts.get("/etc/kanata/config.toml") or "")
print(mounts.get("/etc/kanata/keys") or "")
print(state[: -len("/private")] if state.endswith("/private") else "")' "$model") || die "cannot read the Compose mounts"
  { read -r config; read -r keys_dir; read -r state_dir; } <<<"$out"
  [[ -n $config && -n $keys_dir && -n $state_dir ]] ||
    die "the kanata service must mount the config, KANATA_KEYS_DIR and KANATA_STATE_DIR/private"
  [[ -f $config ]] || die "config not found: $config"
  # Host CLI and containers must resolve the same keys file and state dir.
  python3 - "$config" "$keys_dir" "$state_dir" <<'PY' || exit 1
import os, sys, tomllib
config, keys_dir, state_dir = sys.argv[1:4]
base = os.path.dirname(os.path.realpath(config))
def fail(msg):
    sys.exit("error: " + msg)
if os.path.realpath(keys_dir) != os.path.join(base, "keys"):
    fail(f"KANATA_KEYS_DIR ({keys_dir}) must be {os.path.join(base, 'keys')}")
if os.path.realpath(state_dir) != os.path.join(base, "state"):
    fail(f"KANATA_STATE_DIR ({state_dir}) must be {os.path.join(base, 'state')}")
try:
    with open(config, "rb") as f:
        keys = tomllib.load(f).get("keys")
except (OSError, tomllib.TOMLDecodeError) as e:
    fail(f"cannot read {config}: {e}")
# Inline [[application_keys]] (no [keys]) is deprecated but still deployable.
if keys is not None and (keys.get("file") != "keys/keys.toml" or keys.get("usage_dir") != "state"):
    fail(f'{config}: Compose requires [keys] file = "keys/keys.toml" and usage_dir = "state"')
PY
}

# Missing mount sources would fail `up` (create_host_path: false).
prepare_dirs() {
  local dir
  for dir in "$keys_dir" "$state_dir" "$state_dir/private" "$state_dir/public"; do
    [[ -d $dir ]] || { mkdir -p -- "$dir" && chmod 700 "$dir"; } || die "cannot create $dir"
  done
}

check_config() {
  local services
  deploy_paths
  prepare_dirs
  # Config at /c.toml, keys at /keys: the relative keys/keys.toml resolves the same way.
  # Explicit exits: callers use `check_config && …`, which disables set -e here.
  docker run --rm --network none --read-only -v "$config:/c.toml:ro" -v "$keys_dir:/keys:ro" "$IMAGE" check --config /c.toml ||
    die "config failed kanata check"
  services=$(kanata_services) || exit 1
  if grep -qx kanata-public <<<"$services"; then
    docker run --rm --network none --read-only -v "$config:/c.toml:ro" -v "$keys_dir:/keys:ro" "$IMAGE" \
      check --config /c.toml --plane public >/dev/null || die "config is not valid for the public plane"
  fi
  return 0
}

host_kanata() {
  command -v kanata >/dev/null || die 'host `kanata` not found; install it with: cargo install --locked --path .'
}

deprecated() { echo "deprecated: $*" >&2; }

# `kanata key <sub> ...` against the deployed config.
key_passthrough() {
  local arg
  host_kanata
  deploy_paths
  for arg in "$@"; do
    [[ $arg == --config || $arg == --keys ]] && exec kanata key "$@"
  done
  exec kanata key "$@" --config "$config"
}

running() { docker compose ps --status running --services | grep -qx kanata; }

# Kanata containers in this Compose model: kanata, plus kanata-public with the public profile.
kanata_services() {
  local services
  services=$(docker compose config --services | grep -xE 'kanata|kanata-public') || true
  [[ -n $services ]] || die "cannot read the Compose services; check .env and COMPOSE_FILE"
  printf '%s\n' "$services"
}

# Recreates the Kanata containers only (never cloudflared).
recreate_kanata() {
  local services
  services=$(kanata_services) || exit 1
  # shellcheck disable=SC2086
  docker compose up -d --no-build --force-recreate $services
}

codex() {
  local action=${1:-}
  case $action in login | status | logout) ;; *) die "usage: codex login|status|logout" ;; esac
  running || die 'Kanata is not running; start it with: scripts/kanata.sh up'
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
}

# Legacy `key new ID OUT ...`.
key_new_legacy() {
  local id=$1 out=$2 arg
  shift 2
  deprecated "key new ID OUT; use: kanata key new --config <config> --id $id --key-out $out ..."
  for arg in "$@"; do [[ $arg == --expires ]] && break; done
  [[ $arg == --expires ]] ||
    die "--expires is required: scripts/kanata.sh key new --id $id --key-out $out $* --expires <1|3|7|13|30|60|unlimited>"
  host_kanata
  case $(cd -- "$(dirname -- "$out")" 2>/dev/null && pwd)/ in
    "$PWD"/*) die 'write keys outside the repository (e.g. ~/.config/kanata/keys/)' ;;
  esac
  deploy_paths
  exec kanata key new --config "$config" --id "$id" --key-out "$out" "$@"
}

# Host Ollama, as seen from this machine (not from the container).
OLLAMA_URL=${KANATA_OLLAMA_URL:-http://127.0.0.1:11434}

ollama_context() {
  [[ $# -ge 2 && $# -le 3 ]] || die "usage: ollama-context MODEL TOKENS [NAME]"
  local model=$1 tokens=$2 name=${3:-}
  [[ $tokens =~ ^[0-9]+$ ]] && ((tokens >= 256 && tokens <= 16777216)) ||
    die "TOKENS must be an integer from 256 to 16777216"
  case $model in *-cloud | *:cloud) die "cloud models already run at their full context" ;; esac
  if [[ -z $name ]]; then
    if ((tokens % 1024 == 0)); then name="$model-$((tokens / 1024))k"; else name="$model-ctx$tokens"; fi
  fi
  command -v curl >/dev/null || die "curl is required"

  # The base model's own maximum caps the variant.
  local show max
  show=$(curl -fsS "$OLLAMA_URL/api/show" -d "$(python3 -c 'import json,sys; print(json.dumps({"model": sys.argv[1]}))' "$model")" 2>/dev/null) ||
    die "cannot read $model from Ollama at $OLLAMA_URL (is it pulled?)"
  max=$(python3 -c 'import json, sys
info = json.loads(sys.argv[1]).get("model_info") or {}
print(next((v for k, v in info.items() if k.endswith(".context_length")), ""))' "$show")
  if [[ -n $max ]] && ((tokens > max)); then
    die "$model supports at most $max tokens"
  fi

  python3 -c 'import json, sys
print(json.dumps({"model": sys.argv[1], "from": sys.argv[2], "parameters": {"num_ctx": int(sys.argv[3])}, "stream": False}))' \
    "$name" "$model" "$tokens" | curl -fsS "$OLLAMA_URL/api/create" -d @- >/dev/null ||
    die "Ollama could not create $name"

  cat <<EOF
Created Ollama model $name ($tokens-token context, shares $model's weights).
Add or update a route in the config, then: scripts/kanata.sh restart

[[routes]]
id = "ollama-$(printf '%s' "$name" | tr -c 'A-Za-z0-9\n' '-')"
model_alias = "${name//:/-}"
operation = "chat"
adapter_id = "<your local Ollama adapter>"
upstream_id = "$name"
context_tokens = $tokens
requires_streaming_chat = true
requires_function_tools = false
EOF
}

FM_LABEL=dev.kanata.fm-serve
FM_PORT=${KANATA_FM_PORT:-1976}
FM_PLIST=$HOME/Library/LaunchAgents/$FM_LABEL.plist

fm_serve() {
  [[ $(uname -s) == Darwin && -x /usr/bin/fm ]] || die "fm serve needs macOS 27 or later (/usr/bin/fm)"
  local domain
  domain=gui/$(id -u)
  case ${1:-} in
    install)
      /usr/bin/fm available --model system >/dev/null 2>&1 ||
        die "the on-device model is unavailable; turn on Apple Intelligence first"
      mkdir -p "$(dirname -- "$FM_PLIST")" "$HOME/Library/Logs"
      # Loopback only: fm serve has no authentication.
      cat >"$FM_PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$FM_LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/bin/fm</string><string>serve</string>
    <string>--host</string><string>127.0.0.1</string>
    <string>--port</string><string>$FM_PORT</string>
  </array>
  <key>EnvironmentVariables</key><dict><key>NO_COLOR</key><string>1</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$HOME/Library/Logs/kanata-fm-serve.log</string>
  <key>StandardErrorPath</key><string>$HOME/Library/Logs/kanata-fm-serve.log</string>
</dict>
</plist>
EOF
      launchctl bootout "$domain/$FM_LABEL" 2>/dev/null || true
      launchctl bootstrap "$domain" "$FM_PLIST"
      echo "fm serve runs at http://127.0.0.1:$FM_PORT (from Kanata's container: http://host.orb.internal:$FM_PORT/v1)."
      ;;
    uninstall)
      launchctl bootout "$domain/$FM_LABEL" 2>/dev/null || true
      rm -f "$FM_PLIST"
      echo "fm serve agent removed."
      ;;
    status)
      if launchctl print "$domain/$FM_LABEL" >/dev/null 2>&1; then echo "agent: loaded"; else echo "agent: not loaded"; fi
      if curl -fsS -m 3 "http://127.0.0.1:$FM_PORT/health" >/dev/null 2>&1; then echo "health: ok (port $FM_PORT)"; else echo "health: unreachable (port $FM_PORT)"; fi
      ;;
    *) die "usage: fm-serve install|uninstall|status" ;;
  esac
}

# Legacy `owner-key rotate`: kanata key rotate --owner, key -> KANATA_OWNER_KEY_FILE.
owner_key_rotate() {
  local expires= key_file key_dir
  deprecated "owner-key rotate; use: kanata key rotate --owner --config <config> --expires <days> --key-out <file>"
  while [[ $# -gt 0 ]]; do
    case $1 in
      --no-restart) shift ;;
      --expires) [[ $# -ge 2 ]] || die "--expires needs a value"; expires=$2; shift 2 ;;
      *) die "usage: owner-key rotate --expires <1|3|7|13|30|60|unlimited>" ;;
    esac
  done
  [[ -n $expires ]] || die "--expires is required: scripts/kanata.sh owner-key rotate --expires <1|3|7|13|30|60|unlimited>"
  host_kanata
  # Host-side key path: KANATA_OWNER_KEY_FILE from the environment or .env.
  key_file=${KANATA_OWNER_KEY_FILE:-$(sed -n 's/^KANATA_OWNER_KEY_FILE=//p' .env 2>/dev/null | tail -1)}
  key_file=${key_file:-$HOME/.config/kanata/owner-client-key}
  key_dir=$(dirname -- "$key_file")
  [[ -d $key_dir ]] || die "key directory does not exist: $key_dir"
  case $key_file in "$PWD"/*) die 'the owner key must live outside the repository' ;; esac
  deploy_paths
  python3 - "$config" "$keys_dir/keys.toml" "$key_file" "$expires" <<'PY' || exit 1
import sys, tomllib
config, keys_file, key_file, expires = sys.argv[1:5]
with open(config, "rb") as f:
    if "keys" not in tomllib.load(f):
        sys.exit(f"error: {config} uses inline [[application_keys]]; run `kanata key migrate --config {config}` first")
try:
    with open(keys_file, "rb") as f:
        keys = tomllib.load(f).get("keys", [])
except FileNotFoundError:
    keys = []
if not any(k.get("owner") and "revoked_at" not in k for k in keys):
    sys.exit(f"error: no active owner key; create one with:\n  kanata key new --owner --id owner --chat <alias>... --expires {expires} --config {config} --key-out {key_file}")
PY
  # The CLI never overwrites a key file; write beside it, then replace.
  tmp=$(mktemp -d "$key_dir/.owner-key.XXXXXX")
  trap 'rm -rf "$tmp"' EXIT
  kanata key rotate --owner --config "$config" --expires "$expires" --key-out "$tmp/key"
  chmod 600 "$tmp/key"
  mv -f "$tmp/key" "$key_file"
  rm -rf "$tmp"
  trap - EXIT
  cat <<EOF
New owner key written to $key_file (not printed); the previous one stops working within seconds.

Next:
  - Update every client that uses the owner key (copy it from the key file; don't paste it into chats).
If you suspect a compromise, also:
  - Rotate other keys: kanata key rotate <id> --config $config --expires <days> (or revoke: kanata key rm <id>).
  - Codex: \`codex logout\`, sign out Codex sessions in ChatGPT security settings, then \`codex login\`.
  - Cloudflare: refresh the tunnel token (replace its file, then \`up\`), and roll any
    Cloudflare API tokens used by your reverse proxy.
  - Check \`scripts/kanata.sh logs\` for unfamiliar key ids or client IPs.
EOF
}

command=${1:-help}
[[ $# -eq 0 ]] || shift
case $command in
  build) docker build -t "$IMAGE" . ;;
  check) check_config ;;
  up) check_config && docker compose up -d --no-build ;;
  down) docker compose down ;;
  restart) check_config && recreate_kanata ;;
  status)
    docker compose ps
    if running; then codex status; fi
    ;;
  logs)
    if [[ $# -eq 0 ]]; then
      services=$(kanata_services) || exit 1
      # shellcheck disable=SC2086
      docker compose logs -f --tail 100 $services
    else
      docker compose logs "$@"
    fi
    ;;
  codex) codex "$@" ;;
  key)
    [[ $# -gt 0 ]] || die "usage: key <new|list|show|edit|rm|rotate|migrate> [args] (see: kanata key)"
    if [[ $1 == new && $# -ge 3 && $2 != -* && $3 != -* ]]; then
      shift
      key_new_legacy "$@"
    fi
    key_passthrough "$@"
    ;;
  ollama-context) ollama_context "$@" ;;
  fm-serve) fm_serve "$@" ;;
  owner-key)
    [[ ${1:-} == rotate ]] || die "usage: owner-key rotate --expires <days>"
    shift
    owner_key_rotate "$@"
    ;;
  help | -h | --help) usage ;;
  *) usage >&2; exit 2 ;;
esac
