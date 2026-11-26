// SPDX-License-Identifier: MIT

//! Quick spike tool to inspect real emails in a Stalwart JMAP mailbox.
//!
//! Uses credentials stored in `~/.config/vela/config.toml` + OS Keyring
//! (sign in once via `just run`).

use vela_core::MailboxRole;
use vela_flows::html;
use vela_jmap::{JmapSession, store};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(config) = store::load_saved_jmap_config().await? else {
        eprintln!(
            "No saved account found in ~/.config/vela/config.toml + OS Keyring.\n\
             Run `just run` and sign in once first."
        );
        std::process::exit(1);
    };

    let session = JmapSession::connect(&config).await?;
    let mailboxes = session.list_mailboxes().await?;
    println!("Mailboxes ({}):", mailboxes.len());
    for mb in &mailboxes {
        println!(
            "  - {:<16} role={:?} unread={}/{}",
            mb.name, mb.role, mb.unread_threads, mb.total_threads
        );
    }

    let Some(inbox) = mailboxes
        .iter()
        .find(|m| m.role == MailboxRole::Inbox)
        .or_else(|| mailboxes.first())
    else {
        println!("No mailboxes found.");
        return Ok(());
    };

    let threads = session.list_threads(&inbox.id, 15).await?;
    println!("\nLatest {} threads in {}:", threads.len(), inbox.name);

    let mut rich_count = 0usize;
    let mut remote_img_count = 0usize;
    let mut cid_count = 0usize;
    let mut quoted_count = 0usize;

    for thread in &threads {
        let msgs = session.load_conversation(&thread.id).await?;
        let mut flags = Vec::new();
        for msg in &msgs {
            let prepared = html::prepare_body(msg.text_body.as_deref(), msg.html_body.as_deref());
            if prepared.analysis.is_rich_layout {
                rich_count += 1;
                flags.push("rich-html");
            }
            if prepared.analysis.has_remote_images {
                remote_img_count += 1;
                flags.push("remote-img");
            }
            if !prepared.analysis.cid_refs.is_empty() {
                cid_count += 1;
                flags.push("cid-img");
            }
            if prepared.quoted_text.is_some() {
                quoted_count += 1;
                flags.push("quoted");
            }
        }
        flags.sort_unstable();
        flags.dedup();
        println!(
            "  [{} msgs]{} {}  ({})",
            thread.message_count,
            if thread.unread { " *" } else { "  " },
            thread.subject,
            if flags.is_empty() {
                "plain".to_string()
            } else {
                flags.join(", ")
            }
        );
    }

    println!(
        "\nCorpus summary: rich_html={rich_count}, remote_images={remote_img_count}, cid_inline={cid_count}, foldable_quotes={quoted_count}"
    );
    Ok(())
}
