#!/usr/bin/env bash
# Manage the .env that lives on the Pi (provider keys and ROUTER_TOKENS). Secrets never go through git or GitHub.
#
#   deploy/env.sh edit               open the remote .env in $EDITOR (nano by default), restart on save
#   deploy/env.sh set KEY            read the value from the prompt (hidden) and set KEY, restart
#   deploy/env.sh new-token          append a new random token to ROUTER_TOKENS, print it, restart
#   deploy/env.sh keys               list the variable names set on the Pi (values hidden)
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
  *)
    sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
