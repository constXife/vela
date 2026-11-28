// SPDX-License-Identifier: MIT

//! Structured search query parsing for the thread list search bar.
//!
//! The raw search text is the single source of truth: operators such as
//! `from:`, `has:attachment`, `is:unread` or `newer:7d` are parsed into
//! structured filters, and every remaining term is free text that is matched
//! against thread summaries and the SQLCipher FTS5 index.
//!
//! Supported operators (case-insensitive keys):
//! - `from:<text>` / `to:<text>` — substring of a sender / recipient name or address
//! - `has:attachment`
//! - `is:unread` / `is:read` / `is:flagged`
//! - `after:YYYY-MM-DD` — received on or after that local date (inclusive)
//! - `before:YYYY-MM-DD` — received before that local date (exclusive)
//! - `newer:<N><d|w|m|y>` — received within the last N days / weeks / months / years
//!
//! Values may be double-quoted (`from:"Alice Smith"`). Tokens with an unknown
//! key (e.g. `https://example.org`) are free text. Known operators with an
//! invalid value are reported in [`SearchQuery::invalid`] and ignored.

use std::collections::HashSet;

use jiff::civil::Date;
use jiff::{Span, Timestamp, Zoned};
use vela_core::{EmailAddress, ThreadId, ThreadSummary};

/// Parsed search bar contents.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchQuery {
    raw: String,
    /// Free-text terms (quoted phrases kept as one term).
    pub text: Vec<String>,
    /// Lowercased `from:` needles; all must match a sender.
    pub from: Vec<String>,
    /// Lowercased `to:` needles; all must match a recipient.
    pub to: Vec<String>,
    pub has_attachment: bool,
    /// `Some(true)` for `is:unread`, `Some(false)` for `is:read`.
    pub unread: Option<bool>,
    pub flagged: bool,
    /// Inclusive lower bound on `received_at`.
    pub received_after: Option<Timestamp>,
    /// Exclusive upper bound on `received_at`.
    pub received_before: Option<Timestamp>,
    /// Known operator tokens whose value could not be parsed.
    pub invalid: Vec<String>,
}

impl SearchQuery {
    /// Parse `raw`, resolving calendar dates and relative ages against `now`.
    pub fn parse(raw: &str, now: &Zoned) -> Self {
        let mut query = Self {
            raw: raw.to_string(),
            ..Self::default()
        };
        for token in tokenize(raw) {
            query.apply_token(&token, now);
        }
        query
    }

    /// The raw search text this query was parsed from.
    pub fn raw(&self) -> &str {
        &self.raw
    }

    pub fn is_empty(&self) -> bool {
        self.raw.trim().is_empty()
    }

    pub fn has_text(&self) -> bool {
        !self.text.is_empty()
    }

    /// Whether any structured (non-free-text) filter is active.
    pub fn has_filters(&self) -> bool {
        !self.from.is_empty()
            || !self.to.is_empty()
            || self.has_attachment
            || self.unread.is_some()
            || self.flagged
            || self.received_after.is_some()
            || self.received_before.is_some()
    }

    /// Whether the raw text contains `key:value` as a standalone token.
    pub fn has_operator(&self, key: &str, value: &str) -> bool {
        tokenize(&self.raw).iter().any(|tok| {
            split_operator(tok).is_some_and(|(k, v)| {
                k.eq_ignore_ascii_case(key) && unquote(v).eq_ignore_ascii_case(value)
            })
        })
    }

    /// Raw text with `key:value` toggled: if present, every `key:*` token is
    /// removed; otherwise any other `key:*` token is replaced by `key:value`.
    pub fn toggled_operator(&self, key: &str, value: &str) -> String {
        let was_set = self.has_operator(key, value);
        let mut tokens: Vec<String> = tokenize(&self.raw)
            .into_iter()
            .filter(|tok| !split_operator(tok).is_some_and(|(k, _)| k.eq_ignore_ascii_case(key)))
            .collect();
        if !was_set {
            tokens.push(format!("{key}:{value}"));
        }
        tokens.join(" ")
    }

    /// Whether `thread` satisfies every structured filter and every free-text
    /// term. A thread listed in `fts_hits` (SQLCipher FTS5 matches for
    /// [`Self::text`]) satisfies the free-text part regardless of its summary.
    pub fn matches(&self, thread: &ThreadSummary, fts_hits: Option<&HashSet<ThreadId>>) -> bool {
        if self.has_attachment && !thread.has_attachments {
            return false;
        }
        if self.unread.is_some_and(|unread| unread != thread.unread) {
            return false;
        }
        if self.flagged && !thread.flagged {
            return false;
        }
        if self
            .received_after
            .is_some_and(|after| thread.received_at < after)
        {
            return false;
        }
        if self
            .received_before
            .is_some_and(|before| thread.received_at >= before)
        {
            return false;
        }
        if !self
            .from
            .iter()
            .all(|needle| any_address_contains(&thread.senders, needle))
        {
            return false;
        }
        if !self
            .to
            .iter()
            .all(|needle| any_address_contains(&thread.recipients, needle))
        {
            return false;
        }
        if self.text.is_empty() || fts_hits.is_some_and(|hits| hits.contains(&thread.id)) {
            return true;
        }
        self.text.iter().all(|term| {
            let term = term.to_lowercase();
            thread.subject.to_lowercase().contains(&term)
                || thread.snippet.to_lowercase().contains(&term)
                || any_address_contains(&thread.senders, &term)
        })
    }

