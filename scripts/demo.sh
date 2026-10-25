#!/usr/bin/env bash
# README screenshots: a throwaway home with fake agents on this Mac and two fake machines, rendered
# to assets/screenshot.png (light) and assets/screenshot-dark.png.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -q
bin=$PWD/target/release/arranger src=$PWD out=$PWD/assets
export GIT_AUTHOR_NAME=demo GIT_AUTHOR_EMAIL=demo@example.com GIT_COMMITTER_NAME=demo GIT_COMMITTER_EMAIL=demo@example.com

# A repo with one commit: repo <dir> <file>...
repo() {
    mkdir -p "$1" && cd "$1" && git init -q -b main
    shift
    for f in "$@"; do mkdir -p "$(dirname "$f")"; echo "// $f" > "$f"; done
    git add -A && git commit -qm init
}

cleanup() {
    for h in "$demo"/hosts/*; do ARRANGER_HOME=$h "$bin" stop >/dev/null 2>&1 || true; done
    ARRANGER_HOME=$demo "$bin" stop >/dev/null 2>&1 || true
    pkill -f "sleep 86399" 2>/dev/null || true
    rm -rf "$demo"
}

# shot <theme> <file>: a fresh demo home each time, so both themes show the same screen.
shot() {
    demo=${DEMO_HOME:-$(mktemp -d)}
    mkdir -p "$demo" && demo=$(cd "$demo" && pwd -P)
    trap cleanup EXIT
    export ARRANGER_HOME=$demo HOME=$demo # paths read ~/api
    mkdir -p "$demo/bin"
    for a in claude codex gemini; do ln -s "$src/scripts/demo-agent.py" "$demo/bin/$a"; done
    ln -s "$bin" "$demo/bin/arranger"
    # Stand-in for ssh: each machine is a daemon under hosts/<name> here, and a "tunnel" a symlink.
    cat > "$demo/bin/ssh" <<EOF
#!/bin/sh
fwd= host=
while [ \$# -gt 0 ]; do # options come before and after the host
  case \$1 in -o) shift 2 ;; -N) shift ;; -L) fwd=\$2; shift 2 ;; *) [ -z "\$host" ] || break; host=\$1; shift ;; esac
done
export ARRANGER_HOME=$demo/hosts/\$host
if [ -n "\$fwd" ]; then ln -sf "\${fwd#*:}" "\${fwd%%:*}"; exec sleep 86399; fi
exec sh -c "\$*"
EOF
    chmod +x "$demo/bin/ssh"
    local path=$PATH
    export PATH=$demo/bin:$PATH

    (repo "$demo/api" src/cache.rs src/http.rs tests/server.rs README.md
     "$bin" new claude "add retry with backoff to the http client" >/dev/null
     "$bin" new codex "fix the flaky serves_health test" >/dev/null
     "$bin" new gemini "document the config options in the README" >/dev/null
     "$bin" new claude "shard the cache to cut lock contention" >/dev/null)
    (repo "$demo/ml" train.py
     ARRANGER_HOME=$demo/hosts/gpu-box "$bin" new claude "retrain the ranking model on last week's clicks" >/dev/null)
    (repo "$demo/infra" Dockerfile
     ARRANGER_HOME=$demo/hosts/staging "$bin" new codex "bump the base image and redeploy staging" >/dev/null)
    echo '[{"name": "gpu-box", "ssh": "gpu-box", "repo": "~/ml"}, {"name": "staging", "ssh": "staging", "repo": "~/infra"}]' > "$demo/machines.json"
    printf '{"theme": "%s"}' "$1" > "$demo/gui.json"
    sleep 4

    (cd "$demo/api" && ARRANGER_SELECT=2 ARRANGER_SNAPSHOT=$demo/shot.ppm exec "$bin" 2>/dev/null) & local gui=$!
    sleep 6
    kill $gui 2>/dev/null || true
    wait $gui 2>/dev/null || true
    if command -v ffmpeg >/dev/null 2>&1; then
        ffmpeg -loglevel error -y -i "$demo/shot.ppm" "$out/$2"
    else
        sips -s format png "$demo/shot.ppm" --out "$out/$2" >/dev/null
    fi
    echo "wrote $out/$2"
    cleanup
    trap - EXIT
    export PATH=$path
}

shot Paper screenshot.png
shot Graphite screenshot-dark.png
