// SPDX-License-Identifier: MIT

//! Encrypted local SQLite (SQLCipher) cache for Vela.
//!
//! Stores mailboxes, per-mailbox thread lists, loaded message bodies, and an
//! FTS5 search index in `~/.cache/vela/cache.db` (`0600` inside `0700` dir)
//! encrypted with a 256-bit key stored in the OS Keyring (`oo7`).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};
use vela_core::{
    Attachment, EmailAddress, Mailbox, MailboxId, MailboxRole, Message, MessageId, ThreadId,
    ThreadSummary, VelaError,
};
use zeroize::Zeroizing;

/// Bump on any schema change: a cache with another version is dropped and
/// rebuilt from the server (it holds no data the server does not have).
const SCHEMA_VERSION: &str = "2";

/// Thread-safe handle to the local SQLCipher cache database.
pub struct VelaCache {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl std::fmt::Debug for VelaCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VelaCache")
            .field("path", &self.path)
            .finish()
    }
}

/// Default cache database path (`~/.cache/vela/cache.db`).
pub fn default_cache_path() -> Result<PathBuf, VelaError> {
    let base = dirs::cache_dir()
        .ok_or_else(|| VelaError::Other("unable to determine XDG cache directory".into()))?;
    Ok(base.join("vela").join("cache.db"))
}

/// Per-account cache database path (`~/.cache/vela/cache-<safe_id>.db`).
pub fn cache_path_for_account(account_id: &str) -> Result<PathBuf, VelaError> {
    let base = dirs::cache_dir()
        .ok_or_else(|| VelaError::Other("unable to determine XDG cache directory".into()))?;
    let safe_id: String = account_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe_id = if safe_id.is_empty() {
        "default".to_string()
    } else {
        safe_id
    };
    Ok(base.join("vela").join(format!("cache-{safe_id}.db")))
}

/// Remove the default cache database and its WAL/SHM sidecar files (used on Sign out).
pub fn destroy_default_cache() -> Result<(), VelaError> {
    let path = default_cache_path()?;
    remove_db_files(&path);
    Ok(())
}

/// Remove a specific account's cache database and its WAL/SHM sidecar files.
pub fn destroy_account_cache(account_id: &str) -> Result<(), VelaError> {
    let path = cache_path_for_account(account_id)?;
    remove_db_files(&path);
    Ok(())
}

impl VelaCache {
    /// Open or create the encrypted cache at `~/.cache/vela/cache.db`.
    pub fn open_default(hex_key: &str) -> Result<Self, VelaError> {
        let path = default_cache_path()?;
        Self::open_at(path, hex_key)
    }

    /// Open or create an encrypted per-account cache at `~/.cache/vela/cache-<id>.db`.
    pub fn open_for_account(account_id: &str, hex_key: &str) -> Result<Self, VelaError> {
        let path = cache_path_for_account(account_id)?;
        Self::open_at(path, hex_key)
    }

