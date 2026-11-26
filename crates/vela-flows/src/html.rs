// SPDX-License-Identifier: MIT

//! HTML email analysis, sanitization, and quote folding.
//!
//! Runs purely in memory with zero network access so every policy decision
//! (blocking external images, extracting `cid:` inline parts, folding quoted
//! replies) is deterministic and unit-testable.

use std::borrow::Cow;

use vela_core::RemoteImagePolicy;

/// Summary of an HTML email body before rendering.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HtmlAnalysis {
    pub has_remote_images: bool,
    pub cid_refs: Vec<String>,
    pub is_rich_layout: bool,
}

/// Prepared view of a message body ready for the COSMIC conversation pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedBody {
    pub main_text: String,
    pub quoted_text: Option<String>,
    pub analysis: HtmlAnalysis,
}

/// Inspect raw HTML for remote image references, `cid:` inline parts, and
/// table/CSS newsletter layouts.
pub fn analyze_html(html: &str) -> HtmlAnalysis {
    let lower = html.to_ascii_lowercase();
    let is_rich_layout = lower.contains("<table") || lower.contains("<style");

    let mut has_remote_images = false;
    let mut cid_refs = Vec::new();

    // Scan attribute values for `src="..."` / `src='...'` / `background="..."`
    // and CSS `url(...)` references without executing anything.
    for attr in ["src=", "background="] {
        let mut cursor = 0;
        while let Some(pos) = lower[cursor..].find(attr) {
            let start = cursor + pos + attr.len();
            if let Some((val, next)) = extract_attr_value(&html[start..]) {
                let trimmed = val.trim();
                let trimmed_lower = trimmed.to_ascii_lowercase();
                if trimmed_lower.starts_with("http://") || trimmed_lower.starts_with("https://") {
                    has_remote_images = true;
                } else if let Some(cid) = trimmed_lower.strip_prefix("cid:") {
                    let clean = cid.trim_matches(|c| c == '<' || c == '>').to_string();
                    if !clean.is_empty() && !cid_refs.contains(&clean) {
                        cid_refs.push(clean);
                    }
                }
                cursor = start + next;
            } else {
                cursor = start;
            }
        }
    }

    if !has_remote_images
        && (lower.contains("url(http://")
            || lower.contains("url(https://")
            || lower.contains("url(\"http://")
            || lower.contains("url(\"https://")
            || lower.contains("url('http://")
            || lower.contains("url('https://"))
    {
        has_remote_images = true;
    }

    HtmlAnalysis {
        has_remote_images,
        cid_refs,
        is_rich_layout,
    }
}

/// Sanitize HTML using `ammonia`, enforcing `RemoteImagePolicy` on `<img>` tags.
pub fn sanitize_html(html: &str, policy: RemoteImagePolicy) -> String {
    let mut builder = ammonia::Builder::default();
    builder.add_url_schemes(["cid", "data"]);

    if policy == RemoteImagePolicy::Block {
        builder.attribute_filter(|element, attribute, value| {
            if element == "img" && attribute == "src" {
                let lower = value.trim().to_ascii_lowercase();
                if lower.starts_with("http://") || lower.starts_with("https://") {
                    return None;
                }
            }
            Some(Cow::Borrowed(value))
        });
    }

    builder.clean(html).to_string()
}

/// Prepare a message body for clean display in the conversation pane.
pub fn prepare_body(text_body: Option<&str>, html_body: Option<&str>) -> PreparedBody {
    let analysis = html_body.map(analyze_html).unwrap_or_default();

    let raw_text = match (text_body, html_body) {
        (Some(text), _) if !text.trim().is_empty() => text.to_string(),
        (_, Some(html)) => html_to_readable_text(html),
        _ => String::new(),
    };

    let (main_text, quoted_text) = split_quoted_text(&raw_text);
    PreparedBody {
        main_text,
        quoted_text,
        analysis,
    }
}

/// Convert sanitized HTML into clean readable plain text while preserving
/// paragraph breaks and list structure.
pub fn html_to_readable_text(html: &str) -> String {
    let cleaned = sanitize_html(html, RemoteImagePolicy::Block);
    let mut out = String::with_capacity(cleaned.len() / 2);
    let mut chars = cleaned.chars().peekable();
    let mut in_tag = false;
    let mut tag_buf = String::new();

    while let Some(ch) = chars.next() {
        match ch {
            '<' => {
                in_tag = true;
                tag_buf.clear();
            }
            '>' if in_tag => {
                in_tag = false;
                let tag = tag_buf
                    .trim()
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                match tag.as_str() {
                    "br" | "p" | "div" | "tr" | "h1" | "h2" | "h3" | "h4" | "blockquote" => {
                        if !out.ends_with('\n') {
                            out.push('\n');
                        }
                    }
                    "li" => {
                        if !out.ends_with('\n') {
                            out.push('\n');
                        }
                        if !tag_buf.trim_start().starts_with('/') {
                            out.push_str("• ");
                        }
                    }
                    _ => {}
                }
            }
            _ if in_tag => tag_buf.push(ch),
            '&' => {
                let mut entity = String::new();
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next == ';' || entity.len() > 10 {
                        break;
                    }
                    entity.push(next);
                }
                match entity.as_str() {
                    "amp" => out.push('&'),
                    "lt" => out.push('<'),
                    "gt" => out.push('>'),
                    "quot" => out.push('"'),
                    "#39" | "apos" => out.push('\''),
                    "nbsp" => out.push(' '),
                    other => {
                        out.push('&');
                        out.push_str(other);
                        out.push(';');
                    }
                }
            }
            c => out.push(c),
        }
    }

    collapse_blank_lines(&out)
}

