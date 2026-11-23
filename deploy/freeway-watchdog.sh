#!/usr/bin/env bash
# Runs ON THE PI every minute (freeway-watchdog.timer).
# systemd `Restart=always` only reacts when the process EXITS. A hung process, or a tunnel that lost its
# edge connection, stays "active (running)" forever. This probes both and restarts what does not answer.
#
#   freeway   : local /health must answer 200 or 401 (401 = up, no token sent)  -> restart freeway
#   cloudflared: the public URL must answer 200/401 while freeway is fine         -> restart cloudflared
#
# A service is restarted only after FAILS consecutive failed probes, so a short blip does nothing.
set -uo pipefail

PUBLIC_URL="${FREEWAY_PUBLIC_URL:-https://freeway.gabrieleghisleni.dev}"
LOCAL_URL="${FREEWAY_LOCAL_URL:-http://127.0.0.1:4100/health}"
FAILS="${FREEWAY_WATCHDOG_FAILS:-3}"
STATE=/run/freeway-watchdog
mkdir -p "$STATE"

up() { [[ "$(curl -s -m 5 -o /dev/null -w '%{http_code}' "$1")" =~ ^(200|401)$ ]]; }

# bump <name> <ok|fail>: returns 0 when the failure streak just reached FAILS
bump() {
  local f="$STATE/$1" n
  if [[ "$2" == ok ]]; then echo 0 > "$f"; return 1; fi
  n=$(( $(cat "$f" 2>/dev/null || echo 0) + 1 ))
  echo "$n" > "$f"
  (( n >= FAILS ))
}

if up "$LOCAL_URL"; then
  bump freeway ok
  if up "$PUBLIC_URL"; then
    bump tunnel ok
  elif bump tunnel fail; then
    logger -t freeway-watchdog "public URL down $FAILS times while freeway is up: restarting cloudflared"
    systemctl restart cloudflared
    echo 0 > "$STATE/tunnel"
  fi
elif bump freeway fail; then
  logger -t freeway-watchdog "freeway did not answer $FAILS times: restarting"
  systemctl restart freeway
  echo 0 > "$STATE/freeway"
fi

exit 0
