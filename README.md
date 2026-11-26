# Vela

Lightweight native email client for the COSMIC desktop on Linux, built in Rust with `libcosmic` and JMAP (RFC 8620 / 8621, targeting Stalwart and Fastmail).

## Features

- **Calm three-pane COSMIC UI** — Mailboxes, thread list, and conversation view with collapsible sidebar (`Ctrl+B` / `\`), draggable pane resize handles, and native keyboard shortcuts (`?`).
- **Multi-window workflow** — Standalone draggable **Vela Settings** window (`Ctrl+,`) and double-click (or right-click → *Open in new window*) to pop out any conversation into a resizable, maximizable window with its own triage toolbar.
- **Readable & selectable messages** — Clean HTML-to-text rendering, opt-in external image loading, collapsible quoted replies, selectable read-only message bodies (`Ctrl+C`, right-click text menu), quick copy button, and interactive attachment downloads to `~/Downloads`.
- **Multi-account JMAP** — Connect multiple JMAP accounts (Basic auth or Bearer API token, presets for Custom JMAP / Fastmail) with credentials stored in the OS Keyring (`oo7` / Secret Service).
- **Encrypted local cache & fast search** — Per-account SQLCipher (AES-256) cache with FTS5 full-text search, incremental JMAP `/changes` sync, optimistic triage with Undo (`Z`), and crypto-shredding on sign out.
- **Built-in MCP server (`vela-mcp`)** — Stdio JSON-RPC 2.0 server (`--mode read` by default, opt-in `--mode triage`) with sanitized output, tool annotations, and `0600` JSONL audit logging.

## Workspace Layout

- `crates/vela-core` — Domain types (`Mailbox`, `ThreadSummary`, `MailMessage`, `Attachment`, …).
- `crates/vela-jmap` — JMAP client, incremental `/changes` sync, blob downloads, multi-account config & OS Keyring store.
- `crates/vela-cache` — SQLCipher AES-256 encrypted local cache & FTS5 search index.
- `crates/vela-flows` — Headless inbox state machine, optimistic triage, LRU conversation cache, HTML sanitization.
- `apps/cosmic` — Native `libcosmic` GUI (`vela-cosmic`).
- `apps/mcp` — Headless MCP server (`vela-mcp`).

## Quick Start

```bash
# Run with Nix dev shell (recommended)
nix develop -c cargo run --release -p vela-cosmic

# Or try without an account using built-in sample data
nix develop -c cargo run --release -p vela-cosmic -- --sample
```

### MCP Server

```bash
# Read-only mode (default)
cargo run --release -p vela-mcp

# Triage mode (enables read/flag/archive/trash mutations)
cargo run --release -p vela-mcp -- --mode triage
```

### Development & Testing

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Capture a headless Wayland UI screenshot without touching your desktop
just ui-snapshot --sample /tmp/vela.png
```

## Footprint

Event-driven rendering via `libcosmic` / `wgpu` keeps idle CPU at **0%** (no background redraw loops). Typical release footprint is roughly **~110 MiB** private heap (**~240–260 MiB** total RSS including shared graphics/font libraries), varying by GPU driver and system theme. Local cache databases live in `~/.cache/vela/` encrypted at rest with per-account keys in the OS Keyring.

## License

MIT — see [LICENSE](LICENSE).
