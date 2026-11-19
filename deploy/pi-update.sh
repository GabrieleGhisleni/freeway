#!/usr/bin/env bash
# Runs ON THE PI (via freeway-update.timer). Installs the latest GitHub release if it is new.
# Download and verify first, swap after; if freeway does not answer after the restart, roll back.
# Cheap when nothing changed: one HEAD request to github.com (no API rate limit, no token).
set -euo pipefail

REPO="${FREEWAY_REPO:-GabrieleGhisleni/freeway}"
DIR="${FREEWAY_DIR:-$HOME/freeway}"
ASSET=freeway-aarch64.tar.gz

exec 9>"$DIR/.update.lock"
flock -n 9 || exit 0   # another update is running

url=$(curl -fsS --max-time 20 -o /dev/null -w '%{redirect_url}' "https://github.com/$REPO/releases/latest") || exit 0
TAG="${url##*/}"
[[ "$TAG" == r2[0-9]* ]] || exit 0   # no release yet: the redirect points at /releases

CURRENT=$(cat "$DIR/RELEASE" 2>/dev/null || true)
[[ "$TAG" != "$CURRENT" ]] || exit 0
[[ "$TAG" != "$(cat "$DIR/RELEASE_FAILED" 2>/dev/null || true)" ]] || exit 0   # do not retry a release that failed

echo "updating $CURRENT -> $TAG"
TMP=$(mktemp -d "$DIR/.update.XXXXXX")
trap 'rm -rf "$TMP"' EXIT
base="https://github.com/$REPO/releases/download/$TAG"
curl -fsSL --max-time 300 -o "$TMP/$ASSET" "$base/$ASSET"
curl -fsSL --max-time 30 -o "$TMP/$ASSET.sha256" "$base/$ASSET.sha256"
(cd "$TMP" && sha256sum -c --quiet "$ASSET.sha256")
tar -xzf "$TMP/$ASSET" -C "$TMP" freeway config.yaml
chmod 755 "$TMP/freeway"
[[ "$(file -b "$TMP/freeway")" == *aarch64* ]] || { echo "not an aarch64 binary" >&2; exit 1; }

cp -p "$DIR/freeway" "$DIR/freeway.prev"
cp -p "$DIR/config.yaml" "$DIR/config.yaml.prev"
mv "$TMP/freeway" "$DIR/freeway"
mv "$TMP/config.yaml" "$DIR/config.yaml"
sudo -n systemctl restart freeway

healthy() {
  local code
  for _ in $(seq 1 60); do
    code=$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:4100/health || true)
    [[ "$code" == 401 || "$code" == 200 ]] && return 0
    sleep 1
  done
  return 1
}

if healthy; then
  echo "$TAG" > "$DIR/RELEASE"
  echo "$TAG" > "$DIR/VERSION"
  rm -f "$DIR/RELEASE_FAILED"
  echo "freeway up on $TAG"
else
  echo "freeway did not come up on $TAG: rolling back" >&2
  mv "$DIR/freeway.prev" "$DIR/freeway"
  mv "$DIR/config.yaml.prev" "$DIR/config.yaml"
  sudo -n systemctl restart freeway
  echo "$TAG" > "$DIR/RELEASE_FAILED"
  exit 1
fi
