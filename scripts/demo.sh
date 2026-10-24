#!/usr/bin/env bash
# README screenshot: a throwaway home with fake agents, rendered to assets/screenshot.png.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -q
bin=$PWD/target/release/arranger out=$PWD/assets/screenshot.png

export ARRANGER_HOME=$(cd "$(mktemp -d)" && pwd -P)
export HOME=$ARRANGER_HOME # paths read ~/api
trap '"$bin" stop >/dev/null 2>&1 || true; rm -rf "$ARRANGER_HOME"' EXIT
mkdir -p "$ARRANGER_HOME/bin"
for a in claude codex gemini; do ln -s "$PWD/scripts/demo-agent.py" "$ARRANGER_HOME/bin/$a"; done
export PATH=$ARRANGER_HOME/bin:$PATH

repo=$ARRANGER_HOME/api
mkdir -p "$repo/src" "$repo/tests"
cd "$repo"
git init -q -b main
echo "use std::time::Duration;" > src/http.rs
echo "pub struct Cache;" > src/cache.rs
echo "#[test] fn serves_health() { std::thread::sleep(ms(50)); }" > tests/server.rs
echo "# api" > README.md
git add -A && git -c user.name=demo -c user.email=demo@example.com commit -qm init

"$bin" new claude "add retry with backoff to the http client" >/dev/null
"$bin" new codex "fix the flaky serves_health test" >/dev/null
"$bin" new gemini "document the config options in the README" >/dev/null
"$bin" new claude "shard the cache to cut lock contention" >/dev/null
sleep 4

ARRANGER_SNAPSHOT=$ARRANGER_HOME/shot.ppm "$bin" 2>/dev/null & gui=$!
sleep 4
kill $gui
ffmpeg -loglevel error -y -i "$ARRANGER_HOME/shot.ppm" "$out"
echo "wrote $out"
