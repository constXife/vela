#!/usr/bin/env bash
# Capture a screenshot of Vela inside an isolated headless Wayland compositor
# (sway + grim + wtype) without touching the user's active desktop session.
#
# Usage:
#   scripts/ui-snapshot.sh [OPTIONS] [OUTPUT_PNG]
#
# Options:
#   --sample         Launch Vela in sample mode (default)
#   --live           Launch Vela with the saved JMAP account
#   --click X,Y      Click at window/output coordinates via sway seat (repeatable)
#   --keys KEYS      Comma-separated text to type via wtype (e.g. "j,e");
#                    "@Name" presses a named key (e.g. "@Down,@Return")
#   --size WxH       Headless output resolution (default: 1280x760)
#   --wait SECS      Extra settle delay before capture (default: 1.0)

set -euo pipefail

MODE="--sample"
CLICKS=()
KEYS=""
SIZE="1280x760"
WAIT_SECS="1.0"
OUT_PATH=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --sample)
            MODE="--sample"
            shift
            ;;
        --live)
            MODE=""
            shift
            ;;
        --click)
            CLICKS+=("${2:?missing value for --click (expected X,Y)}")
            shift 2
            ;;
        --keys)
            KEYS="${2:?missing value for --keys}"
            shift 2
            ;;
        --size)
            SIZE="${2:?missing value for --size}"
            shift 2
            ;;
        --wait)
            WAIT_SECS="${2:?missing value for --wait}"
            shift 2
            ;;
        -h|--help)
            sed -n '2,15p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *)
            OUT_PATH="$1"
            shift
            ;;
    esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${VELA_BIN:-$REPO_ROOT/target/debug/vela-cosmic}"
OUT_PATH="${OUT_PATH:-$REPO_ROOT/target/vela-snapshot.png}"

if [[ ! -x "$BIN" ]]; then
    echo "Building vela-cosmic..." >&2
    cargo build -p vela-cosmic --manifest-path "$REPO_ROOT/Cargo.toml"
fi

mkdir -p "$(dirname "$OUT_PATH")"

TMPDIR="$(mktemp -d -t vela-headless.XXXXXX)"
cleanup() {
    rm -rf "$TMPDIR"
}
trap cleanup EXIT

cat > "$TMPDIR/run_inside.sh" <<EOF
#!/usr/bin/env bash
set -euo pipefail

APP_PID=""
finish() {
    if [[ -n "\$APP_PID" ]]; then
        kill "\$APP_PID" 2>/dev/null || true
        wait "\$APP_PID" 2>/dev/null || true
    fi
    swaymsg exit >/dev/null 2>&1 || true
}
trap finish EXIT

"$BIN" --new-instance $MODE >"$TMPDIR/vela.log" 2>&1 &
APP_PID=\$!

# Wait until Sway reports the Vela window in its tree (up to 15s).
for _ in \$(seq 1 75); do
    if ! kill -0 "\$APP_PID" 2>/dev/null; then
        echo "vela-cosmic exited early; log:" >&2
        cat "$TMPDIR/vela.log" >&2
        exit 1
    fi
    if swaymsg -t get_tree 2>/dev/null | grep -q '"com.rlyeh.vela.Cosmic"'; then
        break
    fi
    sleep 0.2
done

sleep "$WAIT_SECS"

CLICKS_SERIALIZED="${CLICKS[*]:-}"
if [[ -n "\$CLICKS_SERIALIZED" ]]; then
    for pt in \$CLICKS_SERIALIZED; do
        cx="\${pt%,*}"
        cy="\${pt#*,}"
        swaymsg seat seat0 cursor set "\$cx" "\$cy" >/dev/null 2>&1 || true
        sleep 0.1
        wlrctl pointer move 1 1 || true
        sleep 0.1
        wlrctl pointer click left || true
        sleep 0.4
    done
fi

if [[ -n "$KEYS" ]]; then
    swaymsg '[app_id=".*"] focus' >/dev/null 2>&1 || true
    sleep 0.2
    IFS=',' read -ra KEY_LIST <<< "$KEYS"
    for k in "\${KEY_LIST[@]}"; do
        if [[ "\$k" == @* ]]; then
            wtype -s 200 -k "\${k#@}"
        else
            wtype -s 200 -d 100 "\$k"
        fi
        sleep 0.4
    done
    sleep 0.3
fi

grim "$OUT_PATH"
EOF
chmod +x "$TMPDIR/run_inside.sh"

cat > "$TMPDIR/sway.conf" <<EOF
xwayland disable
default_border none
output HEADLESS-1 resolution $SIZE
exec /usr/bin/env bash "$TMPDIR/run_inside.sh"
EOF

# Run sway completely headless without stealing focus or touching the active Wayland display.
env -u WAYLAND_DISPLAY -u DISPLAY -u SWAYSOCK -u I3SOCK \
    WLR_BACKENDS=headless \
    WLR_LIBINPUT_NO_DEVICES=1 \
    sway --unsupported-gpu -c "$TMPDIR/sway.conf" >"$TMPDIR/sway.log" 2>&1 || true

if [[ ! -s "$OUT_PATH" ]]; then
    echo "Failed to capture screenshot at $OUT_PATH" >&2
    echo "--- sway.log ---" >&2
    cat "$TMPDIR/sway.log" >&2 || true
    echo "--- vela.log ---" >&2
    cat "$TMPDIR/vela.log" >&2 || true
    exit 1
fi

echo "$OUT_PATH"
