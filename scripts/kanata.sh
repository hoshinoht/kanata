#!/usr/bin/env bash
# Kanata deployment helper: Docker lifecycle, config checks, Codex sign-in and keys.
# Uses the repo's .env (COMPOSE_FILE, KANATA_CONFIG_FILE, ...) like `docker compose`.
set -euo pipefail
cd "$(dirname -- "${BASH_SOURCE[0]}")/.."

IMAGE=${KANATA_IMAGE:-kanata:test}
# Temp paths are global so EXIT traps can still see them.
tmp= new_key= new_config=

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

Keys
  key new ID OUT --chat ALIAS [--chat ALIAS]... [--transcription ALIAS]...
                             Issue a client key: key -> OUT (0600), config block -> stdout
  owner-key rotate [--no-restart]
                             Create or rotate the owner key; update its digest in the config

Local models
  ollama-context MODEL TOKENS [NAME]
                             Create an Ollama variant of MODEL with a fixed context window
                             (default NAME: MODEL-<N>k) and print the route to add
  fm-serve install|uninstall|status
                             Run Apple's `fm serve` (macOS 27+) on 127.0.0.1 as a login agent
EOF
}

die() { echo "error: $*" >&2; exit 1; }

# Config path exactly as Compose mounts it (honours .env).
config_path() {
  local model
  model=$(docker compose config --format json) || die "cannot read the Compose model; check .env and COMPOSE_FILE"
  python3 -c '
import json, sys
for v in json.loads(sys.argv[1])["services"]["kanata"]["volumes"]:
    if v.get("target") == "/etc/kanata/config.toml":
        print(v["source"])' "$model"
}

