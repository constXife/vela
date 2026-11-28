// SPDX-License-Identifier: MIT

//! Structured search query parsing for the thread list search bar.
//!
//! [`SearchInput`] is the single source of truth for what the user typed:
//! committed operator blocks plus the draft text still being edited. It is
//! parsed into a [`SearchQuery`]: operators such as `from:`, `has:attachment`,
//! `is:unread` or `newer:7d` become structured filters, and every remaining
//! term is free text matched against thread summaries and the SQLCipher FTS5
//! index.
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

use jiff::civil::{Date, Weekday};
use jiff::{Span, Timestamp, Zoned};
use vela_core::{EmailAddress, KnownAddress, ThreadId, ThreadSummary};

/// Operator keys; any other `key:value` token is free text.
const OPERATOR_KEYS: [&str; 7] = ["from", "to", "has", "is", "after", "before", "newer"];

const MAX_SUGGESTIONS: usize = 8;

/// Operator templates offered while typing a bare word.
const OPERATOR_TEMPLATES: [(&str, &str); 9] = [
    ("from:", "Sender name or address"),
    ("to:", "Recipient name or address"),
    ("has:attachment", "With attachments"),
    ("is:unread", "Unread"),
    ("is:read", "Read"),
    ("is:flagged", "Flagged"),
    ("after:", "Received on or after YYYY-MM-DD"),
    ("before:", "Received before YYYY-MM-DD"),
    ("newer:", "Received within 7d, 2w, 3m, 1y"),
];

/// A completion for the token being typed in the search draft.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    pub label: String,
    pub detail: Option<String>,
    /// Token that replaces the one being typed.
    pub replacement: String,
}

impl Suggestion {
    fn new(
        label: impl Into<String>,
        detail: Option<String>,
        replacement: impl Into<String>,
    ) -> Self {
        Self {
            label: label.into(),
            detail,
            replacement: replacement.into(),
        }
    }

    fn operator(token: String, detail: &str) -> Self {
        Self::new(token.clone(), Some(detail.to_string()), token)
    }
}

/// Search bar contents: committed operator blocks and the draft being typed.
///
/// An operator token moves from the draft into a block once it is finished
/// (followed by whitespace, or on submit). Blocks are kept verbatim, so
/// invalid operators stay visible and editable instead of being dropped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchInput {
    blocks: Vec<String>,
    draft: String,
}

impl SearchInput {
    /// Everything in `raw` as draft text, without committing blocks.
    pub fn from_raw(raw: &str) -> Self {
        Self {
            blocks: Vec::new(),
            draft: raw.to_string(),
        }
    }

    pub fn blocks(&self) -> &[String] {
        &self.blocks
    }

    pub fn draft(&self) -> &str {
        &self.draft
    }

