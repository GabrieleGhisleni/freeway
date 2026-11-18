#!/usr/bin/env bash
# Manage the .env that lives on the Pi (provider keys and ROUTER_TOKENS). Secrets never go through git or GitHub.
#
#   deploy/env.sh edit               open the remote .env in $EDITOR (nano by default), restart on save
#   deploy/env.sh set KEY            read the value from the prompt (hidden) and set KEY, restart
#   deploy/env.sh new-token          append a new random token to ROUTER_TOKENS, print it, restart
#   deploy/env.sh keys               list the variable names set on the Pi (values hidden)
#   deploy/env.sh named-token NAME   add a named token to ROUTER_NAMED_TOKENS (keeps the others), print it once, restart
#   deploy/env.sh names              list the named-token names on the Pi (tokens hidden)
#
# Environment: DEPLOY_HOST (default rpi), DEPLOY_DIR (default freeway), as in deploy.sh.
set -euo pipefail

HOST="${DEPLOY_HOST:-rpi}"
DIR="${DEPLOY_DIR:-freeway}"
ENVFILE="$DIR/.env"
restart() { ssh "$HOST" "sudo systemctl restart freeway && sleep 2 && systemctl is-active freeway"; }

case "${1:-}" in
  keys)
    ssh "$HOST" "sed -E 's/=.+/=<set>/; s/^([A-Z_]+)=\$/\\1=<empty>/' $ENVFILE"
    ;;
  edit)
    ssh -t "$HOST" "\${EDITOR:-nano} $ENVFILE"
    restart
    ;;
  set)
    KEY="${2:?usage: env.sh set KEY}"
    [[ "$KEY" =~ ^[A-Z][A-Z0-9_]*$ ]] || { echo "invalid variable name" >&2; exit 2; }
    read -r -s -p "$KEY: " VALUE; echo
    [[ -n "$VALUE" && "$VALUE" != *$'\n'* ]] || { echo "empty value" >&2; exit 2; }
    # The value travels on stdin, never on a command line (it would show up in `ps`).
    printf '%s\n' "$VALUE" | ssh "$HOST" "read -r V && f=$ENVFILE && { grep -v '^$KEY=' \$f; echo \"$KEY=\$V\"; } > \$f.new && chmod 600 \$f.new && mv \$f.new \$f"
    restart
    ;;
  new-token)
    TOKEN="$(python3 -c 'import secrets; print(secrets.token_urlsafe(32))')"
    printf '%s\n' "$TOKEN" | ssh "$HOST" "read -r T && f=$ENVFILE && cur=\$(grep '^ROUTER_TOKENS=' \$f | cut -d= -f2-) && { grep -v '^ROUTER_TOKENS=' \$f; echo \"ROUTER_TOKENS=\${cur:+\$cur,}\$T\"; } > \$f.new && chmod 600 \$f.new && mv \$f.new \$f"
    restart
    echo "new token (shown once): $TOKEN"
    ;;
  named-token)
    # Adds one name to ROUTER_NAMED_TOKENS, keeping every other name. Refuses to overwrite an existing name.
    NAME="${2:?usage: env.sh named-token NAME}"
    [[ "$NAME" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || { echo "invalid name (letters, digits, . _ -)" >&2; exit 2; }
    TOKEN="$(python3 -c 'import secrets; print(secrets.token_urlsafe(32))')"
    # set -e stops here if ssh fails: an empty read must never be treated as "no names yet".
    CUR="$(ssh "$HOST" "grep '^ROUTER_NAMED_TOKENS=' $ENVFILE || true")"
    LINE="$(printf '%s' "$CUR" | python3 -c '
import json, sys
name, token = sys.argv[1], sys.argv[2]
raw = sys.stdin.read().strip()
body = raw.split("=", 1)[1].strip("\x27") if raw else "{}"
data = json.loads(body)
if name in data:
    sys.exit("name already exists on the Pi; remove it with: deploy/env.sh edit")
data[name] = token
print("ROUTER_NAMED_TOKENS=\x27" + json.dumps(data, separators=(",", ":")) + "\x27")
' "$NAME" "$TOKEN")"
    printf '%s\n' "$LINE" | ssh "$HOST" "read -r L && f=$ENVFILE && { grep -v '^ROUTER_NAMED_TOKENS=' \$f; echo \"\$L\"; } > \$f.new && chmod 600 \$f.new && mv \$f.new \$f"
    restart
    echo "new named token for '$NAME' (shown once): $TOKEN"
    ;;
  names)
    ssh "$HOST" "grep '^ROUTER_NAMED_TOKENS=' $ENVFILE || true" | python3 -c '
import json, sys
raw = sys.stdin.read().strip()
body = raw.split("=", 1)[1].strip("\x27") if raw else "{}"
print("\n".join(json.loads(body)) or "(no named tokens)")
'
    ;;
  *)
    sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
