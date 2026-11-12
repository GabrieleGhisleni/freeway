#!/usr/bin/env bash
# Test, build for the Pi (see build-aarch64.sh), upload, restart the service.
#
#   deploy/deploy.sh                 tests + build + upload + restart
#   deploy/deploy.sh --no-build      reuse the last build
#   deploy/deploy.sh --skip-tests    skip `cargo test`
#   deploy/deploy.sh --force         deploy even from a branch other than main or with uncommitted changes
#
# Environment (all optional):
#   DEPLOY_HOST  ssh host of the Pi            (default: rpi)
#   DEPLOY_DIR   folder on the Pi, from $HOME  (default: freeway)
#
# Uploaded: the binary and config.yaml. Never touched: .env (keys and tokens) and data/.
# Change .env with deploy/env.sh. First-time setup of the Pi: docs/deploy.md.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HOST="${DEPLOY_HOST:-rpi}"
DIR="${DEPLOY_DIR:-freeway}"
BIN="$ROOT/rust/target/aarch64-unknown-linux-gnu/release/freeway"

BUILD=1 TESTS=1 FORCE=0
for arg in "$@"; do
  case "$arg" in
    --no-build) BUILD=0 ;;
    --skip-tests) TESTS=0 ;;
    --force) FORCE=1 ;;
    *) echo "unknown option: $arg (see the header of this script)" >&2; exit 2 ;;
  esac
done

cd "$ROOT"
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
COMMIT="$(git rev-parse --short HEAD)"
[[ -z "$(git status --porcelain)" ]] || COMMIT="$COMMIT+dirty"
if [[ $FORCE -eq 0 ]]; then
  [[ "$BRANCH" == main ]] || { echo "on branch '$BRANCH', not main (use --force)" >&2; exit 1; }
  [[ -z "$(git status --porcelain)" ]] || { echo "uncommitted changes: commit them first (or use --force)" >&2; exit 1; }
fi

[[ $TESTS -eq 0 ]] || cargo test --quiet --manifest-path "$ROOT/rust/Cargo.toml"
[[ $BUILD -eq 0 ]] || "$ROOT/deploy/build-aarch64.sh"
[[ -f "$BIN" ]] || { echo "missing $BIN (run without --no-build)" >&2; exit 1; }

echo "deploying $BRANCH@$COMMIT to $HOST:~/$DIR"
# Upload next to the live files, then rename: the swap is atomic and a failed upload never leaves a half binary.
scp -q "$BIN" "$HOST:$DIR/freeway.new"
scp -q "$ROOT/config.yaml" "$HOST:$DIR/config.yaml"
ssh "$HOST" "chmod 755 $DIR/freeway.new && mv $DIR/freeway.new $DIR/freeway && echo '$BRANCH@$COMMIT' > $DIR/VERSION && sudo systemctl restart freeway"

# Any HTTP answer from localhost (401 without a token) means the server is up.
ssh "$HOST" 'for i in $(seq 1 15); do
  code=$(curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:4100/health || true)
  [ "$code" = 401 ] || [ "$code" = 200 ] && { echo "freeway up (HTTP $code)"; exit 0; }
  sleep 1
done
echo "freeway did not come up; see: journalctl -u freeway -n 30" >&2; exit 1'
