use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use dom_smoothie::{Config as ReadabilityConfig, Readability};
use feed_rs::parser;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::Read;
use std::net::IpAddr;
use std::sync::OnceLock;
use std::time::Duration;
use url::Url;
use uuid::Uuid;

/// Cap on response body size for article extraction. Anything larger is
/// almost certainly not a typical article page and would only burn CPU in
/// the Readability DOM walker.
const FULLTEXT_MAX_BYTES: usize = 5 * 1024 * 1024;

/// Minimum text length the extracted article body must contain to be
/// considered useful. Pages below this are treated as "page appears empty"
/// — usually JS-rendered shells, login walls, or 404 placeholders.
const FULLTEXT_MIN_LENGTH: usize = 200;

/// Number of leading bytes of the response we sniff for `<meta charset="…">`
/// when the Content-Type header doesn't carry one. The HTML5 spec mandates
/// any in-document declaration must appear in the first 1024 bytes of the
/// document, so reading more is just wasted work.
const META_SNIFF_BYTES: usize = 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Feed {
    pub url: String,
    pub title: String,
    pub items: Vec<FeedItem>,
    #[serde(skip)]
    pub title_lower: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FeedItem {
    pub title: String,
    pub link: Option<String>,
    pub description: Option<String>,
    pub pub_date: Option<String>,
    pub author: Option<String>,
    pub formatted_date: Option<String>,
    #[serde(skip)]
    pub parsed_date: Option<DateTime<Utc>>,
    #[serde(skip)]
    pub plain_text: Option<String>,
    #[serde(skip)]
    pub title_lower: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeedCategory {
    pub id: String,
    pub name: String,
    pub feeds: HashSet<String>, // URLs of feeds in this category, using HashSet for faster lookup
    pub expanded: bool,         // UI state: whether the category is expanded in the UI
}

impl FeedCategory {
    pub fn new(name: &str) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            feeds: HashSet::new(),
            expanded: true,
        }
    }

    pub fn add_feed(&mut self, url: &str) {
        self.feeds.insert(url.to_string());
    }

    pub fn remove_feed(&mut self, url: &str) -> bool {
        self.feeds.remove(url)
    }

    pub fn contains_feed(&self, url: &str) -> bool {
        self.feeds.contains(url)
    }

    pub fn feed_count(&self) -> usize {
        self.feeds.len()
    }

    pub fn rename(&mut self, new_name: &str) {
        self.name = new_name.to_string();
    }

    pub fn toggle_expanded(&mut self) {
        self.expanded = !self.expanded;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeedType {
    Rss,
    Atom,
}

impl fmt::Display for FeedType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FeedType::Rss => write!(f, "RSS"),
            FeedType::Atom => write!(f, "Atom"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DiscoveredFeed {
    pub url: String,
    pub title: String,
    pub feed_type: FeedType,
}

/// Outcome of a successful Readability extraction against an article URL.
/// Decoupled from `dom_smoothie::Article` so the upstream crate can be
/// swapped without rippling through the rest of the codebase.
#[derive(Clone, Debug)]
pub struct ExtractedArticle {
    pub title: String,
    pub plain_text: String,
    pub byline: Option<String>,
    pub site_name: Option<String>,
    pub source_url: String,
}

/// Validates that `url` is safe to fetch on the *auto-fulltext* path (i.e.
/// without a user click). Rejects non-http(s) schemes and hostnames that
/// resolve to private / loopback / link-local addresses, since auto-mode
/// fetches whatever the feed XML put in `<link>` — a hostile feed could
/// otherwise probe the user's internal network. Manual `Shift+F` is the
/// user's explicit action and bypasses this check.
pub fn is_safe_auto_url(url: &str) -> bool {
    let parsed = match Url::parse(url) {
        Ok(u) => u,
        Err(_) => return false,
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    let host = match parsed.host() {
        Some(h) => h,
        None => return false,
    };
    match host {
        url::Host::Ipv4(ip) => is_global_ip(IpAddr::V4(ip)),
        url::Host::Ipv6(ip) => is_global_ip(IpAddr::V6(ip)),
        url::Host::Domain(name) => {
            // Reject obvious local-only names; a hostile feed cannot use
            // these to probe RFC1918 space, but we still want to avoid
            // hitting "localhost"-style endpoints by accident.
            let n = name.to_ascii_lowercase();
            if n == "localhost" || n.ends_with(".localhost") || n.ends_with(".local") {
                return false;
            }
            // DNS resolution itself happens later in reqwest; we don't
            // resolve here because that would block. The intent of the
            // check is "no obvious internal target encoded directly in
            // the URL" — DNS rebinding is out of scope for a feed reader.
            true
        }
    }
}

fn is_global_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.octets()[0] == 0
                // CGNAT 100.64.0.0/10
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64)
                // 169.254/16 is link_local; also reject the carrier-grade
                // /15 and the 192.0.0.0/24 IETF protocol-assignments block.
                || (v4.octets()[0] == 192
                    && v4.octets()[1] == 0
                    && v4.octets()[2] == 0))
        }
        IpAddr::V6(v6) => {
            let seg0 = v6.segments()[0];
            let seg1 = v6.segments()[1];
            !(v6.is_loopback()
                || v6.is_unspecified()
                // unique-local fc00::/7
                || (seg0 & 0xFE00) == 0xFC00
                // link-local fe80::/10
                || (seg0 & 0xFFC0) == 0xFE80
                // deprecated site-local fec0::/10
                || (seg0 & 0xFFC0) == 0xFEC0
                // multicast ff00::/8
                || (seg0 & 0xFF00) == 0xFF00
                // 6to4 2002::/16 — routes to embedded IPv4 via 6to4 relays,
                // so an address like 2002:a9fe:a9fe:: would reach 169.254.169.254
                || seg0 == 0x2002
                // NAT64 well-known prefix 64:ff9b::/96
                || (seg0 == 0x0064 && seg1 == 0xff9b)
                // IPv4-mapped — defer to v4 check via the embedded address
                || v6.to_ipv4_mapped().map(|v4| !is_global_ip(IpAddr::V4(v4))).unwrap_or(false))
        }
    }
}

