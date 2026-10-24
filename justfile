build:
    cargo build

test:
    cargo test

bench-mem:
    cargo build --release
    scripts/bench-mem.sh

# arranger.app in target/, from a release build (macOS)
app:
    cargo build --release
    scripts/bundle.sh target/release target

screenshot:
    scripts/demo.sh