    /// Open or create an encrypted SQLCipher cache at `path`.
    ///
    /// `hex_key` must be a 64-character hex string (256-bit raw key).
    /// If the existing file cannot be decrypted with `hex_key` or is corrupt,
    /// it is wiped and rebuilt cleanly from scratch.
    pub fn open_at(path: PathBuf, hex_key: &str) -> Result<Self, VelaError> {
        validate_hex_key(hex_key)?;
        ensure_parent_dir(&path)?;

        let conn = match open_and_verify(&path, hex_key) {
            Ok(conn) => conn,
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "encrypted cache unreadable or key changed; rebuilding fresh cache"
                );
                remove_db_files(&path);
                open_and_verify(&path, hex_key)?
            }
        };

        enforce_owner_only_file(&path);
        init_schema(&conn)?;

        Ok(Self {
            conn: Mutex::new(conn),
            path,
        })
    }

    /// Persist the full mailbox list atomically.
    pub fn save_mailboxes(&self, mailboxes: &[Mailbox]) -> Result<(), VelaError> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction().map_err(db_err)?;
        tx.execute("DELETE FROM mailboxes", []).map_err(db_err)?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO mailboxes (id, name, role, parent_id, unread_threads, total_threads, sort_rank)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .map_err(db_err)?;
            for mb in mailboxes {
                stmt.execute(params![
                    mb.id.0,
                    mb.name,
                    role_to_str(mb.role),
                    mb.parent_id.as_ref().map(|p| p.0.as_str()),
                    mb.unread_threads,
                    mb.total_threads,
                    i64::from(mb.role.sort_rank()),
                ])
                .map_err(db_err)?;
            }
        }
        tx.commit().map_err(db_err)?;
        Ok(())
    }

    /// Load cached mailboxes sorted by canonical JMAP role rank, then name.
    pub fn load_mailboxes(&self) -> Result<Vec<Mailbox>, VelaError> {
        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, name, role, parent_id, unread_threads, total_threads
                 FROM mailboxes
                 ORDER BY sort_rank ASC, lower(name) ASC",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |row| {
                let role_str: String = row.get(2)?;
                let parent: Option<String> = row.get(3)?;
                Ok(Mailbox {
                    id: MailboxId(row.get(0)?),
                    name: row.get(1)?,
                    role: role_from_str(&role_str),
                    parent_id: parent.map(MailboxId),
                    unread_threads: row.get(4)?,
                    total_threads: row.get(5)?,
                })
            })
            .map_err(db_err)?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(db_err)?);
        }
        Ok(out)
    }

    /// Replace the cached thread list for a given mailbox and update the FTS5 index.
    pub fn save_threads(
        &self,
        mailbox_id: &MailboxId,
        threads: &[ThreadSummary],
    ) -> Result<(), VelaError> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction().map_err(db_err)?;
        tx.execute(
            "DELETE FROM threads WHERE mailbox_id = ?1",
            params![mailbox_id.0],
        )
        .map_err(db_err)?;

        {
            let mut insert_thread = tx
                .prepare(
                    "INSERT OR REPLACE INTO threads
                     (id, mailbox_id, position, subject, snippet, senders_json, recipients_json,
                      received_at, message_count, unread, flagged, has_attachments, mailbox_ids_json)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                )
                .map_err(db_err)?;
            let mut delete_fts = tx
                .prepare("DELETE FROM messages_fts WHERE thread_id = ?1")
                .map_err(db_err)?;
            let mut insert_fts = tx
                .prepare(
                    "INSERT INTO messages_fts (thread_id, subject, senders, body)
                     VALUES (?1, ?2, ?3, ?4)",
                )
                .map_err(db_err)?;

            for (pos, thread) in threads.iter().enumerate() {
                let senders_json = serde_json::to_string(&thread.senders).map_err(json_err)?;
                let recipients_json =
                    serde_json::to_string(&thread.recipients).map_err(json_err)?;
                let mailbox_ids_json =
                    serde_json::to_string(&thread.mailbox_ids).map_err(json_err)?;
                insert_thread
                    .execute(params![
                        thread.id.0,
                        mailbox_id.0,
                        pos as i64,
                        thread.subject,
                        thread.snippet,
                        senders_json,
                        recipients_json,
                        thread.received_at.as_second(),
                        thread.message_count,
                        i64::from(thread.unread),
                        i64::from(thread.flagged),
                        i64::from(thread.has_attachments),
                        mailbox_ids_json,
                    ])
                    .map_err(db_err)?;

                let senders_text = thread
                    .senders
                    .iter()
                    .map(|s| format!("{} {}", s.display_name(), s.email))
                    .collect::<Vec<_>>()
                    .join(" ");
                delete_fts.execute(params![thread.id.0]).map_err(db_err)?;
                insert_fts
                    .execute(params![
                        thread.id.0,
                        thread.subject,
                        senders_text,
                        thread.snippet
                    ])
                    .map_err(db_err)?;
            }
        }

        tx.commit().map_err(db_err)?;
        Ok(())
    }

    /// Load cached thread summaries for a mailbox in display order.
    pub fn load_threads(&self, mailbox_id: &MailboxId) -> Result<Vec<ThreadSummary>, VelaError> {
        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, subject, snippet, senders_json, recipients_json, received_at,
                        message_count, unread, flagged, has_attachments, mailbox_ids_json
                 FROM threads
                 WHERE mailbox_id = ?1
                 ORDER BY position ASC",
            )
            .map_err(db_err)?;

        let rows = stmt
            .query_map(params![mailbox_id.0], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, u32>(6)?,
                    row.get::<_, i64>(7)? != 0,
                    row.get::<_, i64>(8)? != 0,
                    row.get::<_, i64>(9)? != 0,
                    row.get::<_, String>(10)?,
                ))
            })
            .map_err(db_err)?;

        let mut out = Vec::new();
        for row in rows {
            let (
                id,
                subject,
                snippet,
                senders_json,
                recipients_json,
                received_secs,
                message_count,
                unread,
                flagged,
                has_attachments,
                mailbox_ids_json,
            ) = row.map_err(db_err)?;
            let senders: Vec<EmailAddress> =
                serde_json::from_str(&senders_json).map_err(json_err)?;
            let recipients: Vec<EmailAddress> =
                serde_json::from_str(&recipients_json).map_err(json_err)?;
            let mailbox_ids: Vec<MailboxId> =
                serde_json::from_str(&mailbox_ids_json).map_err(json_err)?;
            let received_at =
                Timestamp::from_second(received_secs).unwrap_or(Timestamp::UNIX_EPOCH);
            out.push(ThreadSummary {
                id: ThreadId(id),
                subject,
                snippet,
                senders,
                recipients,
                received_at,
                message_count,
                unread,
                flagged,
                has_attachments,
                mailbox_ids,
            });
        }
        Ok(out)
    }

    /// Persist all messages for a conversation thread and refresh its FTS5 entry.
    pub fn save_conversation(
        &self,
        thread_id: &ThreadId,
        messages: &[Message],
    ) -> Result<(), VelaError> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction().map_err(db_err)?;
        tx.execute(
            "DELETE FROM messages WHERE thread_id = ?1",
            params![thread_id.0],
        )
        .map_err(db_err)?;

        let mut combined_subject = String::new();
        let mut combined_senders = String::new();
        let mut combined_body = String::new();

        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO messages
                     (id, thread_id, position, subject, from_json, to_json, cc_json,
                      received_at, unread, text_body, html_body, attachments_json, has_remote_images)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                )
                .map_err(db_err)?;

            for (pos, msg) in messages.iter().enumerate() {
                let from_json = serde_json::to_string(&msg.from).map_err(json_err)?;
                let to_json = serde_json::to_string(&msg.to).map_err(json_err)?;
                let cc_json = serde_json::to_string(&msg.cc).map_err(json_err)?;
                let attachments_json = serde_json::to_string(&msg.attachments).map_err(json_err)?;

                stmt.execute(params![
                    msg.id.0,
                    thread_id.0,
                    pos as i64,
                    msg.subject,
                    from_json,
                    to_json,
                    cc_json,
                    msg.received_at.as_second(),
                    i64::from(msg.unread),
                    msg.text_body,
                    msg.html_body,
                    attachments_json,
                    i64::from(msg.has_remote_images),
                ])
                .map_err(db_err)?;

                if combined_subject.is_empty() {
                    combined_subject.push_str(&msg.subject);
                }
                for addr in &msg.from {
                    combined_senders.push_str(addr.display_name());
                    combined_senders.push(' ');
                    combined_senders.push_str(&addr.email);
                    combined_senders.push(' ');
                }
                if let Some(text) = &msg.text_body {
                    combined_body.push_str(text);
                    combined_body.push('\n');
                }
            }
        }

        if !messages.is_empty() {
            tx.execute(
                "DELETE FROM messages_fts WHERE thread_id = ?1",
                params![thread_id.0],
            )
            .map_err(db_err)?;
            tx.execute(
                "INSERT INTO messages_fts (thread_id, subject, senders, body)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    thread_id.0,
                    combined_subject,
                    combined_senders,
                    combined_body
                ],
            )
            .map_err(db_err)?;
        }

        tx.commit().map_err(db_err)?;
        Ok(())
    }

    /// Load cached messages for a conversation thread if present.
    pub fn load_conversation(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Option<Vec<Message>>, VelaError> {
        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, subject, from_json, to_json, cc_json, received_at, unread,
                        text_body, html_body, attachments_json, has_remote_images
                 FROM messages
                 WHERE thread_id = ?1
                 ORDER BY position ASC",
            )
            .map_err(db_err)?;

        let rows = stmt
            .query_map(params![thread_id.0], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)? != 0,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, i64>(10)? != 0,
                ))
            })
            .map_err(db_err)?;

        let mut out = Vec::new();
        for row in rows {
            let (
                id,
                subject,
                from_json,
                to_json,
                cc_json,
                received_secs,
                unread,
                text_body,
                html_body,
                attachments_json,
                has_remote_images,
            ) = row.map_err(db_err)?;
            let from: Vec<EmailAddress> = serde_json::from_str(&from_json).map_err(json_err)?;
            let to: Vec<EmailAddress> = serde_json::from_str(&to_json).map_err(json_err)?;
            let cc: Vec<EmailAddress> = serde_json::from_str(&cc_json).map_err(json_err)?;
            let attachments: Vec<Attachment> =
                serde_json::from_str(&attachments_json).map_err(json_err)?;
            let received_at =
                Timestamp::from_second(received_secs).unwrap_or(Timestamp::UNIX_EPOCH);
            out.push(Message {
                id: MessageId(id),
                thread_id: thread_id.clone(),
                subject,
                from,
                to,
                cc,
                received_at,
                unread,
                text_body,
                html_body,
                attachments,
                has_remote_images,
            });
        }

        if out.is_empty() {
            Ok(None)
        } else {
            Ok(Some(out))
        }
    }

    /// Full-text search across cached threads and message bodies using FTS5.
    pub fn search_threads(&self, query: &str, limit: usize) -> Result<Vec<ThreadId>, VelaError> {
        let trimmed = query.trim();
        if trimmed.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        // Quote tokens as prefix phrases so punctuation like `@` or `-` never
        // causes FTS5 syntax errors.
        let fts_query = trimmed
            .split_whitespace()
            .map(|tok| format!("\"{}\"*", tok.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" ");

        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT thread_id
                 FROM messages_fts
                 WHERE messages_fts MATCH ?1
                 ORDER BY rank
                 LIMIT ?2",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![fts_query, limit as i64], |row| {
                row.get::<_, String>(0)
            })
            .map_err(db_err)?;

        let mut out = Vec::new();
        for row in rows {
            out.push(ThreadId(row.map_err(db_err)?));
        }
        Ok(out)
    }

    /// Read a stored JMAP sync state token (e.g. `mailbox_state`, `email_state`).
    pub fn get_sync_state(&self, key: &str) -> Result<Option<String>, VelaError> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(db_err)
    }

    /// Persist a JMAP sync state token.
    pub fn set_sync_state(&self, key: &str, value: &str) -> Result<(), VelaError> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![key, value],
        )
        .map_err(db_err)?;
        Ok(())
    }

    /// Selectively evict cached messages and FTS entries for threads reported
    /// as updated or destroyed by JMAP `Thread/changes`.
    pub fn invalidate_threads(&self, thread_ids: &[ThreadId]) -> Result<(), VelaError> {
        if thread_ids.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction().map_err(db_err)?;
        {
            let mut del_msgs = tx
                .prepare("DELETE FROM messages WHERE thread_id = ?1")
                .map_err(db_err)?;
            let mut del_fts = tx
                .prepare("DELETE FROM messages_fts WHERE thread_id = ?1")
                .map_err(db_err)?;
            for tid in thread_ids {
                del_msgs.execute(params![tid.0]).map_err(db_err)?;
                del_fts.execute(params![tid.0]).map_err(db_err)?;
            }
        }
        tx.commit().map_err(db_err)?;
        Ok(())
    }

    fn lock_conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>, VelaError> {
        self.conn
            .lock()
            .map_err(|_| VelaError::Other("cache mutex poisoned".into()))
    }
}

