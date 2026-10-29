#!/usr/bin/env bash
# Takes a screenshot of Eloquawk in demo mode on niri (docs/SPEC.md section 12.3).
#
#   scripts/screenshot.sh <name> [demo-actions] [theme]
#
# Builds, starts `eloquawk --demo --theme <theme> --demo-actions "<actions>,shot"`, waits for
# the ELOQUAWK_READY line, captures the window with that title to target/shots/<name>.png,
# prints every ELOQUAWK_EXPECT line and kills the app. Exits non-zero when an expectation
# failed, the app never became ready or the capture failed. Opens a real window: keep runs
# short.
set -euo pipefail

name=${1:?usage: scripts/screenshot.sh <name> [demo-actions] [theme]}
actions=${2:-wait:1500}
theme=${3:-dark}

cd "$(dirname "$0")/.."
cargo build --quiet
mkdir -p target/shots
log=$(mktemp)
shot="$PWD/target/shots/$name.png"
rm -f "$shot"

target/debug/eloquawk --demo --theme "$theme" --demo-actions "$actions,shot" >"$log" 2>&1 &
pid=$!
cleanup() {
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    rm -f "$log"
}
trap cleanup EXIT

title=""
for _ in $(seq 1 200); do
    if line=$(grep -m1 '^ELOQUAWK_READY ' "$log"); then
        title=${line#ELOQUAWK_READY }
        break
    fi
    if ! kill -0 "$pid" 2>/dev/null; then
        break
    fi
    sleep 0.1
done

status=0
grep '^ELOQUAWK_EXPECT ' "$log" || true
if grep -q '^ELOQUAWK_EXPECT fail' "$log"; then
    status=1
fi

if [[ -z "$title" ]]; then
    echo "screenshot: the app never printed ELOQUAWK_READY" >&2
    sed 's/^/  | /' "$log" >&2
    exit 1
fi

# Give the compositor a frame to present the last render.
sleep 0.3
id=$(niri msg -j windows | jq -r --arg t "$title" \
    '.[] | select(.app_id=="korora.tech.Eloquawk" and .title==$t) | .id' | head -1)
if [[ -z "$id" ]]; then
    echo "screenshot: no niri window titled \"$title\"" >&2
    exit 1
fi
niri msg action screenshot-window --id "$id" --path "$shot" --write-to-disk true >/dev/null
for _ in $(seq 1 50); do
    [[ -s "$shot" ]] && break
    sleep 0.1
done
if [[ ! -s "$shot" ]]; then
    echo "screenshot: niri wrote nothing to $shot" >&2
    exit 1
fi
echo "screenshot: $shot"
exit "$status"