    fn apply_token(&mut self, token: &str, now: &Zoned) {
        let Some((key, value)) = split_operator(token) else {
            self.push_text(token);
            return;
        };
        let value = unquote(value);
        let valid = match key.to_ascii_lowercase().as_str() {
            "from" => push_needle(&mut self.from, value),
            "to" => push_needle(&mut self.to, value),
            "has" if value.eq_ignore_ascii_case("attachment") => {
                self.has_attachment = true;
                true
            }
            "is" if value.eq_ignore_ascii_case("unread") => {
                self.unread = Some(true);
                true
            }
            "is" if value.eq_ignore_ascii_case("read") => {
                self.unread = Some(false);
                true
            }
            "is" if value.eq_ignore_ascii_case("flagged") => {
                self.flagged = true;
                true
            }
            "after" => match local_midnight(value, now) {
                Some(ts) => {
                    self.raise_lower_bound(ts);
                    true
                }
                None => false,
            },
            "before" => match local_midnight(value, now) {
                Some(ts) => {
                    self.received_before = Some(self.received_before.map_or(ts, |b| b.min(ts)));
                    true
                }
                None => false,
            },
            "newer" => match relative_age_start(value, now) {
                Some(ts) => {
                    self.raise_lower_bound(ts);
                    true
                }
                None => false,
            },
            "has" | "is" => false,
            _ => {
                self.push_text(token);
                return;
            }
        };
        if !valid {
            self.invalid.push(token.to_string());
        }
    }

    fn push_text(&mut self, token: &str) {
        let term = unquote(token).trim();
        if !term.is_empty() {
            self.text.push(term.to_string());
        }
    }

    fn raise_lower_bound(&mut self, ts: Timestamp) {
        self.received_after = Some(self.received_after.map_or(ts, |a| a.max(ts)));
    }
}

fn push_needle(needles: &mut Vec<String>, value: &str) -> bool {
    let needle = value.trim().to_lowercase();
    if needle.is_empty() {
        return false;
    }
    needles.push(needle);
    true
}

fn any_address_contains(addrs: &[EmailAddress], needle: &str) -> bool {
    addrs.iter().any(|addr| {
        addr.email.to_lowercase().contains(needle)
            || addr
                .name
                .as_deref()
                .is_some_and(|name| name.to_lowercase().contains(needle))
    })
}

fn local_midnight(value: &str, now: &Zoned) -> Option<Timestamp> {
    let date: Date = value.parse().ok()?;
    date.to_zoned(now.time_zone().clone())
        .ok()
        .map(|z| z.timestamp())
}

fn relative_age_start(value: &str, now: &Zoned) -> Option<Timestamp> {
    let unit = value.chars().last()?;
    let amount: i64 = value[..value.len() - unit.len_utf8()].parse().ok()?;
    if amount <= 0 {
        return None;
    }
    let span = match unit.to_ascii_lowercase() {
        'd' => Span::new().try_days(amount).ok()?,
        'w' => Span::new().try_weeks(amount).ok()?,
        'm' => Span::new().try_months(amount).ok()?,
        'y' => Span::new().try_years(amount).ok()?,
        _ => return None,
    };
    now.checked_sub(span).ok().map(|z| z.timestamp())
}

/// Split `key:value` where `key` is a non-empty ASCII word.
fn split_operator(token: &str) -> Option<(&str, &str)> {
    let (key, value) = token.split_once(':')?;
    if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    Some((key, value))
}

fn unquote(value: &str) -> &str {
    let value = value.strip_prefix('"').unwrap_or(value);
    value.strip_suffix('"').unwrap_or(value)
}

