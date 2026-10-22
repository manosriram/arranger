#!/usr/bin/env bash
# Start the daemon, spawn 20 fake agents through it, print daemon RSS.
set -euo pipefail
cd "$(dirname "$0")/.."
N=20
bin=$PWD/target/release/arranger
agent=$PWD/scripts/fake-agent.py

export ARRANGER_HOME=$(mktemp -d)
trap 'kill $daemon 2>/dev/null || true; rm -rf "$ARRANGER_HOME"' EXIT
git init -q "$ARRANGER_HOME/repo"
git -C "$ARRANGER_HOME/repo" commit -q --allow-empty -m init

$bin serve 2>/dev/null & daemon=$!
cd "$ARRANGER_HOME/repo"
for _ in $(seq $N); do $bin new "$agent" > /dev/null; done
sleep 3

echo "daemon RSS: $(ps -o rss= -p $daemon | tr -d ' ') KB ($($bin ls | grep -c running)/$N fake agents running)"
