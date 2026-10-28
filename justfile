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

# The plan usage chip and popover, rendered offscreen and checked by pixel (needs a display)
check-usage-ui:
    cargo build
    python3 -I scripts/check-usage-ui.py

# Before a release: do headless claude and codex still send what the interactive ones do?
# Needs both logged in; costs one short request per variant.
parity:
    scripts/parity.py
    scripts/parity.py --agent codex
