# Vela

Lightweight native email client for the COSMIC desktop on Linux, built in Rust with `libcosmic` and JMAP (targeting Stalwart).

## Goals

- Calm, three-pane daily triage workflow (mailboxes · thread list · conversation), inspired by Apple Mail.
- Clean separation between the native UI shell and headless mail/protocol/flow crates.
- First-class JMAP integration (RFC 8620 / 8621) so messages stay on the server and coexist seamlessly with Thunderbird.
- Safe, readable HTML email rendering with inline (`cid:`) image support and opt-in external image loading.

## Status

Early prototype stage.