    /// Blocks followed by the draft, as one raw query string.
    pub fn raw(&self) -> String {
        self.blocks
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(self.draft.trim()))
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty() && self.draft.trim().is_empty()
    }

    /// Replace the draft; an operator token just finished with whitespace
    /// becomes a block.
    pub fn with_draft(&self, draft: &str) -> Self {
        let mut next = Self {
            blocks: self.blocks.clone(),
            draft: draft.to_string(),
        };
        if !draft.ends_with(char::is_whitespace) || !quotes_balanced(draft) {
            return next;
        }
        let mut tokens = tokenize(draft);
        if tokens.last().is_some_and(|tok| is_operator_token(tok)) {
            next.blocks.extend(tokens.pop());
            next.draft = join_draft(&tokens, true);
        }
        next
    }

    /// Commit every finished operator token in the draft (e.g. on Enter).
    pub fn committed(&self) -> Self {
        if !quotes_balanced(&self.draft) {
            return self.clone();
        }
        let (operators, text): (Vec<String>, Vec<String>) = tokenize(&self.draft)
            .into_iter()
            .partition(|tok| is_operator_token(tok));
        let mut blocks = self.blocks.clone();
        blocks.extend(operators);
        Self {
            blocks,
            draft: join_draft(&text, false),
        }
    }

    pub fn without_block(&self, index: usize) -> Self {
        let mut next = self.clone();
        if index < next.blocks.len() {
            next.blocks.remove(index);
        }
        next
    }

    pub fn without_last_block(&self) -> Self {
        let mut next = self.clone();
        next.blocks.pop();
        next
    }

    /// Move block `index` back to the end of the draft for editing.
    pub fn editing_block(&self, index: usize) -> Self {
        let mut next = self.clone();
        if index >= next.blocks.len() {
            return next;
        }
        let block = next.blocks.remove(index);
        let draft = next.draft.trim_end();
        next.draft = if draft.is_empty() {
            block
        } else {
            format!("{draft} {block}")
        };
        next
    }

    /// Whether `key:value` is present as a block or a draft token.
    pub fn has_operator(&self, key: &str, value: &str) -> bool {
        self.blocks
            .iter()
            .cloned()
            .chain(tokenize(&self.draft))
            .any(|tok| {
                split_operator(&tok).is_some_and(|(k, v)| {
                    k.eq_ignore_ascii_case(key) && unquote(v).eq_ignore_ascii_case(value)
                })
            })
    }

    /// Completions for the token being typed at the end of the draft:
    /// operator keys for a bare word (2+ characters), fixed values for
    /// `is:`/`has:`/`newer:`/`after:`/`before:`, and correspondents from
    /// `book` for `from:`/`to:`.
    pub fn suggestions(&self, book: &[KnownAddress], now: &Zoned) -> Vec<Suggestion> {
        let Some(token) = self.current_token() else {
            return Vec::new();
        };
        let Some((key, value)) = split_operator(&token).filter(|_| is_operator_token(&token))
        else {
            if token.chars().count() < 2 {
                return Vec::new();
            }
            let prefix = token.to_lowercase();
            return OPERATOR_TEMPLATES
                .iter()
                .filter(|(template, _)| template.starts_with(&prefix) && *template != prefix)
                .map(|(template, detail)| Suggestion::operator(template.to_string(), detail))
                .collect();
        };
        let key = key.to_ascii_lowercase();
        let value = unquote(value).to_lowercase();
        let fixed = |options: &[(String, &str)]| -> Vec<Suggestion> {
            options
                .iter()
                .filter(|(option, _)| option.starts_with(&value))
                .map(|(option, detail)| Suggestion::operator(format!("{key}:{option}"), detail))
                .collect()
        };
        match key.as_str() {
            "from" => address_suggestions(&key, &value, book, |k| k.as_sender),
            "to" => address_suggestions(&key, &value, book, |k| k.as_recipient),
            "is" => fixed(&[
                ("unread".into(), "Unread"),
                ("read".into(), "Read"),
                ("flagged".into(), "Flagged"),
            ]),
            "has" => fixed(&[("attachment".into(), "With attachments")]),
            "newer" => fixed(&[
                ("1d".into(), "Last day"),
                ("7d".into(), "Last 7 days"),
                ("30d".into(), "Last 30 days"),
                ("1y".into(), "Last year"),
            ]),
            "after" | "before" => fixed(&date_presets(now)),
            _ => Vec::new(),
        }
    }

    /// Replace the token being typed with `suggestion`. A complete operator
    /// becomes a block; a bare key (`from:`) stays in the draft for its value.
    pub fn accepting(&self, suggestion: &Suggestion) -> Self {
        let mut tokens = tokenize(&self.draft);
        if self.current_token().is_some() {
            tokens.pop();
        }
        tokens.push(suggestion.replacement.clone());
        let draft = join_draft(&tokens, false);
        if suggestion.replacement.ends_with(':') {
            Self {
                blocks: self.blocks.clone(),
                draft,
            }
        } else {
            self.with_draft(&format!("{draft} "))
        }
    }

    /// The token at the end of the draft while it is still being typed.
    fn current_token(&self) -> Option<String> {
        if self.draft.ends_with(char::is_whitespace) && quotes_balanced(&self.draft) {
            return None;
        }
        tokenize(&self.draft).pop()
    }

    /// Toggle `key:value`: if present, every `key:*` block and draft token is
    /// removed; otherwise they are replaced by a `key:value` block.
    pub fn toggled_operator(&self, key: &str, value: &str) -> Self {
        let was_set = self.has_operator(key, value);
        let has_key =
            |tok: &String| split_operator(tok).is_some_and(|(k, _)| k.eq_ignore_ascii_case(key));
        let mut blocks: Vec<String> = self
            .blocks
            .iter()
            .filter(|t| !has_key(t))
            .cloned()
            .collect();
        let draft_tokens: Vec<String> = tokenize(&self.draft)
            .into_iter()
            .filter(|t| !has_key(t))
            .collect();
        if !was_set {
            blocks.push(format!("{key}:{value}"));
        }
        Self {
            blocks,
            draft: join_draft(&draft_tokens, self.draft.ends_with(char::is_whitespace)),
        }
    }
}