/// Split the main body of a reply from trailing quoted history so long threads
/// can collapse repetitive quotes behind a compact `[⋯]` toggle.
pub fn split_quoted_text(text: &str) -> (String, Option<String>) {
    let lines: Vec<&str> = text.lines().collect();
    let mut split_at = None;

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if is_quote_header(trimmed) {
            split_at = Some(idx);
            break;
        }
        if trimmed.starts_with('>') && idx > 0 {
            // Only fold a trailing quote block (where remaining non-empty lines
            // are also quoted or blank), so inline replies stay visible.
            let rest_is_quoted = lines[idx..]
                .iter()
                .all(|l| l.trim().is_empty() || l.trim().starts_with('>'));
            if rest_is_quoted {
                split_at = Some(idx);
                break;
            }
        }
    }

    match split_at {
        Some(idx) if idx > 0 => {
            let main = lines[..idx].join("\n").trim_end().to_string();
            let quoted = lines[idx..].join("\n").trim().to_string();
            if quoted.is_empty() {
                (main, None)
            } else {
                (main, Some(quoted))
            }
        }
        _ => (text.trim().to_string(), None),
    }
}

fn is_quote_header(trimmed: &str) -> bool {
    let lower = trimmed.to_ascii_lowercase();
    (lower.starts_with("on ") && lower.ends_with("wrote:"))
        || trimmed.ends_with("написал:")
        || trimmed.ends_with("написала:")
        || trimmed.ends_with("написал(а):")
        || lower.starts_with("-----original message-----")
        || lower.starts_with("-------- forwarded message")
}

fn extract_attr_value(slice: &str) -> Option<(&str, usize)> {
    let trimmed_start = slice.len() - slice.trim_start().len();
    let rest = &slice[trimmed_start..];
    let first = rest.chars().next()?;
    if first == '"' || first == '\'' {
        let after_quote = &rest[1..];
        let end = after_quote.find(first)?;
        Some((&after_quote[..end], trimmed_start + 1 + end + 1))
    } else {
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '>')
            .unwrap_or(rest.len());
        Some((&rest[..end], trimmed_start + end))
    }
}

fn collapse_blank_lines(input: &str) -> String {
    let mut result = Vec::new();
    let mut consecutive_empty = 0u8;
    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            consecutive_empty = consecutive_empty.saturating_add(1);
            if consecutive_empty <= 1 && !result.is_empty() {
                result.push(String::new());
            }
        } else {
            consecutive_empty = 0;
            result.push(trimmed.to_string());
        }
    }
    while result.last().is_some_and(|l| l.is_empty()) {
        result.pop();
    }
    result.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_remote_images_cid_and_table_layout() {
        let html = r#"
            <html><body>
              <table><tr><td>
                <img src="cid:logo@yogg" />
                <img src="https://tracker.example/pixel.gif" />
                <p>Hello &amp; welcome!</p>
              </td></tr></table>
            </body></html>
        "#;
        let analysis = analyze_html(html);
        assert!(analysis.has_remote_images);
        assert!(analysis.is_rich_layout);
        assert_eq!(analysis.cid_refs, vec!["logo@yogg"]);

        let blocked = sanitize_html(html, RemoteImagePolicy::Block);
        assert!(!blocked.contains("tracker.example"));
        assert!(blocked.contains("cid:logo@yogg"));

        let allowed = sanitize_html(html, RemoteImagePolicy::AllowOnce);
        assert!(allowed.contains("https://tracker.example/pixel.gif"));
    }

    #[test]
    fn splits_trailing_quoted_reply() {
        let body =
            "Sounds good, let's ship it!\n\nOn Tue, Nov 25, Alice wrote:\n> Shall we deploy?";
        let (main, quoted) = split_quoted_text(body);
        assert_eq!(main, "Sounds good, let's ship it!");
        assert!(quoted.unwrap().contains("Shall we deploy?"));
    }
}
