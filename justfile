appid := 'com.rlyeh.vela.Cosmic'
rootdir := ''
prefix := '/usr/local'
base-dir := absolute_path(clean(rootdir / prefix))
bin-dst := base-dir / 'bin'
desktop-dst := base-dir / 'share' / 'applications' / (appid + '.desktop')
metainfo-dst := base-dir / 'share' / 'metainfo' / (appid + '.metainfo.xml')
icon-dst := base-dir / 'share' / 'icons' / 'hicolor' / 'scalable' / 'apps' / (appid + '.svg')

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

# Build optimized release binaries (vela-cosmic + vela-mcp)
build-release:
    cargo build --release -p vela-cosmic -p vela-mcp

# Install release binaries, .desktop entry, AppStream metadata, and icon
install: build-release
    install -Dm0755 target/release/vela-cosmic {{bin-dst}}/vela-cosmic
    install -Dm0755 target/release/vela-mcp {{bin-dst}}/vela-mcp
    install -Dm0644 resources/{{appid}}.desktop {{desktop-dst}}
    install -Dm0644 resources/{{appid}}.metainfo.xml {{metainfo-dst}}
    install -Dm0644 resources/icons/hicolor/scalable/apps/{{appid}}.svg {{icon-dst}}

# Uninstall files installed by `just install`
uninstall:
    rm -f {{bin-dst}}/vela-cosmic {{bin-dst}}/vela-mcp {{desktop-dst}} {{metainfo-dst}} {{icon-dst}}

# Inspect the latest 15 threads in a live Stalwart JMAP mailbox
inspect:
    cargo run -p vela-jmap --example inspect_mailbox

# Capture an isolated headless Wayland screenshot of Vela (passes args to scripts/ui-snapshot.sh)
ui-snapshot *ARGS:
    cargo build -p vela-cosmic
    ./scripts/ui-snapshot.sh {{ARGS}}
