#!/usr/bin/env bash
# Kanata deployment helper: Docker lifecycle, config checks, sign-in and host keys.
# Uses the repo's .env (COMPOSE_FILE, KANATA_CONFIG_FILE, ...) like `docker compose`.
set -euo pipefail
cd "$(dirname -- "${BASH_SOURCE[0]}")/../.."

# Image tag: KANATA_IMAGE from the environment or .env, as Compose resolves it.
IMAGE=${KANATA_IMAGE:-}
if [[ -z $IMAGE && -f .env ]]; then
  IMAGE=$(sed -n 's/^KANATA_IMAGE=//p' .env | tail -1)
fi
IMAGE=${IMAGE:-kanata:test}

usage() {
  cat <<'EOF'
Usage: deploy/scripts/kanata.sh <command> [args]

Lifecycle
  build                      Build the Kanata image
  check                      Validate the live config with the image (offline)
  up                         Check the config, then start the stack
  down                       Stop and remove the stack
  restart                    Recreate Kanata (both containers with the public profile)
  status                     Show containers
  logs [compose logs args]   Follow Kanata logs (default: -f --tail 100 of both Kanata containers)

ChatGPT (host browser sign-in, deploy/compose/compose.kanata.chatgpt.yml)
  chatgpt login|status|logout|models [--profile NAME]
                             Use deployed credentials; --profile for login/logout only

Private dashboard (host `kanata` binary: cargo install --locked --path .)
  portal [--port PORT]        Manage deployed keys at 127.0.0.1 (default port 9091)

Keys (host `kanata` binary: cargo install --locked --path .; changes apply without restart)
  key <new|list|show|edit|rm|rotate|usage> [args]
                             Run `kanata key ...` with --config set to the deployed config

Local models
  ollama-context MODEL TOKENS [NAME]
                             Create an Ollama variant of MODEL with a fixed context window
                             (default NAME: MODEL-<N>k) and print the route to add
  fm-serve install|uninstall|status
                             Run Apple's `fm serve` (macOS 27+) on 127.0.0.1 as a login agent
  host-serve install CONFIG [BIN] | uninstall | restart | status
                             Run a host-native `kanata serve --plane private` as a login agent
                             (macOS launchd, log ~/Library/Logs/kanata-host-serve.log; Linux
                             systemd user unit kanata-host-serve.service, log in the journal;
                             BIN defaults to `kanata` on PATH). Linux user units stop at
                             logout unless lingering is on: loginctl enable-linger "$USER"
EOF
}

die() { echo "error: $*" >&2; exit 1; }

# Deployed paths exactly as Compose mounts them (honours .env): config, keys dir, state dir.
config= keys_dir= state_dir= chatgpt_dir=
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
  chatgpt_dir=$(python3 - "$config" "$keys_dir" "$state_dir" "$model" "$PWD" <<'PY'
import json, os, stat, sys, tomllib
config, keys_dir, state_dir, model, checkout = sys.argv[1:6]
base = os.path.dirname(os.path.realpath(config))
def fail(msg):
    sys.exit("error: " + msg)
if os.path.realpath(keys_dir) != os.path.join(base, "keys"):
    fail(f"KANATA_KEYS_DIR ({keys_dir}) must be {os.path.join(base, 'keys')}")
if os.path.realpath(state_dir) != os.path.join(base, "state"):
    fail(f"KANATA_STATE_DIR ({state_dir}) must be {os.path.join(base, 'state')}")
try:
    with open(config, "rb") as f:
        settings = tomllib.load(f)
        keys = settings.get("keys")
except (OSError, tomllib.TOMLDecodeError) as e:
    fail(f"cannot read {config}: {e}")
if keys is None or keys.get("file") != "keys/keys.toml" or keys.get("usage_dir") != "state":
    fail(f'{config}: Compose requires [keys] file = "keys/keys.toml" and usage_dir = "state"')
auth = settings.get("chatgpt_auth")
if auth is not None:
    path = auth.get("state_dir", "")
    if not isinstance(path, str) or not os.path.isabs(path) or path == "/" or any(ord(c) < 32 or ord(c) == 127 for c in path):
        fail("chatgpt_auth.state_dir must be a private absolute directory")
    if os.path.realpath(path) != path or os.path.normpath(path) != path:
        fail("chatgpt_auth.state_dir must be canonical, without symlinks")
    if os.path.commonpath([path, os.path.realpath(checkout)]) == os.path.realpath(checkout):
        fail("chatgpt_auth.state_dir must be outside the checkout")
    services = json.loads(model)["services"]
    mounts = services["kanata"].get("volumes", [])
    if not any(v.get("type") == "bind" and v.get("source") == path and v.get("target") == path and not v.get("read_only", False) for v in mounts):
        fail("ChatGPT requires deploy/compose/compose.kanata.chatgpt.yml with KANATA_CHATGPT_STATE_DIR matching chatgpt_auth.state_dir")
    for mount in services.get("kanata-public", {}).get("volumes", []):
        for name in ("source", "target"):
            value = mount.get(name, "")
            if os.path.isabs(value) and os.path.commonpath([path, os.path.realpath(value)]) in (path, os.path.realpath(value)):
                fail("ChatGPT credentials must not be mounted in kanata-public")
    if os.path.lexists(path):
        info = os.lstat(path)
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
            fail("ChatGPT state directory must be owned by the host command user with mode 0700")
    print(path)
PY
  ) || exit 1
}