/// Parsed search bar contents.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchQuery {
    input: SearchInput,
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
    /// Parse raw query text, resolving calendar dates and relative ages
    /// against `now`.
    pub fn parse(raw: &str, now: &Zoned) -> Self {
        Self::from_input(SearchInput::from_raw(raw), now)
    }

    /// Parse search bar contents, resolving dates against `now`.
    pub fn from_input(input: SearchInput, now: &Zoned) -> Self {
        let mut query = Self::default();
        for token in tokenize(&input.raw()) {
            query.apply_token(&token, now);
        }
        query.input = input;
        query
    }

    /// The search bar contents this query was parsed from.
    pub fn input(&self) -> &SearchInput {
        &self.input
    }

    pub fn is_empty(&self) -> bool {
        self.input.is_empty()
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
        let Some((key, value)) = split_operator(token).filter(|_| is_operator_token(token)) else {
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
            _ => false,
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

fn address_suggestions(
    key: &str,
    needle: &str,
    book: &[KnownAddress],
    role_count: impl Fn(&KnownAddress) -> u32,
) -> Vec<Suggestion> {
    let mut matches: Vec<&KnownAddress> = book
        .iter()
        .filter(|known| {
            known.address.email.to_lowercase().contains(needle)
                || known
                    .address
                    .name
                    .as_deref()
                    .is_some_and(|name| name.to_lowercase().contains(needle))
        })
        .collect();
    // `book` is already ranked by total; prefer the role this operator asks for.
    matches.sort_by_key(|known| std::cmp::Reverse(role_count(known)));
    matches
        .into_iter()
        .take(MAX_SUGGESTIONS)
        .map(|known| {
            let email = &known.address.email;
            let (label, detail) = match &known.address.name {
                Some(name) => (name.clone(), Some(email.clone())),
                None => (email.clone(), None),
            };
            Suggestion::new(label, detail, format!("{key}:{email}"))
        })
        .collect()
}

fn date_presets(now: &Zoned) -> Vec<(String, &'static str)> {
    let today = now.date();
    let week_start = today
        .nth_weekday(-1, Weekday::Monday)
        .ok()
        .filter(|_| today.weekday() != Weekday::Monday)
        .unwrap_or(today);
    let month_start = today.first_of_month();
    let year_start = today.first_of_year();
    vec![
        (today.to_string(), "Today"),
        (week_start.to_string(), "This week"),
        (month_start.to_string(), "This month"),
        (year_start.to_string(), "This year"),
    ]
}

fn is_operator_token(token: &str) -> bool {
    split_operator(token).is_some_and(|(key, _)| {
        OPERATOR_KEYS
            .iter()
            .any(|known| known.eq_ignore_ascii_case(key))
    })
}

fn quotes_balanced(text: &str) -> bool {
    text.matches('"').count().is_multiple_of(2)
}

fn join_draft(tokens: &[String], trailing_space: bool) -> String {
    let mut draft = tokens.join(" ");
    if trailing_space && !draft.is_empty() {
        draft.push(' ');
    }
    draft
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
    fn finished_operators_become_blocks() {
        let input = SearchInput::default().with_draft("invoice from:ali");
        assert!(input.blocks().is_empty());

        // Whitespace after an operator commits it; free text stays in the draft.
        let input = input.with_draft("invoice from:alice ");
        assert_eq!(input.blocks(), ["from:alice"]);
        assert_eq!(input.draft(), "invoice ");

        // Unknown keys are text; open quotes keep the token in the draft.
        let input = input.with_draft("invoice https://x.test ");
        assert_eq!(input.blocks(), ["from:alice"]);
        let input = input.with_draft(r#"invoice to:"Bob "#);
        assert_eq!(input.draft(), r#"invoice to:"Bob "#);
        let input = input.with_draft(r#"invoice to:"Bob Lee" "#);
        assert_eq!(input.blocks(), ["from:alice", r#"to:"Bob Lee""#]);

        // Invalid operators become blocks too, so they stay visible.
        let input = input.with_draft("invoice after:2026-13-01 ");
        assert_eq!(input.blocks().last().unwrap(), "after:2026-13-01");
        assert_eq!(
            input.raw(),
            r#"from:alice to:"Bob Lee" after:2026-13-01 invoice"#
        );
        let q = SearchQuery::from_input(input, &now());
        assert_eq!(q.invalid, vec!["after:2026-13-01"]);
        assert_eq!(q.text, vec!["invoice"]);
    }

    #[test]
    fn submit_edit_and_remove_blocks() {
        let input = SearchInput::default().with_draft("is:unread q3 has:attachment");
        let input = input.committed();
        assert_eq!(input.blocks(), ["is:unread", "has:attachment"]);
        assert_eq!(input.draft(), "q3");

        let edited = input.editing_block(0);
        assert_eq!(edited.blocks(), ["has:attachment"]);
        assert_eq!(edited.draft(), "q3 is:unread");

        assert_eq!(input.without_block(1).blocks(), ["is:unread"]);
        assert_eq!(input.without_last_block().blocks(), ["is:unread"]);
        assert_eq!(input.without_block(9), input);
    }

    #[test]
    fn toggles_chip_operators() {
        let input = SearchInput::default().with_draft("newer:30d invoice");
        assert!(input.has_operator("newer", "30d"));
        assert!(!input.has_operator("newer", "7d"));

        // Selecting another preset replaces the existing one (even in the draft).
        let seven = input.toggled_operator("newer", "7d");
        assert_eq!(seven.blocks(), ["newer:7d"]);
        assert_eq!(seven.draft(), "invoice");
        // Toggling the active preset removes it.
        let off = seven.toggled_operator("newer", "7d");
        assert!(off.blocks().is_empty());
        assert_eq!(off.raw(), "invoice");
    }

    fn book() -> Vec<KnownAddress> {
        let known = |name: Option<&str>, email: &str, as_sender, as_recipient| KnownAddress {
            address: EmailAddress {
                name: name.map(str::to_string),
                email: email.into(),
            },
            as_sender,
            as_recipient,
        };
        vec![
            known(Some("Me"), "me@corp.test", 1, 9),
            known(Some("Alice Smith"), "alice@example.com", 5, 0),
            known(None, "alerts@monitor.test", 2, 0),
        ]
    }

    fn labels(input: &SearchInput) -> Vec<String> {
        input
            .suggestions(&book(), &now())
            .into_iter()
            .map(|s| s.label)
            .collect()
    }

    #[test]
    fn suggests_operators_for_bare_words() {
        let input = SearchInput::default().with_draft("invoice is");
        assert_eq!(labels(&input), vec!["is:unread", "is:read", "is:flagged"]);
        // One character, finished words and unknown prefixes suggest nothing.
        assert!(labels(&SearchInput::default().with_draft("i")).is_empty());
        assert!(labels(&SearchInput::default().with_draft("is ")).is_empty());
        assert!(labels(&SearchInput::default().with_draft("invoice")).is_empty());
    }

    #[test]
    fn suggests_operator_values() {
        let input = SearchInput::default().with_draft("newer:");
        assert_eq!(
            labels(&input),
            vec!["newer:1d", "newer:7d", "newer:30d", "newer:1y"]
        );
        let input = SearchInput::default().with_draft("is:un");
        assert_eq!(labels(&input), vec!["is:unread"]);
        // now() is Saturday 2026-11-28.
        let input = SearchInput::default().with_draft("after:");
        assert_eq!(
            labels(&input),
            vec![
                "after:2026-11-28",
                "after:2026-11-23",
                "after:2026-11-01",
                "after:2026-01-01"
            ]
        );
    }

    #[test]
    fn suggests_addresses_ranked_by_role() {
        let suggestions = SearchInput::default()
            .with_draft("from:")
            .suggestions(&book(), &now());
        let replacements: Vec<&str> = suggestions.iter().map(|s| s.replacement.as_str()).collect();
        assert_eq!(
            replacements,
            vec![
                "from:alice@example.com",
                "from:alerts@monitor.test",
                "from:me@corp.test"
            ]
        );
        assert_eq!(suggestions[0].label, "Alice Smith");
        assert_eq!(suggestions[0].detail.as_deref(), Some("alice@example.com"));

        // Matches names and addresses; `to:` prefers recipients.
        let input = SearchInput::default().with_draft("to:AL");
        assert_eq!(labels(&input), vec!["Alice Smith", "alerts@monitor.test"]);
        let input = SearchInput::default().with_draft("to:");
        assert_eq!(labels(&input)[0], "Me");
    }

    #[test]
    fn accepting_replaces_the_current_token() {
        let input = SearchInput::default().with_draft("invoice fr");
        let from = &input.suggestions(&book(), &now())[0];
        // A bare key stays in the draft so the value can be typed next.
        let input = input.accepting(from);
        assert_eq!(input.draft(), "invoice from:");
        assert!(input.blocks().is_empty());

        // A complete operator becomes a block.
        let input = input.with_draft("invoice from:ali");
        let alice = &input.suggestions(&book(), &now())[0];
        let input = input.accepting(alice);
        assert_eq!(input.blocks(), ["from:alice@example.com"]);
        assert_eq!(input.draft(), "invoice ");
    }
}
