//! Reads provider Atom/RSS feeds and picks out entries that look like a
//! usage-limit reset. A match is only a candidate: a person confirms it.

use std::io::Read;
use std::time::Duration;

/// Feeds checked on 2026-11-11 to exist and parse. Social posts are not covered:
/// they have no public feed. Release-note feeds were tried and dropped: changelog
/// lines about limits matched on every release.
pub const DEFAULT_FEEDS: &[&str] = &[
    "https://status.claude.com/history.atom",
    "https://status.openai.com/history.atom",
    "https://openai.com/news/rss.xml",
];

const MAX_FEED_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub title: String,
    pub link: String,
    pub published: String,
    pub summary: String,
}

pub fn fetch(url: &str) -> Result<String, String> {
    let response = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .user_agent("feed-reader/1.0")
        .build()
        .get(url)
        .call()
        .map_err(|e| e.to_string())?;
    let mut body = String::new();
    response
        .into_reader()
        .take(MAX_FEED_BYTES)
        .read_to_string(&mut body)
        .map_err(|e| e.to_string())?;
    Ok(body)
}

fn unescape(text: &str) -> String {
    let text = text.trim();
    let text = text
        .strip_prefix("<![CDATA[")
        .and_then(|t| t.strip_suffix("]]>"))
        .unwrap_or(text);
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Text of the first `<tag ...>...</tag>` in `block`.
fn tag_text(block: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut from = 0;
    while let Some(start) = block[from..].find(&open).map(|i| i + from) {
        let after = &block[start + open.len()..];
        // `<link` must not match `<linked>`.
        if !after.starts_with(['>', ' ', '/', '\t', '\n']) {
            from = start + open.len();
            continue;
        }
        let Some(gt) = after.find('>') else { break };
        if after[..gt].ends_with('/') {
            return String::new();
        }
        let body = &after[gt + 1..];
        return body
            .find(&close)
            .map(|end| unescape(&body[..end]))
            .unwrap_or_default();
    }
    String::new()
}

fn attr(block: &str, tag: &str, name: &str) -> String {
    let open = format!("<{tag} ");
    let Some(start) = block.find(&open) else {
        return String::new();
    };
    let rest = &block[start..];
    let end = rest.find('>').unwrap_or(rest.len());
    let needle = format!("{name}=\"");
    rest[..end]
        .find(&needle)
        .map(|i| &rest[i + needle.len()..end])
        .and_then(|v| v.split('"').next())
        .map(unescape)
        .unwrap_or_default()
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut inside = false;
    for c in html.chars() {
        match c {
            '<' => inside = true,
            '>' => {
                inside = false;
                out.push(' ');
            }
            _ if !inside => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Entries of an Atom (`<entry>`) or RSS (`<item>`) document. Tolerant by design:
/// a feed that does not parse yields no entries rather than an error.
pub fn parse(xml: &str) -> Vec<Entry> {
    let (open, close) = if xml.contains("<entry") {
        ("<entry", "</entry>")
    } else {
        ("<item", "</item>")
    };
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(open) {
        let Some(len) = rest[start..].find(close) else {
            break;
        };
        let block = &rest[start..start + len];
        let mut link = attr(block, "link", "href");
        if link.is_empty() {
            link = tag_text(block, "link");
        }
        let mut published = tag_text(block, "updated");
        for tag in ["published", "pubDate"] {
            if published.is_empty() {
                published = tag_text(block, tag);
            }
        }
        let mut summary = tag_text(block, "content");
        for tag in ["summary", "description"] {
            if summary.is_empty() {
                summary = tag_text(block, tag);
            }
        }
        out.push(Entry {
            title: strip_tags(&tag_text(block, "title")),
            link,
            published,
            summary: strip_tags(&unescape(&summary)),
        });
        rest = &rest[start + len + close.len()..];
    }
    out
}

/// True when the entry says limits were, or will be, reset. Requires both a
/// limit word and a reset word so ordinary incident posts do not match.
pub fn looks_like_reset(entry: &Entry) -> bool {
    let text = format!("{} {}", entry.title, entry.summary).to_lowercase();
    let reset = ["reset", "resetting", "restored usage", "refund", "credited"]
        .iter()
        .any(|w| text.contains(w));
    let limit = [
        "usage limit",
        "rate limit",
        "weekly limit",
        "usage cap",
        "quota",
        "limits for all",
        "limits have been",
    ]
    .iter()
    .any(|w| text.contains(w));
    reset && limit
}

pub fn provider_guess(feed: &str, entry: &Entry) -> &'static str {
    let text = format!("{feed} {} {}", entry.title, entry.summary).to_lowercase();
    if text.contains("claude") || text.contains("anthropic") {
        "claude"
    } else if text.contains("codex") || text.contains("openai") || text.contains("chatgpt") {
        "codex"
    } else {
        ""
    }
}

/// Stable short id for an entry: FNV-1a over feed, link and title.
pub fn entry_id(feed: &str, entry: &Entry) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in feed
        .bytes()
        .chain([0])
        .chain(entry.link.bytes())
        .chain([0])
        .chain(entry.title.bytes())
    {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ATOM: &str = r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><title>Status</title>
<entry><id>1</id><published>2026-11-01T10:00:00Z</published><updated>2026-11-01T12:00:00Z</updated>
<link rel="alternate" type="text/html" href="https://status.example/incidents/1"/>
<title>Elevated errors on Claude Code</title>
<content type="html">&lt;p&gt;Resolved. As a courtesy we have reset usage limits for all subscribers.&lt;/p&gt;</content></entry>
<entry><id>2</id><updated>2026-11-02T12:00:00Z</updated><link href="https://status.example/incidents/2"/>
<title>Degraded performance</title><content type="html">&lt;p&gt;Investigating elevated latency.&lt;/p&gt;</content></entry></feed>"#;

    const RSS: &str = r#"<rss><channel><title>News</title>
<item><title><![CDATA[Codex rate limits reset for Plus & Pro]]></title><link>https://example.com/a</link>
<pubDate>Tue, 03 Nov 2026 10:00:00 GMT</pubDate><description>We are resetting rate limits today.</description></item>
<item><title>New model</title><link>https://example.com/b</link><description>A new model.</description></item></channel></rss>"#;

    #[test]
    fn atom_entries() {
        let entries = parse(ATOM);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].title, "Elevated errors on Claude Code");
        assert_eq!(entries[0].link, "https://status.example/incidents/1");
        assert_eq!(entries[0].published, "2026-11-01T12:00:00Z");
        assert!(entries[0].summary.contains("reset usage limits"));
        assert!(looks_like_reset(&entries[0]));
        assert!(!looks_like_reset(&entries[1]));
        assert_eq!(
            provider_guess("https://status.claude.com/history.atom", &entries[0]),
            "claude"
        );
    }

    #[test]
    fn rss_items() {
        let entries = parse(RSS);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].title, "Codex rate limits reset for Plus & Pro");
        assert_eq!(entries[0].link, "https://example.com/a");
        assert!(looks_like_reset(&entries[0]));
        assert!(!looks_like_reset(&entries[1]));
        assert_eq!(
            provider_guess("https://openai.com/news/rss.xml", &entries[0]),
            "codex"
        );
    }

    #[test]
    fn a_reset_word_alone_is_not_enough() {
        let e = Entry {
            title: "Password reset emails delayed".into(),
            link: String::new(),
            published: String::new(),
            summary: String::new(),
        };
        assert!(!looks_like_reset(&e));
    }

    #[test]
    fn ids_are_stable_and_distinct() {
        let entries = parse(ATOM);
        assert_eq!(entry_id("f", &entries[0]), entry_id("f", &entries[0]));
        assert_ne!(entry_id("f", &entries[0]), entry_id("f", &entries[1]));
        assert_eq!(entry_id("f", &entries[0]).len(), 16);
    }

    #[test]
    fn garbage_yields_nothing() {
        assert!(parse("not xml at all").is_empty());
        assert!(parse("<entry><title>unterminated").is_empty());
    }
}