/// Split on whitespace, keeping double-quoted sections (including
/// `key:"quoted value"`) inside a single token.
fn tokenize(raw: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for c in raw.chars() {
        if c == '"' {
            in_quotes = !in_quotes;
            current.push(c);
        } else if c.is_whitespace() && !in_quotes {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use jiff::tz::{self, TimeZone};
    use vela_core::{EmailAddress, MailboxId};

    use super::*;

    // Fixed offsets keep tests hermetic: no system tzdb (absent in the Nix sandbox).
    fn now() -> Zoned {
        ts("2026-11-28T12:00:00Z").to_zoned(TimeZone::UTC)
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn thread(id: &str) -> ThreadSummary {
        ThreadSummary {
            id: ThreadId(id.into()),
            subject: "Quarterly invoice".into(),
            snippet: "Please find the PDF attached".into(),
            senders: vec![EmailAddress {
                name: Some("Alice Smith".into()),
                email: "alice@example.com".into(),
            }],
            recipients: vec![EmailAddress {
                name: Some("Bob".into()),
                email: "bob@corp.test".into(),
            }],
            received_at: ts("2026-11-20T09:00:00Z"),
            message_count: 1,
            unread: true,
            flagged: false,
            has_attachments: true,
            mailbox_ids: vec![MailboxId("mb-inbox".into())],
        }
    }

    #[test]
    fn parses_operators_and_free_text() {
        let q = SearchQuery::parse(
            r#"invoice from:"Alice Smith" TO:bob has:attachment is:unread is:flagged "q3 report" https://x.test"#,
            &now(),
        );
        assert_eq!(q.text, vec!["invoice", "q3 report", "https://x.test"]);
        assert_eq!(q.from, vec!["alice smith"]);
        assert_eq!(q.to, vec!["bob"]);
        assert!(q.has_attachment && q.flagged);
        assert_eq!(q.unread, Some(true));
        assert!(q.invalid.is_empty());
    }

    #[test]
    fn resolves_date_bounds_in_local_time_zone() {
        let now = ts("2026-11-28T09:00:00Z").to_zoned(TimeZone::fixed(tz::offset(3)));
        let q = SearchQuery::parse("after:2026-11-01 before:2026-11-15", &now);
        assert_eq!(q.received_after, Some(ts("2026-10-31T21:00:00Z")));
        assert_eq!(q.received_before, Some(ts("2026-11-14T21:00:00Z")));

        // `newer:` and `after:` both narrow the lower bound; the later one wins.
        let q = SearchQuery::parse("after:2026-01-01 newer:7d", &now);
        assert_eq!(q.received_after, Some(ts("2026-11-21T09:00:00Z")));
        let q = SearchQuery::parse("newer:2w", &now);
        assert_eq!(q.received_after, Some(ts("2026-11-14T09:00:00Z")));
        let q = SearchQuery::parse("newer:1m", &now);
        assert_eq!(q.received_after, Some(ts("2026-10-28T09:00:00Z")));
    }

    #[test]
    fn invalid_operator_values_are_reported_not_searched() {
        let q = SearchQuery::parse(
            "after:2026-13-01 newer:7x newer:0d has:pdf is:spam from: hello",
            &now(),
        );
        assert_eq!(
            q.invalid,
            vec![
                "after:2026-13-01",
                "newer:7x",
                "newer:0d",
                "has:pdf",
                "is:spam",
                "from:"
            ]
        );
        assert_eq!(q.text, vec!["hello"]);
        assert!(!q.has_filters());
    }

    #[test]
    fn matches_structured_filters() {
        let t = thread("t-1");
        let m = |raw: &str| SearchQuery::parse(raw, &now()).matches(&t, None);

        assert!(m(""));
        assert!(m("from:alice to:corp.test has:attachment is:unread"));
        assert!(m("from:smith"));
        assert!(!m("from:bob"));
        assert!(!m("to:alice"));
        assert!(!m("is:read"));
        assert!(!m("is:flagged"));
        assert!(m("after:2026-11-20 before:2026-11-21"));
        assert!(!m("after:2026-11-21"));
        assert!(!m("before:2026-11-20"));
        assert!(m("newer:10d"));
        assert!(!m("newer:7d"));
    }

    #[test]
    fn free_text_terms_match_summary_or_fts_hits() {
        let t = thread("t-1");
        let q = SearchQuery::parse("invoice pdf", &now());
        assert!(q.matches(&t, None));

        // Every term must match the summary...
        let q = SearchQuery::parse("invoice kubernetes", &now());
        assert!(!q.matches(&t, None));
        // ...unless the FTS5 body index matched the thread.
        let hits: HashSet<ThreadId> = [ThreadId("t-1".into())].into();
        assert!(q.matches(&t, Some(&hits)));

        // FTS hits never bypass structured filters.
        let q = SearchQuery::parse("kubernetes is:read", &now());
        assert!(!q.matches(&t, Some(&hits)));
    }

    #[test]
    fn toggles_chip_operators_in_raw_text() {
        let q = SearchQuery::parse("invoice newer:30d", &now());
        assert!(q.has_operator("newer", "30d"));
        assert!(!q.has_operator("newer", "7d"));

        // Selecting another preset replaces the existing one.
        assert_eq!(q.toggled_operator("newer", "7d"), "invoice newer:7d");
        // Toggling the active preset removes it.
        assert_eq!(q.toggled_operator("newer", "30d"), "invoice");

        let q = SearchQuery::parse(r#"from:"Alice Smith""#, &now());
        assert_eq!(
            q.toggled_operator("has", "attachment"),
            r#"from:"Alice Smith" has:attachment"#
        );
    }
}