/// Fetch `url` with `client` and run Mozilla-Readability extraction over the
/// returned HTML. Feed auth headers are intentionally NOT propagated: the
/// article URL is on a different host and may be third-party, so leaking
/// per-feed `Authorization` headers would be a credential leak.
///
/// Rejects non-`text/html` responses, oversized bodies (`FULLTEXT_MAX_BYTES`),
/// and pages whose extracted text falls below `FULLTEXT_MIN_LENGTH` (treated
/// as JS-rendered or empty). Body is read with a size-bounded reader so peak
/// allocation cannot exceed the cap, and the response charset is honored so
/// non-UTF8 pages decode correctly.
pub fn extract_article(
    url: &str,
    client: &reqwest::blocking::Client,
    user_agent: Option<&str>,
) -> Result<ExtractedArticle> {
    // Scheme allowlist — reqwest's blocking client refuses non-http(s) by
    // default, but this gives a clearer error and makes the policy explicit.
    let parsed = Url::parse(url).context("Article URL is not a valid URL")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(anyhow::anyhow!(
            "Article URL scheme '{}' is not allowed (only http/https)",
            parsed.scheme()
        ));
    }

    let default_user_agent =
        "Mozilla/5.0 (compatible; Feedr/1.0; +https://github.com/bahdotsh/feedr)";
    let ua = user_agent.unwrap_or(default_user_agent);

    let response = client
        .get(url)
        .header("User-Agent", ua)
        .header("Accept", "text/html, application/xhtml+xml, */*;q=0.5")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Accept-Encoding", "gzip, deflate")
        .send()
        .context("Failed to fetch article")?;

    let status = response.status();
    if !status.is_success() {
        return Err(anyhow::anyhow!(
            "HTTP error {} fetching article from {}",
            status,
            url
        ));
    }

    let content_type_raw = response
        .headers()
        .get("content-type")
        .and_then(|ct| ct.to_str().ok())
        .unwrap_or("")
        .to_string();
    let content_type = content_type_raw.to_lowercase();
    if !content_type.is_empty()
        && !content_type.contains("text/html")
        && !content_type.contains("application/xhtml")
    {
        return Err(anyhow::anyhow!(
            "Article URL did not return HTML (content-type: {})",
            content_type
        ));
    }

    // Reject upfront if Content-Length already exceeds the cap — saves us
    // the full transfer for obviously oversized responses.
    if let Some(declared_len) = response
        .headers()
        .get("content-length")
        .and_then(|cl| cl.to_str().ok())
        .and_then(|cl| cl.parse::<usize>().ok())
    {
        if declared_len > FULLTEXT_MAX_BYTES {
            return Err(anyhow::anyhow!(
                "Article body too large ({} bytes declared, cap {} bytes)",
                declared_len,
                FULLTEXT_MAX_BYTES
            ));
        }
    }

    let final_url = response.url().to_string();

    // Read at most FULLTEXT_MAX_BYTES + 1 so we can detect overflow without
    // ever allocating beyond cap+1. `reqwest::blocking::Response` implements
    // `Read`, so `.take()` gives us a hard cap on input consumed.
    let mut bytes = Vec::with_capacity(64 * 1024);
    let mut reader = response.take((FULLTEXT_MAX_BYTES as u64) + 1);
    reader
        .read_to_end(&mut bytes)
        .context("Failed to read article body")?;
    if bytes.len() > FULLTEXT_MAX_BYTES {
        return Err(anyhow::anyhow!(
            "Article body too large (exceeded cap {} bytes)",
            FULLTEXT_MAX_BYTES
        ));
    }

    let html = decode_html_bytes(&bytes, &content_type_raw);
    extract_from_html(&html, &final_url)
}

