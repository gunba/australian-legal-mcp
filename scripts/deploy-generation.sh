#!/usr/bin/env bash
# Transfer the locally active immutable generation directly to one Linux VM,
# activate it there, restart the loopback service, and roll back on failure.
set -euo pipefail

HOST="${1:?usage: deploy-generation.sh user@host}"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOCAL_DATA_DIR="${LEGAL_MCP_DATA_DIR:-$REPO_DIR/data/runtime}"
REMOTE_DATA_DIR=/var/lib/australian-legal-mcp
REMOTE_BIN=/usr/local/bin/legal-mcp
REMOTE_SERVICE=legal-mcp.service
MARGIN_BYTES=$((5 * 1024 * 1024 * 1024))

[[ "$HOST" != -* && "$HOST" =~ ^[A-Za-z0-9._@:-]+$ ]] || { echo "unsafe SSH host" >&2; exit 2; }
for command_name in ssh rsync python3; do command -v "$command_name" >/dev/null || { echo "missing $command_name" >&2; exit 2; }; done
remote() {
  command ssh -o ServerAliveInterval=30 -o ServerAliveCountMax=120 "$HOST" "$@"
}

GENERATION="$(<"$LOCAL_DATA_DIR/active-generation")"
[[ "$GENERATION" =~ ^[0-9a-f]{64}$ ]] || { echo "local active-generation is malformed" >&2; exit 2; }
SOURCE="$LOCAL_DATA_DIR/generations/$GENERATION"
[[ -d "$SOURCE" && ! -L "$SOURCE" ]] || { echo "local generation is missing: $SOURCE" >&2; exit 2; }
INCOMING="$REMOTE_DATA_DIR/incoming/$GENERATION"
LOCK_DIR="$REMOTE_DATA_DIR/.deploy-lock"
LOCAL_BYTES="$(du --apparent-size --summarize --block-size=1 "$SOURCE" | awk '{print $1}')"
[[ "$LOCAL_BYTES" =~ ^[0-9]+$ ]] || { echo "could not measure local generation" >&2; exit 2; }

REMOTE_USER="$(remote 'id -un')"
REMOTE_GROUP="$(remote 'id -gn')"
[[ "$REMOTE_USER" =~ ^[A-Za-z_][A-Za-z0-9_-]*$ && "$REMOTE_GROUP" =~ ^[A-Za-z_][A-Za-z0-9_-]*$ ]] || {
  echo "unsafe remote user or group" >&2; exit 2;
}

remote "sudo install -d -o legal-mcp -g legal-mcp -m 0750 '$REMOTE_DATA_DIR' && sudo mkdir '$LOCK_DIR' && sudo sh -c 'printf %s\\n \"$REMOTE_USER $(date -u +%FT%TZ)\" > \"$LOCK_DIR/owner\"'" || {
  echo "another deployment may be active; inspect $HOST:$LOCK_DIR" >&2
  exit 2
}
release_lock() {
  remote "sudo rm -f '$LOCK_DIR/owner'; sudo rmdir '$LOCK_DIR'" >/dev/null 2>&1 || true
}
trap release_lock EXIT
trap 'exit 130' INT TERM

REMOTE_ACTIVE="$(remote "sudo sh -c 'test -f \"$REMOTE_DATA_DIR/active-generation\" && cat \"$REMOTE_DATA_DIR/active-generation\" || true'")"
if [[ -n "$REMOTE_ACTIVE" && ! "$REMOTE_ACTIVE" =~ ^[0-9a-f]{64}$ ]]; then
  echo "remote active-generation is malformed" >&2
  exit 2
fi

if [[ "$REMOTE_ACTIVE" == "$GENERATION" ]]; then
  echo "generation $GENERATION is already active remotely; verifying service"
