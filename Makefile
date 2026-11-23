# Day-to-day checks for the freeway instance on the Raspberry Pi.
# `make help` lists the targets. Needs the `rpi` host in ~/.ssh/config.
#
# Overrides:  make status HOST=other-host PUBLIC_URL=https://freeway.example.com
HOST       ?= rpi
PUBLIC_URL ?= https://freeway.gabrieleghisleni.dev
DIR        ?= freeway
SINCE      ?= 24 hours ago

# Run on the Pi: first admin token from .env, never printed.
define ADMIN_CURL
T=$$(grep '^ROUTER_TOKENS=' ~/$(DIR)/.env | cut -d= -f2- | cut -d, -f1); curl -s -m 10 -H "Authorization: Bearer $$T"
endef

.DEFAULT_GOAL := help
.PHONY: help up status health public logs errors restarts tunnel net models paused update-log watch

help:  ## list the targets
	@grep -E '^[a-z-]+:.*##' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  make %-11s %s\n", $$1, $$2}'

up:  ## one-line verdict: is it up? (exit 1 when not)
	@code=$$(ssh -o ConnectTimeout=8 $(HOST) "curl -s -m 5 -o /dev/null -w '%{http_code}' http://127.0.0.1:4100/health" 2>/dev/null); \
	pub=$$(curl -s -m 8 -o /dev/null -w '%{http_code}' $(PUBLIC_URL)/health); \
	echo "local(Pi)=$${code:-unreachable}  public=$$pub   (401/200 = up)"; \
	case "$$code:$$pub" in 401:401|200:200|401:200|200:401) exit 0;; *) echo "DOWN"; exit 1;; esac

status:  ## systemd state, restarts, uptime, memory
	@ssh $(HOST) 'systemctl show freeway -p ActiveState,SubState,NRestarts,ActiveEnterTimestamp,MemoryCurrent,Result; \
	  echo; systemctl is-active freeway cloudflared tailscaled; uptime -p; free -m | sed -n 2p'

health:  ## health check from inside the Pi (bypasses the tunnel)
	@ssh $(HOST) "curl -s -m 5 -o /dev/null -w 'local /health -> %{http_code} in %{time_total}s\n' http://127.0.0.1:4100/health"

public:  ## health check through the Cloudflare tunnel (from this PC)
	@curl -s -m 10 -o /dev/null -w 'public /health -> %{http_code} in %{time_total}s\n' $(PUBLIC_URL)/health

logs:  ## follow the live log
	@ssh -t $(HOST) 'journalctl -u freeway -f -o cat'

errors:  ## failed attempts and warnings since SINCE (default 24 hours ago)
	@ssh $(HOST) 'journalctl -u freeway --since "$(SINCE)" --no-pager -o cat | grep -E "failed:|WARN|ERROR|panick" || echo "none"'

restarts:  ## every start/stop/crash of the service in the last 7 days
	@ssh $(HOST) 'journalctl -u freeway --since "7 days ago" --no-pager | grep -E "Started|Stopping|exited|panicked|Scheduled restart|oom|killed" | cut -c1-170'

tunnel:  ## Cloudflare tunnel drops in the last 24 hours
	@ssh $(HOST) 'journalctl -u cloudflared --since "$(SINCE)" --no-pager | grep -E "Connection terminated|Unable|failed to" | cut -c1-30,70-190 | tail -20; echo "total: $$(journalctl -u cloudflared --since "$(SINCE)" --no-pager | grep -c "Connection terminated")"'

net:  ## Wi-Fi/DHCP drops on the Pi in the last 24 hours
	@ssh $(HOST) 'journalctl -u NetworkManager --since "$(SINCE)" --no-pager | grep -E "wlan0.*(disconnected|deauth|connected)" | grep -E "-> (disconnected|completed)" | cut -c1-140 | tail -20'

models:  ## per-provider / per-model success and failure report (from /admin/info)
	@ssh $(HOST) '$(ADMIN_CURL) http://127.0.0.1:4100/admin/info' | python3 -I deploy/analyze.py

paused:  ## endpoints currently in cooldown
	@ssh $(HOST) '$(ADMIN_CURL) http://127.0.0.1:4100/admin/info' | python3 -I -c 'import json,sys,time; d=json.load(sys.stdin); p=d["paused"]; print(*[f"{x}" for x in p], sep="\n") if p else print("nothing paused")'

update-log:  ## last runs of the release updater
	@ssh $(HOST) 'journalctl -u freeway-update --since "7 days ago" --no-pager -o cat | grep -vE "Starting|Finished|Deactivated|Consumed" | tail -20; echo "installed: $$(cat ~/$(DIR)/RELEASE)"'

watch:  ## poll `make up` every 10 s
	@while true; do date +%T | tr '\n' ' '; $(MAKE) -s up || true; sleep 10; done