/// Decode `bytes` into a Rust `String` using the encoding declared by the
/// `Content-Type` header, falling back to `<meta charset>` sniffing in the
/// first ~1 KB of the document, and finally to UTF-8. Without this, any
/// non-UTF8 page (Windows-1252, ISO-8859-1, Shift_JIS, GBK, …) produces
/// U+FFFD-laced mojibake that Readability would then misclassify as too
/// short.
pub(crate) fn decode_html_bytes(bytes: &[u8], content_type: &str) -> String {
    let mut encoding: Option<&'static encoding_rs::Encoding> = None;

    // 1) Honor charset= in the Content-Type header.
    if let Some(label) = charset_from_content_type(content_type) {
        encoding = encoding_rs::Encoding::for_label(label.as_bytes());
    }

    // 2) Otherwise sniff the leading bytes for an in-document <meta charset>.
    if encoding.is_none() {
        let sniff_len = bytes.len().min(META_SNIFF_BYTES);
        if let Some(label) = sniff_meta_charset(&bytes[..sniff_len]) {
            encoding = encoding_rs::Encoding::for_label(label.as_bytes());
        }
    }

    // 3) Default to UTF-8 — same fate `from_utf8_lossy` would have given us,
    //    but routed through encoding_rs so behavior is uniform.
    let encoding = encoding.unwrap_or(encoding_rs::UTF_8);
    let (cow, _enc, _had_errors) = encoding.decode(bytes);
    cow.into_owned()
}

