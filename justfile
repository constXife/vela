# Run all fast headless tests (core + jmap + flows)
test:
    cargo test --workspace

# Format check and clippy with warnings as errors
lint:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings

# Run the COSMIC desktop shell
run:
    cargo run -p vela-cosmic

# Inspect the latest 15 threads in a live Stalwart JMAP mailbox
inspect:
    cargo run -p vela-jmap --example inspect_mailbox

# Capture an isolated headless Wayland screenshot of Vela (passes args to scripts/ui-snapshot.sh)
ui-snapshot *ARGS:
    cargo build -p vela-cosmic
    ./scripts/ui-snapshot.sh {{ARGS}}