# Missing mount sources would fail `up` (create_host_path: false).
prepare_dirs() {
  local dir
  for dir in "$keys_dir" "$state_dir" "$state_dir/private" "$state_dir/public"; do
    [[ -d $dir ]] || { mkdir -p -- "$dir" && chmod 700 "$dir"; } || die "cannot create $dir"
  done
  if [[ -n $chatgpt_dir && ! -d $chatgpt_dir ]]; then
    mkdir -m 700 -- "$chatgpt_dir" || die "cannot create ChatGPT state directory; create its parent first"
  fi
}

# File-backed Compose secrets of the kanata service, mounted where the container sees them,
# so files read at config load (e.g. an adapter ca_file) resolve during `check`.
secret_mounts() {
  local model
  model=$(docker compose config --format json) || die "cannot read the Compose model; check .env and COMPOSE_FILE"
  python3 -c '
import json, sys
model = json.loads(sys.argv[1])
files = {name: spec.get("file") for name, spec in (model.get("secrets") or {}).items()}
for secret in model["services"]["kanata"].get("secrets") or []:
    source = secret.get("source")
    target = secret.get("target") or source
    path = files.get(source)
    if path:
        print(path + ":/run/secrets/" + target + ":ro")' "$model" ||
    die "cannot read the Compose secrets"
}

