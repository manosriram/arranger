build:
    cargo build

test:
    cargo test

bench-mem:
    cargo build --release
    scripts/bench-mem.sh
