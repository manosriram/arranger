#!/usr/bin/env bash
# Snapshot of the chat with back-to-back reads grouped into one scrollable window.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -q
bin=$PWD/target/release/arranger
out=${OUT:-$PWD/assets/reads-window.png}
export GIT_AUTHOR_NAME=demo GIT_AUTHOR_EMAIL=demo@example.com GIT_COMMITTER_NAME=demo GIT_COMMITTER_EMAIL=demo@example.com
demo=${DEMO_HOME:-$(mktemp -d)}
mkdir -p "$demo" && demo=$(cd "$demo" && pwd -P)
cleanup() { ARRANGER_HOME=$demo "$bin" stop >/dev/null 2>&1 || true; rm -rf "$demo"; }
trap cleanup EXIT
export ARRANGER_HOME=$demo HOME=$demo
mkdir -p "$demo/bin"
# Our fake stream-json claude, ahead of any real one.
ln -s "$PWD/scripts/fake-reads-claude.py" "$demo/bin/claude"

# A repo with one commit.
mkdir -p "$demo/api/src" "$demo/api/tests"
printf '// src/http.rs\n' > "$demo/api/src/http.rs"
printf '// src/cache.rs\n' > "$demo/api/src/cache.rs"
printf '// tests/server.rs\n' > "$demo/api/tests/server.rs"
(cd "$demo/api" && git init -q -b main && git add -A && git commit -qm init)

export PATH=$demo/bin:$PATH
export CLAUDE_CODE_EXECPATH=
export CLAUDE_IGNORE=

# Start a chat task (headless by default for claude).
(cd "$demo/api" && "$bin" new claude "read the http, cache and server files" >/dev/null)
sleep 2

# Snapshot the selected task's chat, offscreen.
(cd "$demo/api" && ARRANGER_SELECT=1 ARRANGER_SNAPSHOT=$demo/shot.ppm exec "$bin" 2>/dev/null) & local_gui=$!
sleep 6
kill $local_gui 2>/dev/null || true
wait $local_gui 2>/dev/null || true

if command -v ffmpeg >/dev/null 2>&1; then
    ffmpeg -loglevel error -y -i "$demo/shot.ppm" "$out"
else
    sips -s format png "$demo/shot.ppm" --out "$out" >/dev/null
fi
echo "wrote $out"
