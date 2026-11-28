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

    builder.clean(&plaintext_as_pre(html)).to_string()
}

/// Rewrite the obsolete `<plaintext>` element as `<pre>`.
///
/// HTML5 tokenizes everything after `<plaintext>` as raw text, so the
/// document's own closing tags (`</plaintext></body></html>`) would otherwise
/// leak into the visible body. `<pre>` keeps the preformatted intent while the
/// rest of the markup stays markup.
fn plaintext_as_pre(html: &str) -> Cow<'_, str> {
    const NAME: &str = "plaintext";
    let lower = html.to_ascii_lowercase();
    if !lower.contains(NAME) {
        return Cow::Borrowed(html);
    }

    let mut out = String::with_capacity(html.len());
    let mut copied = 0;
    let mut cursor = 0;
    while let Some(pos) = lower[cursor..].find('<') {
        let open = cursor + pos;
        let name_start = if lower[open + 1..].starts_with('/') {
            open + 2
        } else {
            open + 1
        };
        let name_end = name_start + NAME.len();
        let is_plaintext_tag = lower[name_start..].starts_with(NAME)
            && lower[name_end..]
                .chars()
                .next()
                .is_none_or(|c| c == '>' || c == '/' || c.is_ascii_whitespace());
        if is_plaintext_tag {
            out.push_str(&html[copied..name_start]);
            out.push_str("pre");
            copied = name_end;
        }
        cursor = open + 1;
    }
    out.push_str(&html[copied..]);
    Cow::Owned(out)
}

/// Prepare a message body for clean display in the conversation pane.
pub fn prepare_body(text_body: Option<&str>, html_body: Option<&str>) -> PreparedBody {
    let analysis = html_body.map(analyze_html).unwrap_or_default();

    let raw_text = match (text_body, html_body) {
        (Some(text), html) if !text.trim().is_empty() => {
            if looks_like_html(text) {
                let cleaned_text = html_to_readable_text(text);
                if let Some(html_src) = html {
                    let cleaned_html = html_to_readable_text(html_src);
                    if cleaned_text.len() < 16 && cleaned_html.len() > cleaned_text.len() {
                        cleaned_html
                    } else {
                        cleaned_text
                    }
                } else {
                    cleaned_text
                }
            } else {
                text.to_string()
            }
        }
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

/// Sanitize and compact a thread preview/snippet into a single readable line
/// bounded to `max_chars` Unicode scalar values.
///
/// Handles servers that return raw/truncated HTML (such as `<!DOCTYPE html>...<style>`)
/// or stray closing tags in JMAP `preview` fields.
pub fn clean_snippet(raw: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }

    let text = if looks_like_html(raw) {
        html_to_readable_text(raw)
    } else {
        raw.to_string()
    };

    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let char_count = collapsed.chars().count();
    if char_count <= max_chars {
        return collapsed;
    }

    let take = max_chars.saturating_sub(1);
    let mut truncated: String = collapsed.chars().take(take).collect();
    if let Some(last_space) = truncated.rfind(' ')
        && last_space >= take / 2
    {
        truncated.truncate(last_space);
    }
    let trimmed = truncated.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, ',' | ';'));
    format!("{trimmed}…")
}

/// Heuristic check for HTML markup or fragments accidentally embedded in
/// `text/plain` bodies or JMAP `preview` strings.
fn looks_like_html(input: &str) -> bool {
    let lower = input.to_ascii_lowercase();
    const MARKERS: &[&str] = &[
        "<!doctype",
        "<html",
        "<head",
        "<style",
        "<body",
        "<table",
        "<div",
        "<span",
        "<p>",
        "<p ",
        "<br",
        "<a ",
        "<img",
        "<meta",
        "<tr",
        "<td",
    ];
    MARKERS.iter().any(|m| lower.contains(m)) || has_closing_tag(&lower)
}

/// Any `</name` sequence, including one cut off by preview truncation
/// (`</plaint`). Plain text practically never contains `</` before a letter,
/// unlike `<user@host>` or `Vec<String>` which must stay untouched.
fn has_closing_tag(input: &str) -> bool {
    input
        .match_indices("</")
        .any(|(pos, _)| input[pos + 2..].starts_with(|c: char| c.is_ascii_alphabetic()))
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

    #[test]
    fn clean_snippet_strips_truncated_html_and_css() {
        let raw = r#"<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Transitional//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-transitional.dtd">
<html xmlns="http://www.w3.org/1999/xhtml">
<head>
<style type="text/css">
*{
  -ms-text-size-adjust:100%;
  -web..."#;
        assert_eq!(clean_snippet(raw, 90), "");
    }

    #[test]
    fn clean_snippet_strips_stray_tags_and_compacts_lines() {
        assert_eq!(clean_snippet("</p>int", 90), "int");

        let multiline = "via paddle.com\nYour subscription has been canceled.\nYour subscription was canceled on\nSep 3, 2026 9:31 am UTC.";
        let compacted = clean_snippet(multiline, 60);
        assert!(!compacted.contains('\n'));
        assert!(compacted.ends_with('…'));
        assert!(compacted.chars().count() <= 60);
    }

    #[test]
    fn prepare_body_cleans_broken_html_in_text_body() {
        let prepared = prepare_body(
            Some("</p>int"),
            Some(
                "<html><body><p>Full delivery report succeeded for domain example.com.</p></body></html>",
            ),
        );
        assert_eq!(
            prepared.main_text,
            "Full delivery report succeeded for domain example.com."
        );
    }

    #[test]
    fn clean_snippet_drops_truncated_closing_tag() {
        assert_eq!(clean_snippet("</plaint", 90), "");
        assert_eq!(
            clean_snippet("Email Delivery Report for example.com </plaint", 90),
            "Email Delivery Report for example.com"
        );
    }

    #[test]
    fn clean_snippet_keeps_angle_brackets_in_plain_text() {
        assert_eq!(
            clean_snippet("Reply from <ops@example.com>: Vec<String> & 1 </ 2", 90),
            "Reply from <ops@example.com>: Vec<String> & 1 </ 2"
        );
    }

    #[test]
    fn prepare_body_prefers_html_when_text_body_is_a_stray_tag() {
        let prepared = prepare_body(
            Some("</plaint"),
            Some(
                "<html><body><h1>Email Delivery Report</h1><p>All messages to example.com were delivered.</p></body></html>",
            ),
        );
        assert_eq!(
            prepared.main_text,
            "Email Delivery Report\nAll messages to example.com were delivered."
        );
    }

    #[test]
    fn plaintext_element_does_not_leak_closing_tags() {
        let html = "<html><body><p>Email Delivery Report</p><PLAINTEXT>Delivered: 12\nFailed: 0</plaintext></body></html>";
        let text = html_to_readable_text(html);
        assert_eq!(text, "Email Delivery Report\nDelivered: 12\nFailed: 0");

        let prepared = prepare_body(Some("</plaint"), Some(html));
        assert_eq!(prepared.main_text, text);
        assert_eq!(
            clean_snippet(html, 90),
            "Email Delivery Report Delivered: 12 Failed: 0"
        );
    }
}
