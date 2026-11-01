#!/usr/bin/env bash
# Live interop test: boots a Paper 26.3 server with the official
# voicechat-bukkit-2.6.27 plugin and runs tools/interop-harness against it.
#
# Environment overrides:
#   SERVER_DIR  working directory containing paper-26.3-*.jar, eula.txt,
#               server.properties (online-mode=false) and plugins/ with the
#               official voicechat bukkit plugin jar (default: ./server
#               next to this repository checkout)
#   JAVA_BIN    java launcher to use; Paper 26.3 requires Java 25+
#               (default: java on PATH)
#
# The Minecraft client part of the test is offline-mode; the server must run
# with online-mode=false for the harness to join.
set -u

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SERVER_DIR="${SERVER_DIR:-$(dirname "$REPO_DIR")/server}"
JAVA_BIN="${JAVA_BIN:-java}"

JAR="$(ls "$SERVER_DIR"/paper-26.3-*.jar 2>/dev/null | head -1)"
if [ -z "$JAR" ]; then
  echo "No paper-26.3-*.jar found in $SERVER_DIR" >&2
  echo "Download one via https://fill.papermc.io/v3/projects/paper/versions/26.3/builds/latest" >&2
  exit 2
fi
if [ ! -f "$SERVER_DIR/eula.txt" ]; then
  echo "eula=true" > "$SERVER_DIR/eula.txt"
fi
mkdir -p "$SERVER_DIR/plugins"

LOG="$SERVER_DIR/interop-run.log"
cd "$SERVER_DIR"

pkill -f "paper-26.3" 2>/dev/null
sleep 1
: > "$LOG"

# A FIFO console keeps Paper's console thread from spinning on a closed stdin.
rm -f console.fifo
mkfifo console.fifo
sleep 100000 > console.fifo &
KEEPER=$!
cleanup() { kill "$SERVER_PID" "$KEEPER" 2>/dev/null; }
trap cleanup EXIT

"$JAVA_BIN" -Xms512M -Xmx1536M -Dvoicechat.debug=true -jar "$JAR" nogui \
  < console.fifo > "$LOG" 2>&1 &
SERVER_PID=$!

for _ in $(seq 1 120); do
  if grep -q "Done (" "$LOG" 2>/dev/null; then break; fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "Server died during startup:" >&2
    tail -20 "$LOG" >&2
    exit 2
  fi
  sleep 1
done
if ! grep -q "Done (" "$LOG"; then
  echo "Server start timeout" >&2
  tail -20 "$LOG" >&2
  exit 2
fi
echo "=== server ready (pid $SERVER_PID) ==="
grep -E "voicechat.*(Enabling|Compatibility|Voice chat server started)" "$LOG" || true

cd "$REPO_DIR"
export PATH="$HOME/.cargo/bin:$PATH"
cargo run -q -p interop-harness
HARNESS_CODE=$?

sleep 3

echo "=== harness exit code: $HARNESS_CODE ==="
echo "=== voicechat server log lines ==="
grep -E "voicechat|Disconnect|joined" "$LOG" | tail -12
exit "$HARNESS_CODE"