fn validate_hex_key(hex_key: &str) -> Result<(), VelaError> {
    if hex_key.len() != 64 || !hex_key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(VelaError::Auth(
            "cache encryption key must be 64 hex characters (256-bit)".into(),
        ));
    }
    Ok(())
}

fn open_and_verify(path: &Path, hex_key: &str) -> Result<Connection, VelaError> {
    let conn = Connection::open(path).map_err(db_err)?;
    let pragma = Zeroizing::new(format!("PRAGMA key = \"x'{hex_key}'\";"));
    conn.execute_batch(&pragma).map_err(db_err)?;
    // Force SQLCipher to read page 1 and verify the key / HMAC immediately.
    let _: i64 = conn
        .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
        .map_err(db_err)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;",
    )
    .map_err(db_err)?;
    Ok(conn)
}

fn init_schema(conn: &Connection) -> Result<(), VelaError> {
    drop_outdated_schema(conn)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (
             key   TEXT PRIMARY KEY,
             value TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS mailboxes (
             id             TEXT PRIMARY KEY,
             name           TEXT NOT NULL,
             role           TEXT NOT NULL,
             parent_id      TEXT,
             unread_threads INTEGER NOT NULL,
             total_threads  INTEGER NOT NULL,
             sort_rank      INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS threads (
             id               TEXT NOT NULL,
             mailbox_id       TEXT NOT NULL,
             position         INTEGER NOT NULL,
             subject          TEXT NOT NULL,
             snippet          TEXT NOT NULL,
             senders_json     TEXT NOT NULL,
             recipients_json  TEXT NOT NULL,
             received_at      INTEGER NOT NULL,
             message_count    INTEGER NOT NULL,
             unread           INTEGER NOT NULL,
             flagged          INTEGER NOT NULL,
             has_attachments  INTEGER NOT NULL,
             mailbox_ids_json TEXT NOT NULL,
             PRIMARY KEY (mailbox_id, id)
         );
         CREATE INDEX IF NOT EXISTS idx_threads_mailbox_pos
             ON threads (mailbox_id, position);
         CREATE TABLE IF NOT EXISTS messages (
             id                TEXT PRIMARY KEY,
             thread_id         TEXT NOT NULL,
             position          INTEGER NOT NULL,
             subject           TEXT NOT NULL,
             from_json         TEXT NOT NULL,
             to_json           TEXT NOT NULL,
             cc_json           TEXT NOT NULL,
             received_at       INTEGER NOT NULL,
             unread            INTEGER NOT NULL,
             text_body         TEXT,
             html_body         TEXT,
             attachments_json  TEXT NOT NULL,
             has_remote_images INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_messages_thread_pos
             ON messages (thread_id, position);
         CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
             thread_id UNINDEXED,
             subject,
             senders,
             body,
             tokenize='unicode61'
         );",
    )
    .map_err(db_err)?;

    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
        params![SCHEMA_VERSION],
    )
    .map_err(db_err)?;
    Ok(())
}