else
  # Delete old rollback copies only when the current active generation passes
  # strict all-source model/ANN verification. Never trade the last known-good
  # copy for space when the active installation is damaged.
  if [[ -n "$REMOTE_ACTIVE" ]]; then
    if ! remote "sudo -u legal-mcp sh -c 'set -a; . /etc/australian-legal-mcp/legal-mcp.env; set +a; exec /usr/local/bin/legal-mcp verify' >/dev/null"; then
      echo "remote active generation is not strictly valid; refusing destructive pruning" >&2
      exit 1
    fi
    remote "sudo -u legal-mcp env LEGAL_MCP_DATA_DIR='$REMOTE_DATA_DIR' '$REMOTE_BIN' prune-generations --keep-inactive 0 >/dev/null"
  fi

  remote "sudo install -d -o '$REMOTE_USER' -g '$REMOTE_GROUP' -m 0750 '$REMOTE_DATA_DIR/incoming'; if test -d '$INCOMING'; then sudo chown -R '$REMOTE_USER:$REMOTE_GROUP' '$INCOMING'; sudo chmod -R u+rwX '$INCOMING'; else sudo install -d -o '$REMOTE_USER' -g '$REMOTE_GROUP' -m 0750 '$INCOMING'; fi"
  read -r REMOTE_AVAILABLE REMOTE_PARTIAL < <(remote "available=\$(df --output=avail --block-size=1 '$REMOTE_DATA_DIR' | tail -1 | tr -d ' '); partial=\$(du --apparent-size --summarize --block-size=1 '$INCOMING' 2>/dev/null | awk '{print \$1}'); printf '%s %s\\n' \"\$available\" \"\${partial:-0}\"")
  [[ "$REMOTE_AVAILABLE" =~ ^[0-9]+$ && "$REMOTE_PARTIAL" =~ ^[0-9]+$ ]] || { echo "remote free-space probe failed" >&2; exit 2; }
  REMAINING=$(( LOCAL_BYTES > REMOTE_PARTIAL ? LOCAL_BYTES - REMOTE_PARTIAL : 0 ))
  REQUIRED=$(( REMAINING + MARGIN_BYTES ))
  if (( REMOTE_AVAILABLE < REQUIRED )); then
    echo "insufficient remote space: available=$REMOTE_AVAILABLE required=$REQUIRED" >&2
    exit 1
  fi

  rsync -e 'ssh -o ServerAliveInterval=30 -o ServerAliveCountMax=120' -a --delete-delay --partial --info=progress2 "$SOURCE/" "$HOST:$INCOMING/"
  remote "sudo chown -R legal-mcp:legal-mcp '$INCOMING'; sudo chown legal-mcp:legal-mcp '$REMOTE_DATA_DIR/incoming'; sudo chmod u+rwx '$INCOMING'; sudo chmod u+w '$INCOMING/legal.db'"

  ACTIVATION_JSON="$(remote "sudo -u legal-mcp env LEGAL_MCP_DATA_DIR='$REMOTE_DATA_DIR' '$REMOTE_BIN' activate --generation-dir '$INCOMING'")"
  echo "$ACTIVATION_JSON"
  ACTIVATED="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["active_generation"])' <<<"$ACTIVATION_JSON")"
  [[ "$ACTIVATED" == "$GENERATION" ]] || { echo "remote activated an unexpected generation" >&2; exit 1; }
  PREVIOUS="$(python3 -c 'import json,sys; print(json.load(sys.stdin).get("previous_generation") or "")' <<<"$ACTIVATION_JSON")"
fi

remote_ready() {
  local expected="$1"
  remote "for n in \$(seq 1 120); do body=\$(curl -fsS http://127.0.0.1:51235/readyz 2>/dev/null) && printf '%s' \"\$body\" | python3 -c 'import json,sys; x=json.load(sys.stdin); raise SystemExit(0 if x.get(\"status\")==\"ok\" and x.get(\"generation\")==\"$expected\" else 1)' && exit 0; sleep 1; done; exit 1"
}

if ! remote "sudo systemctl restart '$REMOTE_SERVICE'" || ! remote_ready "$GENERATION" || \
  ! remote "sudo -u legal-mcp sh -c 'set -a; . /etc/australian-legal-mcp/legal-mcp.env; set +a; exec /usr/local/bin/legal-mcp verify' | python3 -c 'import json,sys; x=json.load(sys.stdin); raise SystemExit(0 if x.get(\"active_generation\")==\"$GENERATION\" and x.get(\"semantic_search_ready\") is True else 1)'"; then
  echo "new generation failed readiness; rolling back" >&2
  if [[ -n "${PREVIOUS:-}" && "$PREVIOUS" != "$GENERATION" ]]; then
    remote "sudo -u legal-mcp env LEGAL_MCP_DATA_DIR='$REMOTE_DATA_DIR' '$REMOTE_BIN' rollback --generation '$PREVIOUS' >/dev/null; sudo systemctl restart '$REMOTE_SERVICE'"
    remote_ready "$PREVIOUS"
    remote "sudo -u legal-mcp sh -c 'set -a; . /etc/australian-legal-mcp/legal-mcp.env; set +a; exec /usr/local/bin/legal-mcp verify' >/dev/null"
  else
    remote "sudo systemctl stop '$REMOTE_SERVICE'" || true
  fi
  exit 1
fi

echo "deployed and verified generation $GENERATION on $HOST"