fn charset_from_content_type(content_type: &str) -> Option<String> {
    for part in content_type.split(';') {
        let lower = part.trim().to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("charset=") {
            let v = rest.trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Tiny sniffer for `<meta charset="…">` or `<meta http-equiv="Content-Type"
/// content="…charset=…">`. Stays byte-level (latin-1 lossy decode is fine
/// here: ASCII-superset encodings agree on the bytes we care about) so we
/// don't have to pick an encoding before we know one.
fn sniff_meta_charset(head: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(head).to_ascii_lowercase();
    // <meta charset="utf-8">
    if let Some(idx) = s.find("charset") {
        let after = &s[idx + "charset".len()..];
        // Skip optional `=`, whitespace, optional quote
        let after = after.trim_start();
        let after = after.strip_prefix('=').unwrap_or(after).trim_start();
        let after = after
            .strip_prefix('"')
            .or_else(|| after.strip_prefix('\''))
            .unwrap_or(after);
        let end = after
            .find(['"', '\'', ' ', '>', ';', '/'])
            .unwrap_or(after.len());
        let label = &after[..end];
        if !label.is_empty() {
            return Some(label.to_string());
        }
    }
    None
}

/// Pure Readability step — no I/O. Split out so unit tests can exercise the
/// extraction + min-length + error-path logic without a network round-trip.
pub fn extract_from_html(html: &str, source_url: &str) -> Result<ExtractedArticle> {
    let cfg = ReadabilityConfig {
        max_elements_to_parse: 9000,
        ..ReadabilityConfig::default()
    };
    let mut readability = Readability::new(html, Some(source_url), Some(cfg))
        .context("Failed to initialize Readability")?;
    let article = readability
        .parse()
        .context("Readability could not parse article")?;

    if article.length < FULLTEXT_MIN_LENGTH {
        return Err(anyhow::anyhow!(
            "Page appears empty (only {} chars of body) — likely JS-rendered or paywalled",
            article.length
        ));
    }

    Ok(ExtractedArticle {
        title: article.title,
        plain_text: article.text_content.to_string(),
        byline: article.byline,
        site_name: article.site_name,
        source_url: source_url.to_string(),
    })
}

/// Result of fetching a URL that was expected to be a feed.
pub enum FeedFetchResult {
    /// Successfully parsed as an RSS/Atom feed.
    Feed(Feed),
    /// The URL returned an HTML page; any discovered feed links are included.
    DiscoveredFeeds {
        feeds: Vec<DiscoveredFeed>,
        page_url: String,
    },
}

impl FeedFetchResult {
    /// Convert to a `Feed`, returning an error if this was an HTML page.
    pub fn into_feed(self) -> Result<Feed> {
        match self {
            FeedFetchResult::Feed(feed) => Ok(feed),
            FeedFetchResult::DiscoveredFeeds { feeds, page_url } => {
                if feeds.is_empty() {
                    Err(anyhow::anyhow!(
                        "No RSS/Atom feed links found on this page: {}",
                        page_url
                    ))
                } else {
                    Err(anyhow::anyhow!(
                        "URL is an HTML page, not a feed. {} feed link(s) found on: {}",
                        feeds.len(),
                        page_url
                    ))
                }
            }
        }
    }
}

pub fn discover_feeds_from_html(html: &[u8], base_url: &Url) -> Vec<DiscoveredFeed> {
    static SELECTOR: OnceLock<Selector> = OnceLock::new();
    let selector = SELECTOR.get_or_init(|| Selector::parse("link[rel=alternate]").unwrap());

    let html_str = String::from_utf8_lossy(html);
    let document = Html::parse_document(&html_str);

    let mut seen_urls = HashSet::new();
    let mut feeds = Vec::new();

    for element in document.select(selector) {
        let link_type = match element.value().attr("type") {
            Some(t) => t.to_lowercase(),
            None => continue,
        };

        let feed_type = if link_type == "application/rss+xml" {
            FeedType::Rss
        } else if link_type == "application/atom+xml" {
            FeedType::Atom
        } else {
            continue;
        };

        let href = match element.value().attr("href") {
            Some(h) if !h.is_empty() => h,
            _ => continue,
        };

        let resolved = match base_url.join(href) {
            Ok(u) => u.to_string(),
            Err(_) => continue,
        };

        if !seen_urls.insert(resolved.clone()) {
            continue;
        }

        let title = element
            .value()
            .attr("title")
            .filter(|t| !t.is_empty())
            .unwrap_or(&resolved)
            .to_string();

        feeds.push(DiscoveredFeed {
            url: resolved,
            title,
            feed_type,
        });
    }

    feeds
}

impl Feed {
    /// Fetch and parse a feed from a URL with default timeout
    pub fn from_url(url: &str) -> Result<Self> {
        Self::from_url_with_config(url, 15, None, None)
    }

    /// Fetch and parse a feed from a URL with custom timeout
    pub fn from_url_with_timeout(url: &str, timeout_secs: u64) -> Result<Self> {
        Self::from_url_with_config(url, timeout_secs, None, None)
    }

    /// Fetch and parse a feed from a URL with custom timeout and user agent.
    /// Returns an error if the URL is an HTML page (use `fetch_url` for discovery support).
    pub fn from_url_with_config(
        url: &str,
        timeout_secs: u64,
        user_agent: Option<&str>,
        custom_headers: Option<&HashMap<String, String>>,
    ) -> Result<Self> {
        let client = Self::build_client(timeout_secs)?;
        Self::fetch_url(url, &client, user_agent, custom_headers)?.into_feed()
    }

    /// Build a shared HTTP client with the given timeout
    pub fn build_client(timeout_secs: u64) -> Result<reqwest::blocking::Client> {
        reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(10))
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .context("Failed to create HTTP client")
    }

    /// Build an HTTP client whose redirect policy re-validates each hop with
    /// `is_safe_auto_url`. Used for the auto-fulltext path so a hostile feed
    /// can't publish a public-looking `<link>` URL that 302s into the user's
    /// internal network (e.g. RFC1918 / loopback / cloud metadata endpoints).
    /// The redirect chain is still capped at 10 hops, matching `build_client`.
    pub fn build_safe_redirect_client(timeout_secs: u64) -> Result<reqwest::blocking::Client> {
        reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 10 {
                    return attempt.stop();
                }
                if !is_safe_auto_url(attempt.url().as_str()) {
                    return attempt.stop();
                }
                attempt.follow()
            }))
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .context("Failed to create safe-redirect HTTP client")
    }

    /// Fetch a URL and return either a parsed feed or discovered feed links.
    pub fn fetch_url(
        url: &str,
        client: &reqwest::blocking::Client,
        user_agent: Option<&str>,
        custom_headers: Option<&HashMap<String, String>>,
    ) -> Result<FeedFetchResult> {
        let default_user_agent =
            "Mozilla/5.0 (compatible; Feedr/1.0; +https://github.com/bahdotsh/feedr)";
        let ua = user_agent.unwrap_or(default_user_agent);

        let mut request = client
            .get(url)
            .header("User-Agent", ua)
            .header(
                "Accept",
                "application/rss+xml, application/atom+xml, application/xml, text/xml, */*",
            )
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Accept-Encoding", "gzip, deflate")
            .header("Cache-Control", "no-cache")
            .header("Connection", "keep-alive");

        if let Some(headers) = custom_headers {
            for (key, value) in headers {
                request = request.header(key, value);
            }
        }

        let response = request.send().context("Failed to fetch feed")?;

        // Check if we got redirected or have an unusual status
        let final_url = response.url().clone();
        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|ct| ct.to_str().ok())
            .unwrap_or("unknown")
            .to_lowercase();

        if !status.is_success() {
            return Err(anyhow::anyhow!(
                "HTTP error {}: Failed to fetch feed from {}",
                status,
                url
            ));
        }

        let content = response.bytes().context("Failed to read response body")?;

        // Reject suspiciously short responses (likely empty/error pages)
        if content.len() < 100 {
            return Err(anyhow::anyhow!(
                "Response too short ({} bytes), might be empty or an error page",
                content.len()
            ));
        }

        // Try parsing as feed first — some servers serve valid feeds with text/html content-type
        let feed = match parser::parse(&content[..]) {
            Ok(f) => f,
            Err(parse_err) => {
                // Parse failed — check if this looks like HTML and try feed discovery
                let content_start =
                    String::from_utf8_lossy(&content[..std::cmp::min(200, content.len())]);
                let trimmed_lower = content_start.trim_start().to_lowercase();
                if content_type.contains("text/html")
                    || trimmed_lower.starts_with("<!doctype html")
                    || trimmed_lower.starts_with("<html")
                {
                    let discovered = discover_feeds_from_html(&content, &final_url);
                    return Ok(FeedFetchResult::DiscoveredFeeds {
                        feeds: discovered,
                        page_url: final_url.to_string(),
                    });
                }

                let content_preview =
                    String::from_utf8_lossy(&content[..std::cmp::min(300, content.len())]);
                return Err(parse_err).with_context(|| {
                    format!(
                        "Failed to parse feed (RSS/Atom) from URL: {} (final URL: {}, {} bytes, content-type: {}, preview: {})",
                        url, final_url, content.len(), content_type, content_preview.trim()
                    )
                });
            }
        };

        let items = feed.entries.iter().map(FeedItem::from_feed_entry).collect();

        let title = feed
            .title
            .map(|t| t.content)
            .unwrap_or_else(|| "Untitled Feed".to_string());
        let title_lower = title.to_lowercase();

        Ok(FeedFetchResult::Feed(Feed {
            url: url.to_string(),
            title,
            items,
            title_lower,
        }))
    }
}