/// Drop every table (including sync state tokens) when the stored schema
/// version differs, so the next sync refetches everything from the server.
fn drop_outdated_schema(conn: &Connection) -> Result<(), VelaError> {
    let has_meta: bool = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')",
            [],
            |row| row.get(0),
        )
        .map_err(db_err)?;
    if !has_meta {
        return Ok(());
    }
    let version: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(db_err)?;
    if version.as_deref() == Some(SCHEMA_VERSION) {
        return Ok(());
    }
    tracing::info!(
        from = version.as_deref().unwrap_or("none"),
        to = SCHEMA_VERSION,
        "cache schema changed; rebuilding local cache"
    );
    conn.execute_batch(
        "DROP TABLE IF EXISTS messages_fts;
         DROP TABLE IF EXISTS messages;
         DROP TABLE IF EXISTS threads;
         DROP TABLE IF EXISTS mailboxes;
         DROP TABLE IF EXISTS meta;",
    )
    .map_err(db_err)
}

fn ensure_parent_dir(path: &Path) -> Result<(), VelaError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            VelaError::Other(format!("failed to create {}: {err}", parent.display()))
        })?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(|err| {
            VelaError::Other(format!(
                "failed to set permissions on {}: {err}",
                parent.display()
            ))
        })?;
    }
    Ok(())
}