check_config() {
  local config=${1:-} services
  if [[ -z $config ]]; then config=$(config_path) || exit 1; fi
  [[ -f $config ]] || die "config not found: $config"
  # Explicit exits: callers use `check_config && …`, which disables set -e here.
  docker run --rm --network none --read-only -v "$config:/c.toml:ro" "$IMAGE" check --config /c.toml ||
    die "config failed kanata check"
  services=$(kanata_services) || exit 1
  if grep -qx kanata-public <<<"$services"; then
    docker run --rm --network none --read-only -v "$config:/c.toml:ro" "$IMAGE" \
      check --config /c.toml --plane public >/dev/null || die "config is not valid for the public plane"
  fi
  return 0
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

key_new() {
  [[ $# -ge 4 && ${1:-} != -* ]] || die "usage: key new ID OUT --chat ALIAS [--chat ALIAS]... [--transcription ALIAS]..."
  local id=$1 out=$2 dir
  shift 2
  [[ ! -e $out ]] || die "$out already exists"
  dir=$(cd -- "$(dirname -- "$out")" && pwd)
  case $dir/ in "$PWD"/*) die 'write keys outside the repository (e.g. ~/.config/kanata/keys/)' ;; esac
  tmp=$(mktemp -d "$dir/.kanata-key.XXXXXX")
  trap 'rm -rf "$tmp"' EXIT
  docker run --rm --network none --read-only --user "$(id -u):$(id -g)" -v "$tmp:/out" \
    "$IMAGE" key new --id "$id" "$@" --key-out /out/key
  mv -n "$tmp/key" "$dir/$(basename -- "$out")"
  rm -rf "$tmp"
  trap - EXIT
  chmod 600 "$dir/$(basename -- "$out")"
  cat >&2 <<EOF

Key written to $dir/$(basename -- "$out") (not printed).
Next: paste the block above into the config, then: scripts/kanata.sh restart
Send the key file's contents to its holder over a private channel.
EOF
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

owner_key_rotate() {
  local restart=1 key_file config key_dir
  case ${1:-} in '') ;; --no-restart) restart=0 ;; *) die "usage: owner-key rotate [--no-restart]" ;; esac
  # Host-side key path: KANATA_OWNER_KEY_FILE from the environment or .env.
  key_file=${KANATA_OWNER_KEY_FILE:-$(sed -n 's/^KANATA_OWNER_KEY_FILE=//p' .env 2>/dev/null | tail -1)}
  key_file=${key_file:-$HOME/.config/kanata/owner-client-key}
  config=$(config_path) || exit 1
  [[ -f $config ]] || die "config not found: $config"
  key_dir=$(dirname -- "$key_file")
  [[ -d $key_dir ]] || die "key directory does not exist: $key_dir"
  case $key_file in "$PWD"/*) die 'the owner key must live outside the repository' ;; esac

  umask 077
  new_key=$(mktemp "$key_dir/.owner-key.XXXXXX")
  new_config=$(mktemp "$(dirname -- "$config")/.config.XXXXXX")
  trap 'rm -f "$new_key" "$new_config"' EXIT
  # 32 random bytes, base64url without padding (43 chars).
  { printf 'kanata_sk_'; head -c 32 /dev/urandom | base64 | tr '+/' '-_' | tr -d '=\n'; printf '\n'; } >"$new_key"

  # Point the single owner block at the new digest.
  python3 - "$new_key" "$config" "$new_config" <<'PY'
import hashlib, re, sys
key_path, config_path, out_path = sys.argv[1:4]
key = open(key_path, "rb").read().rstrip(b"\n")
if not re.fullmatch(rb"kanata_sk_[A-Za-z0-9_-]{43}", key):
    sys.exit("error: key generation failed")
text = open(config_path).read()
blocks = re.split(r"(?m)^(?=\[\[?[^\]\n]+\]\]?\s*$)", text)
owners = [i for i, b in enumerate(blocks) if b.startswith("[[application_keys]]") and re.search(r"(?m)^owner\s*=\s*true\s*$", b)]
if len(owners) != 1:
    sys.exit("error: expected exactly one [[application_keys]] block with owner = true")
i = owners[0]
line = 'secret_ref = "sha256:%s"' % hashlib.sha256(key).hexdigest()
blocks[i], count = re.subn(r'(?m)^secret_ref\s*=.*$', line, blocks[i], count=1)
if count != 1:
    sys.exit("error: owner block has no secret_ref line")
open(out_path, "w").write("".join(blocks))
PY

  check_config "$new_config" >/dev/null || die 'updated config failed kanata check; nothing changed'
  chmod 600 "$new_key"
  chmod 444 "$new_config"
  mv -f "$new_key" "$key_file"
  mv -f "$new_config" "$config"
  trap - EXIT
  echo "New owner key written to $key_file (not printed); owner digest updated in $config."

  if [[ $restart -eq 1 ]]; then
    recreate_kanata
    echo 'Kanata recreated; the previous owner key no longer authenticates.'
  else
    echo 'Not restarted: the old key keeps working until: scripts/kanata.sh restart'
  fi
  cat <<'EOF'

Next:
  - Update every client that uses the owner key (copy it from the key file; don't paste it into chats).
If you suspect a compromise, also:
  - Re-issue other keys: remove their [[application_keys]] blocks, create new ones with `key new`, restart.
  - Codex: `codex logout`, sign out Codex sessions in ChatGPT security settings, then `codex login`.
  - Cloudflare: refresh the tunnel token (replace its file, then `up`), and roll any
    Cloudflare API tokens used by your reverse proxy.
  - Check `scripts/kanata.sh logs` for unfamiliar key ids or client IPs.
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
    [[ ${1:-} == new ]] || die "usage: key new ID OUT --chat ALIAS ..."
    shift
    key_new "$@"
    ;;
  ollama-context) ollama_context "$@" ;;
  fm-serve) fm_serve "$@" ;;
  owner-key)
    [[ ${1:-} == rotate ]] || die "usage: owner-key rotate [--no-restart]"
    shift
    owner_key_rotate "$@"
    ;;
  help | -h | --help) usage ;;
  *) usage >&2; exit 2 ;;
esac