impl FeedItem {
    fn from_feed_entry(entry: &feed_rs::model::Entry) -> Self {
        // Extract publication date - try multiple date formats
        let (pub_date_string, formatted_date, parsed_date) =
            if let Some(published) = &entry.published {
                let pub_string = published.to_rfc3339();
                let formatted = format_date(*published);
                (Some(pub_string), Some(formatted), Some(*published))
            } else if let Some(updated) = &entry.updated {
                let pub_string = updated.to_rfc3339();
                let formatted = format_date(*updated);
                (Some(pub_string), Some(formatted), Some(*updated))
            } else {
                (None, None, None)
            };

        // Extract author information
        let author = entry.authors.first().map(|author| {
            if !author.name.is_empty() {
                author.name.clone()
            } else if let Some(email) = &author.email {
                email.clone()
            } else {
                "Unknown".to_string()
            }
        });

        // Extract content/description - prefer content over summary
        let description = if let Some(content) = entry.content.as_ref() {
            Some(content.body.clone().unwrap_or_default())
        } else {
            entry
                .summary
                .as_ref()
                .map(|summary| summary.content.clone())
        };

        // Cache plain text from description (avoids repeated HTML parsing)
        let plain_text = description
            .as_ref()
            .map(|desc| html2text::from_read(desc.as_bytes(), 80));

        // Extract the primary link
        let link = entry.links.first().map(|link| link.href.clone());

        let title = entry
            .title
            .as_ref()
            .map(|t| t.content.clone())
            .unwrap_or_else(|| "Untitled".to_string());
        let title_lower = title.to_lowercase();

        FeedItem {
            title,
            link,
            description,
            pub_date: pub_date_string,
            author,
            formatted_date,
            parsed_date,
            plain_text,
            title_lower,
        }
    }
}