fn enforce_owner_only_file(path: &Path) {
    if path.exists() {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
}

fn remove_db_files(path: &Path) {
    let _ = fs::remove_file(path);
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    let _ = fs::remove_file(wal);
    let mut shm = path.as_os_str().to_os_string();
    shm.push("-shm");
    let _ = fs::remove_file(shm);
}

fn role_to_str(role: MailboxRole) -> &'static str {
    match role {
        MailboxRole::Inbox => "inbox",
        MailboxRole::Archive => "archive",
        MailboxRole::Drafts => "drafts",
        MailboxRole::Sent => "sent",
        MailboxRole::Trash => "trash",
        MailboxRole::Junk => "junk",
        MailboxRole::Custom => "custom",
    }
}

fn role_from_str(s: &str) -> MailboxRole {
    MailboxRole::from_jmap_role(Some(s))
}

fn db_err(err: rusqlite::Error) -> VelaError {
    VelaError::Other(format!("cache database error: {err}"))
}

fn json_err(err: serde_json::Error) -> VelaError {
    VelaError::Other(format!("cache serialization error: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const KEY_B: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    fn temp_db_path(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("vela-cache-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("cache.db")
    }

    #[test]
    fn roundtrip_and_encryption_on_disk() {
        let path = temp_db_path("roundtrip");
        let cache = VelaCache::open_at(path.clone(), KEY_A).expect("open encrypted cache");

        let inbox_id = MailboxId("mb-inbox".into());
        let mailboxes = vec![Mailbox {
            id: inbox_id.clone(),
            name: "Inbox".into(),
            role: MailboxRole::Inbox,
            parent_id: None,
            unread_threads: 1,
            total_threads: 1,
        }];
        cache.save_mailboxes(&mailboxes).unwrap();
        assert_eq!(cache.load_mailboxes().unwrap(), mailboxes);

        let thread_id = ThreadId("t-secret".into());
        let secret_marker = "SUPER_SECRET_INVOICE_99421";
        let threads = vec![ThreadSummary {
            id: thread_id.clone(),
            subject: format!("Receipt {secret_marker}"),
            snippet: "Encrypted preview".into(),
            senders: vec![EmailAddress {
                name: Some("Billing".into()),
                email: "billing@example.com".into(),
            }],
            recipients: vec![EmailAddress {
                name: None,
                email: "you@example.com".into(),
            }],
            received_at: Timestamp::from_second(1_700_000_000).unwrap(),
            message_count: 1,
            unread: true,
            flagged: false,
            has_attachments: false,
            mailbox_ids: vec![inbox_id.clone()],
        }];
        cache.save_threads(&inbox_id, &threads).unwrap();
        assert_eq!(cache.load_threads(&inbox_id).unwrap(), threads);

        let messages = vec![Message {
            id: MessageId("m-1".into()),
            thread_id: thread_id.clone(),
            subject: format!("Receipt {secret_marker}"),
            from: vec![EmailAddress {
                name: Some("Billing".into()),
                email: "billing@example.com".into(),
            }],
            to: vec![EmailAddress {
                name: Some("You".into()),
                email: "you@example.com".into(),
            }],
            cc: vec![],
            received_at: Timestamp::from_second(1_700_000_000).unwrap(),
            unread: true,
            text_body: Some(format!("Body containing {secret_marker}")),
            html_body: None,
            attachments: vec![],
            has_remote_images: false,
        }];
        cache.save_conversation(&thread_id, &messages).unwrap();
        assert_eq!(cache.load_conversation(&thread_id).unwrap(), Some(messages));

        // FTS5 search finds the thread.
        let hits = cache.search_threads(secret_marker, 10).unwrap();
        assert_eq!(hits, vec![thread_id.clone()]);

        // Selective invalidation removes only the invalidated thread's messages & FTS entry.
        cache
            .invalidate_threads(std::slice::from_ref(&thread_id))
            .unwrap();
        assert_eq!(cache.load_conversation(&thread_id).unwrap(), None);
        assert!(cache.search_threads(secret_marker, 10).unwrap().is_empty());

        drop(cache);

        // Verify the plaintext secret does NOT appear anywhere in the raw database file.
        let raw_bytes = fs::read(&path).unwrap();
        let needle = secret_marker.as_bytes();
        assert!(
            !raw_bytes.windows(needle.len()).any(|w| w == needle),
            "plaintext secret leaked into SQLCipher database file!"
        );

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn outdated_schema_version_is_dropped_and_rebuilt() {
        let path = temp_db_path("schema_version");
        {
            let cache = VelaCache::open_at(path.clone(), KEY_A).unwrap();
            cache.set_sync_state("email_state", "s-123").unwrap();
            let conn = cache.lock_conn().unwrap();
            conn.execute_batch(
                "UPDATE meta SET value = '0' WHERE key = 'schema_version';
                 ALTER TABLE threads DROP COLUMN recipients_json;",
            )
            .unwrap();
        }

        let rebuilt = VelaCache::open_at(path.clone(), KEY_A).unwrap();
        // Sync tokens are dropped with the data so the next sync is a full refetch.
        assert_eq!(rebuilt.get_sync_state("email_state").unwrap(), None);
        let mailbox_id = MailboxId("mb".into());
        rebuilt.save_threads(&mailbox_id, &[]).unwrap();
        assert!(rebuilt.load_threads(&mailbox_id).unwrap().is_empty());

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn wrong_key_triggers_graceful_rebuild() {
        let path = temp_db_path("wrong_key");
        {
            let cache = VelaCache::open_at(path.clone(), KEY_A).unwrap();
            cache.set_sync_state("email_state", "s-123").unwrap();
        }

        // Opening with a different key must wipe and rebuild cleanly instead of failing.
        let rebuilt = VelaCache::open_at(path.clone(), KEY_B).unwrap();
        assert_eq!(rebuilt.get_sync_state("email_state").unwrap(), None);

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
