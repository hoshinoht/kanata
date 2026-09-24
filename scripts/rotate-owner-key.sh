#!/usr/bin/env bash
# Create or rotate the owner key. The key goes only to a host file (never printed
# or mounted); the config's owner block gets its sha256 digest; Kanata is recreated
# so the old key stops working. Also used for first-time setup.
# usage: scripts/rotate-owner-key.sh [--no-restart]
set -euo pipefail
cd "$(dirname -- "${BASH_SOURCE[0]}")/.."

restart=1
case ${1:-} in
  '') ;;
  --no-restart) restart=0 ;;
  *) echo "usage: $0 [--no-restart]" >&2; exit 2 ;;
esac

# Host-side key path: KANATA_OWNER_KEY_FILE from the environment or .env.
key_file=${KANATA_OWNER_KEY_FILE:-$(sed -n 's/^KANATA_OWNER_KEY_FILE=//p' .env 2>/dev/null | tail -1)}
key_file=${key_file:-$HOME/.config/kanata/owner-client-key}
# Config path exactly as Compose mounts it (honours .env).
config=$(docker compose config --format json | python3 -c '
import json, sys
for v in json.load(sys.stdin)["services"]["kanata"]["volumes"]:
    if v.get("target") == "/etc/kanata/config.toml":
        print(v["source"])')
[[ -f $config ]] || { echo "error: config not found: $config" >&2; exit 1; }
key_dir=$(dirname -- "$key_file")
[[ -d $key_dir ]] || { echo "error: key directory does not exist: $key_dir" >&2; exit 1; }
case $key_file in
  "$PWD"/*) echo 'error: the owner key must live outside the repository' >&2; exit 1 ;;
esac

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

docker run --rm --network none --read-only -v "$new_config:/c.toml:ro" "${KANATA_IMAGE:-kanata:test}" \
  check --config /c.toml >/dev/null || { echo 'error: updated config failed kanata check; nothing changed' >&2; exit 1; }
chmod 444 "$new_key" "$new_config"
mv -f "$new_key" "$key_file"
mv -f "$new_config" "$config"
trap - EXIT
echo "New owner key written to $key_file (not printed); owner digest updated in $config."

if [[ $restart -eq 1 ]]; then
  docker compose up -d --no-build --force-recreate kanata
  echo 'Kanata recreated; the previous owner key no longer authenticates.'
else
  echo 'Not restarted: the old key keeps working until: docker compose up -d --force-recreate kanata'
fi

cat <<'EOF'

Next:
  - Update every client that uses the owner key (copy it from the key file; don't paste it into chats).
If you suspect a compromise, also:
  - Re-issue other keys: remove their [[application_keys]] blocks, create new ones with scripts/new-client-key.sh, restart.
  - Codex: scripts/codex-login.sh logout, sign out Codex sessions in ChatGPT security settings, then log in again.
  - Cloudflare: refresh the tunnel token (then replace its file and `docker compose up -d`), and roll any
    Cloudflare API tokens used by your reverse proxy.
  - Check `docker compose logs kanata` for unfamiliar key ids or client IPs.
EOF