fn format_date(dt: DateTime<Utc>) -> String {
    // Calculate how long ago the item was published
    let now = Utc::now();
    let diff = now.signed_duration_since(dt);

    if diff.num_minutes() < 60 {
        format!("{} minutes ago", diff.num_minutes())
    } else if diff.num_hours() < 24 {
        format!("{} hours ago", diff.num_hours())
    } else if diff.num_days() < 7 {
        format!("{} days ago", diff.num_days())
    } else {
        // For older items, show the actual date
        dt.format("%B %d, %Y").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discover_single_rss_feed() {
        let html = br#"<html><head>
            <link rel="alternate" type="application/rss+xml" title="My Blog" href="/feed.xml">
        </head><body></body></html>"#;
        let base = Url::parse("https://example.com/blog/").unwrap();
        let feeds = discover_feeds_from_html(html, &base);
        assert_eq!(feeds.len(), 1);
        assert_eq!(feeds[0].url, "https://example.com/feed.xml");
        assert_eq!(feeds[0].title, "My Blog");
        assert_eq!(feeds[0].feed_type, FeedType::Rss);
    }

    #[test]
    fn test_discover_multiple_feeds() {
        let html = br#"<html><head>
            <link rel="alternate" type="application/rss+xml" title="RSS Feed" href="/rss.xml">
            <link rel="alternate" type="application/atom+xml" title="Atom Feed" href="/atom.xml">
        </head><body></body></html>"#;
        let base = Url::parse("https://example.com/").unwrap();
        let feeds = discover_feeds_from_html(html, &base);
        assert_eq!(feeds.len(), 2);
        assert_eq!(feeds[0].feed_type, FeedType::Rss);
        assert_eq!(feeds[1].feed_type, FeedType::Atom);
    }

    #[test]
    fn test_discover_no_feeds() {
        let html = br#"<html><head><title>No feeds</title></head><body></body></html>"#;
        let base = Url::parse("https://example.com/").unwrap();
        let feeds = discover_feeds_from_html(html, &base);
        assert!(feeds.is_empty());
    }

    #[test]
    fn test_discover_relative_url_resolution() {
        let html = br#"<html><head>
            <link rel="alternate" type="application/rss+xml" href="feed.xml">
        </head><body></body></html>"#;
        let base = Url::parse("https://example.com/blog/page").unwrap();
        let feeds = discover_feeds_from_html(html, &base);
        assert_eq!(feeds[0].url, "https://example.com/blog/feed.xml");
    }

    #[test]
    fn test_discover_absolute_url_preserved() {
        let html = br#"<html><head>
            <link rel="alternate" type="application/rss+xml" href="https://feeds.example.com/rss">
        </head><body></body></html>"#;
        let base = Url::parse("https://example.com/").unwrap();
        let feeds = discover_feeds_from_html(html, &base);
        assert_eq!(feeds[0].url, "https://feeds.example.com/rss");
    }

    #[test]
    fn test_discover_missing_title_falls_back_to_url() {
        let html = br#"<html><head>
            <link rel="alternate" type="application/rss+xml" href="/feed.xml">
        </head><body></body></html>"#;
        let base = Url::parse("https://example.com/").unwrap();
        let feeds = discover_feeds_from_html(html, &base);
        assert_eq!(feeds[0].title, "https://example.com/feed.xml");
    }

    #[test]
    fn test_discover_deduplicates() {
        let html = br#"<html><head>
            <link rel="alternate" type="application/rss+xml" title="Feed" href="/feed.xml">
            <link rel="alternate" type="application/rss+xml" title="Same Feed" href="/feed.xml">
        </head><body></body></html>"#;
        let base = Url::parse("https://example.com/").unwrap();
        let feeds = discover_feeds_from_html(html, &base);
        assert_eq!(feeds.len(), 1);
    }

    #[test]
    fn test_discover_case_insensitive_type() {
        let html = br#"<html><head>
            <link rel="alternate" type="Application/RSS+XML" href="/feed.xml">
        </head><body></body></html>"#;
        let base = Url::parse("https://example.com/").unwrap();
        let feeds = discover_feeds_from_html(html, &base);
        assert_eq!(feeds.len(), 1);
    }

    #[test]
    fn test_discover_lowercase_doctype() {
        let html = br#"<!doctype html><html><head>
            <link rel="alternate" type="application/rss+xml" title="Feed" href="/feed.xml">
        </head><body></body></html>"#;
        let base = Url::parse("https://example.com/").unwrap();
        let feeds = discover_feeds_from_html(html, &base);
        assert_eq!(feeds.len(), 1);
        assert_eq!(feeds[0].url, "https://example.com/feed.xml");
    }

    #[test]
    fn test_into_feed_with_feed_variant() {
        let feed = Feed {
            url: "https://example.com/feed.xml".to_string(),
            title: "Test Feed".to_string(),
            items: vec![],
            title_lower: "test feed".to_string(),
        };
        let result = FeedFetchResult::Feed(feed);
        let feed = result.into_feed().unwrap();
        assert_eq!(feed.url, "https://example.com/feed.xml");
        assert_eq!(feed.title, "Test Feed");
    }

    #[test]
    fn test_into_feed_with_discovered_feeds_returns_error() {
        let result = FeedFetchResult::DiscoveredFeeds {
            feeds: vec![DiscoveredFeed {
                url: "https://example.com/rss".to_string(),
                title: "RSS Feed".to_string(),
                feed_type: FeedType::Rss,
            }],
            page_url: "https://example.com/".to_string(),
        };
        let err = result.into_feed().unwrap_err();
        assert!(err.to_string().contains("HTML page"));
        assert!(err.to_string().contains("1 feed link(s)"));
    }

    #[test]
    fn test_into_feed_with_empty_discovered_feeds_returns_error() {
        let result = FeedFetchResult::DiscoveredFeeds {
            feeds: vec![],
            page_url: "https://example.com/".to_string(),
        };
        let err = result.into_feed().unwrap_err();
        assert!(err.to_string().contains("No RSS/Atom feed links found"));
    }

    /// A blog-post-shaped fixture long enough to clear FULLTEXT_MIN_LENGTH
    /// once Readability strips boilerplate.
    fn fixture_article_html() -> String {
        let body = "Feedr is a terminal-based RSS feed reader. It supports \
            categories, OPML import, and a configurable keybinding system. \
            This paragraph is repeated several times to ensure the extracted \
            text content comfortably exceeds the minimum-length threshold \
            that the extractor enforces before declaring the page empty. ";
        let mut content = String::new();
        for _ in 0..5 {
            content.push_str("<p>");
            content.push_str(body);
            content.push_str("</p>");
        }
        format!(
            r#"<!doctype html><html><head>
                <title>About Feedr</title>
                <meta name="author" content="Test Author">
            </head><body>
                <nav>Home About Contact</nav>
                <article>
                    <h1>About Feedr</h1>
                    {}
                </article>
                <footer>Copyright 2026</footer>
            </body></html>"#,
            content
        )
    }

    #[test]
    fn test_extract_from_html_succeeds_on_blog_shaped_page() {
        let html = fixture_article_html();
        let article = extract_from_html(&html, "https://example.com/post").unwrap();
        assert!(!article.title.is_empty(), "title should be extracted");
        assert!(
            article.plain_text.len() >= FULLTEXT_MIN_LENGTH,
            "extracted text below minimum: {}",
            article.plain_text.len()
        );
        assert_eq!(article.source_url, "https://example.com/post");
    }

    #[test]
    fn test_extract_from_html_rejects_short_content() {
        let html = r#"<!doctype html><html><body><p>tiny.</p></body></html>"#;
        let err = extract_from_html(html, "https://example.com/empty").unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("empty"),
            "expected 'empty' in error, got: {}",
            err
        );
    }

    #[test]
    fn test_extract_from_html_rejects_bad_document_url() {
        let html = fixture_article_html();
        let err = extract_from_html(&html, "not-a-url").unwrap_err();
        let msg = format!("{:?}", err);
        // dom_smoothie's BadDocumentURL wrapped by our context string.
        assert!(
            msg.contains("Failed to initialize") || msg.to_lowercase().contains("url"),
            "expected BadDocumentURL error, got: {}",
            msg
        );
    }

    #[test]
    fn test_decode_html_bytes_uses_content_type_charset() {
        // "Café" in Windows-1252: 0x43 0x61 0x66 0xE9
        let bytes: &[u8] = &[0x43, 0x61, 0x66, 0xE9];
        let decoded = decode_html_bytes(bytes, "text/html; charset=windows-1252");
        assert_eq!(decoded, "Café");
    }

    #[test]
    fn test_decode_html_bytes_handles_quoted_charset_in_header() {
        let bytes: &[u8] = &[0x43, 0x61, 0x66, 0xE9];
        let decoded = decode_html_bytes(bytes, "text/html; charset=\"windows-1252\"");
        assert_eq!(decoded, "Café");
    }

    #[test]
    fn test_decode_html_bytes_falls_back_to_meta_charset() {
        // Document declares Shift_JIS via <meta>; Content-Type omits charset.
        // Bytes "日本" in Shift_JIS: 0x93 0xFA 0x96 0x7B.
        let mut html: Vec<u8> = b"<html><head><meta charset=\"shift_jis\"></head><body>".to_vec();
        html.extend_from_slice(&[0x93, 0xFA, 0x96, 0x7B]);
        html.extend_from_slice(b"</body></html>");
        let decoded = decode_html_bytes(&html, "text/html");
        assert!(
            decoded.contains("日本"),
            "expected meta-sniffed Shift_JIS to decode, got: {}",
            decoded
        );
    }

    #[test]
    fn test_decode_html_bytes_defaults_to_utf8() {
        let html = "<html><body>Café</body></html>";
        let decoded = decode_html_bytes(html.as_bytes(), "");
        assert!(decoded.contains("Café"));
    }

    #[test]
    fn test_is_safe_auto_url_accepts_public_https() {
        assert!(is_safe_auto_url("https://example.com/article"));
        assert!(is_safe_auto_url("http://example.com/article"));
    }

    #[test]
    fn test_is_safe_auto_url_rejects_non_http_scheme() {
        assert!(!is_safe_auto_url("file:///etc/passwd"));
        assert!(!is_safe_auto_url("ftp://example.com/x"));
        assert!(!is_safe_auto_url("javascript:alert(1)"));
        assert!(!is_safe_auto_url("data:text/html,<script>"));
    }

    #[test]
    fn test_is_safe_auto_url_rejects_private_ips() {
        // RFC1918
        assert!(!is_safe_auto_url("http://10.0.0.1/x"));
        assert!(!is_safe_auto_url("http://192.168.1.1/x"));
        assert!(!is_safe_auto_url("http://172.16.0.1/x"));
        // Loopback
        assert!(!is_safe_auto_url("http://127.0.0.1/x"));
        // Link-local (incl. AWS metadata endpoint)
        assert!(!is_safe_auto_url(
            "http://169.254.169.254/latest/meta-data/"
        ));
        // CGNAT
        assert!(!is_safe_auto_url("http://100.64.0.1/x"));
        // IPv6 loopback / unique-local / link-local
        assert!(!is_safe_auto_url("http://[::1]/x"));
        assert!(!is_safe_auto_url("http://[fc00::1]/x"));
        assert!(!is_safe_auto_url("http://[fe80::1]/x"));
        // IPv6 deprecated site-local fec0::/10
        assert!(!is_safe_auto_url("http://[fec0::1]/x"));
        // IPv6 multicast ff00::/8
        assert!(!is_safe_auto_url("http://[ff02::1]/x"));
        // 6to4 wrapping link-local — would route via 6to4 relay to
        // 169.254.169.254 (cloud metadata).
        assert!(!is_safe_auto_url("http://[2002:a9fe:a9fe::]/x"));
        // NAT64 well-known prefix 64:ff9b::/96
        assert!(!is_safe_auto_url("http://[64:ff9b::a9fe:a9fe]/x"));
    }

    #[test]
    fn test_is_safe_auto_url_rejects_localhost_names() {
        assert!(!is_safe_auto_url("http://localhost/x"));
        assert!(!is_safe_auto_url("http://printer.local/x"));
        assert!(!is_safe_auto_url("http://api.localhost/x"));
    }

    #[test]
    fn test_is_safe_auto_url_rejects_invalid_url() {
        assert!(!is_safe_auto_url("not-a-url"));
        assert!(!is_safe_auto_url(""));
    }

    #[test]
    fn test_extract_article_rejects_non_http_scheme() {
        // Build a client; the function should reject before any I/O.
        let client = reqwest::blocking::Client::new();
        let err = extract_article("file:///etc/passwd", &client, None).unwrap_err();
        assert!(
            err.to_string().contains("scheme") || err.to_string().contains("not allowed"),
            "expected scheme rejection, got: {}",
            err
        );
    }
}