check_config() {
  local services mount
  local -a mounts=()
  deploy_paths
  prepare_dirs
  while IFS= read -r mount; do
    [[ -n $mount ]] && mounts+=(-v "$mount")
  done < <(secret_mounts)
  # Config at /c.toml, keys at /keys: the relative keys/keys.toml resolves the same way.
  # Explicit exits: callers use `check_config && …`, which disables set -e here.
  docker run --rm --network none --read-only -v "$config:/c.toml:ro" -v "$keys_dir:/keys:ro" ${mounts[@]+"${mounts[@]}"} "$IMAGE" check --config /c.toml ||
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

portal() {
  [[ $# -eq 0 || ( $# -eq 2 && $1 == --port ) ]] || die "usage: portal [--port PORT]"
  host_kanata
  deploy_paths
  prepare_dirs
  exec kanata portal --config "$config" "$@"
}

chatgpt() {
  local action=${1:-}
  case $action in login | status | logout | models) ;; *) die "usage: chatgpt login|status|logout|models [--profile NAME]" ;; esac
  shift
  if [[ $# -ne 0 ]]; then
    [[ ( $action == login || $action == logout ) && $# -eq 2 && $1 == --profile && -n $2 ]] ||
      die "--profile NAME is supported only for ChatGPT login/logout"
  fi
  host_kanata
  deploy_paths
  [[ -n $chatgpt_dir ]] || die "configure [chatgpt_auth] and deploy/compose/compose.kanata.chatgpt.yml first"
  prepare_dirs
  exec kanata auth chatgpt "$action" --config "$config" "$@"
}

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
Add or update a route in the config, then: deploy/scripts/kanata.sh restart

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

HOST_LABEL=dev.kanata.host-serve
HOST_PLIST=$HOME/Library/LaunchAgents/$HOST_LABEL.plist
HOST_LOG=$HOME/Library/Logs/kanata-host-serve.log
HOST_UNIT=kanata-host-serve.service
HOST_UNIT_FILE=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/$HOST_UNIT

# Host-native gateway for local clients (e.g. loopback upstreams such as CLIProxyAPI).
host_serve() {
  # KANATA_HOST_OS overrides `uname -s` for tests.
  case ${KANATA_HOST_OS:-$(uname -s)} in
    Darwin) host_serve_launchd "$@" ;;
    Linux) host_serve_systemd "$@" ;;
    *) die "host-serve supports macOS (launchd) and Linux (systemd user units)" ;;
  esac
}

# Validates `install CONFIG [BIN]` and checks the config; sets config and bin.
host_serve_target() {
  [[ $# -ge 1 && $# -le 2 ]] || die "usage: host-serve install CONFIG [BIN]"
  [[ $1 == /* && -f $1 ]] || die "CONFIG must be an absolute path to an existing file"
  config=$1
  bin=${2:-$(command -v kanata || true)}
  [[ -n $bin && $bin == /* && -x $bin ]] || die "BIN must be an absolute path to the kanata binary"
  [[ $config != *[\<\>\&\"\\%\$]* && $bin != *[\<\>\&\"\\%\$]* &&
    $config != *$'\n'* && $bin != *$'\n'* ]] ||
    die 'paths must not contain <, >, &, ", \, %, $ or newlines'
  "$bin" check --config "$config" --plane private >/dev/null || die "config failed kanata check"
}

host_serve_launchd() {
  local domain config bin
  domain=gui/$(id -u)
  case ${1:-} in
    install)
      shift
      host_serve_target "$@"
      mkdir -p "$(dirname -- "$HOST_PLIST")" "$(dirname -- "$HOST_LOG")"
      cat >"$HOST_PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$HOST_LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$bin</string><string>serve</string>
    <string>--config</string><string>$config</string>
    <string>--plane</string><string>private</string>
  </array>
  <key>WorkingDirectory</key><string>$(dirname -- "$config")</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>$HOST_LOG</string>
  <key>StandardErrorPath</key><string>$HOST_LOG</string>
</dict>
</plist>
EOF
      launchctl bootout "$domain/$HOST_LABEL" 2>/dev/null || true
      launchctl bootstrap "$domain" "$HOST_PLIST"
      echo "kanata host-serve agent installed ($config); log: $HOST_LOG"
      ;;
    uninstall)
      launchctl bootout "$domain/$HOST_LABEL" 2>/dev/null || true
      rm -f "$HOST_PLIST"
      echo "kanata host-serve agent removed."
      ;;
    restart)
      launchctl kickstart -k "$domain/$HOST_LABEL" || die "agent not loaded; run host-serve install first"
      ;;
    status)
      if launchctl print "$domain/$HOST_LABEL" >/dev/null 2>&1; then echo "agent: loaded"; else echo "agent: not loaded"; fi
      [[ -f $HOST_PLIST ]] || return 0
      bin=$(/usr/libexec/PlistBuddy -c 'Print :ProgramArguments:0' "$HOST_PLIST")
      config=$(/usr/libexec/PlistBuddy -c 'Print :ProgramArguments:3' "$HOST_PLIST")
      if "$bin" health --config "$config" >/dev/null 2>&1; then echo "health: ok ($config)"; else echo "health: failing ($config)"; fi
      ;;
    *) die "usage: host-serve install CONFIG [BIN] | uninstall | restart | status" ;;
  esac
}

host_serve_systemd() {
  local config bin
  case ${1:-} in
    install)
      shift
      host_serve_target "$@"
      mkdir -p "$(dirname -- "$HOST_UNIT_FILE")"
      cat >"$HOST_UNIT_FILE" <<EOF
[Unit]
Description=Kanata host-native gateway (private plane)

[Service]
ExecStart="$bin" serve --config "$config" --plane private
WorkingDirectory=$(dirname -- "$config")
Restart=always
RestartSec=10
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=default.target
EOF
      systemctl --user daemon-reload
      systemctl --user enable "$HOST_UNIT"
      # Restart, not --now, so a reinstall applies the new unit.
      systemctl --user restart "$HOST_UNIT"
      echo "kanata host-serve unit installed ($config); log: journalctl --user -u $HOST_UNIT"
      echo "User units stop at logout; to keep it running: loginctl enable-linger \"\$USER\""
      ;;
    uninstall)
      systemctl --user disable --now "$HOST_UNIT" 2>/dev/null || true
      rm -f "$HOST_UNIT_FILE"
      systemctl --user daemon-reload
      echo "kanata host-serve unit removed."
      ;;
    restart)
      [[ -f $HOST_UNIT_FILE ]] || die "unit not installed; run host-serve install first"
      systemctl --user restart "$HOST_UNIT"
      ;;
    status)
      if systemctl --user is-active --quiet "$HOST_UNIT"; then echo "service: active"; else echo "service: inactive"; fi
      [[ -f $HOST_UNIT_FILE ]] || return 0
      bin=$(sed -n 's/^ExecStart="\([^"]*\)" serve .*/\1/p' "$HOST_UNIT_FILE")
      config=$(sed -n 's/^ExecStart=.* --config "\([^"]*\)" --plane private$/\1/p' "$HOST_UNIT_FILE")
      if "$bin" health --config "$config" >/dev/null 2>&1; then echo "health: ok ($config)"; else echo "health: failing ($config)"; fi
      ;;
    *) die "usage: host-serve install CONFIG [BIN] | uninstall | restart | status" ;;
  esac
}

command=${1:-help}
[[ $# -eq 0 ]] || shift
case $command in
  build) docker build -f deploy/docker/Dockerfile -t "$IMAGE" . ;;
  check) check_config ;;
  up) check_config && docker compose up -d --no-build ;;
  down) docker compose down ;;
  restart) check_config && recreate_kanata ;;
  status) docker compose ps ;;
  logs)
    if [[ $# -eq 0 ]]; then
      services=$(kanata_services) || exit 1
      # shellcheck disable=SC2086
      docker compose logs -f --tail 100 $services
    else
      docker compose logs "$@"
    fi
    ;;
  chatgpt) chatgpt "$@" ;;
  portal) portal "$@" ;;
  key)
    [[ $# -gt 0 ]] || die "usage: key <new|list|show|edit|rm|rotate|usage> [args] (see: kanata key --help)"
    key_passthrough "$@"
    ;;
  ollama-context) ollama_context "$@" ;;
  fm-serve) fm_serve "$@" ;;
  host-serve) host_serve "$@" ;;
  help | -h | --help) usage ;;
  *) usage >&2; exit 2 ;;
esac
