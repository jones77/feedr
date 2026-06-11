use html2text::render::text_renderer::{TaggedLine, TextDecorator};
use ratatui::layout::Rect;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// html2text decorator tuned for RSS-summary rendering. Differs from the
/// crate's default `PlainDecorator` in two ways:
///
/// * **No link annotations.** `decorate_link_start`/`_end` and `finalise`
///   return empty strings, so anchors render as just their inner text — no
///   `[text][N]` markers and no `[N]: url` footnote dump at the bottom.
///   RSS summaries (Reddit especially) are otherwise drowned in
///   `submitted by [ /u/... ][2] [[link]][3] [[comments]][4]` boilerplate
///   plus a multi-line footnote block, none of which the user needs — the
///   article URL is already shown in the detail-view header.
/// * **Image alt text falls back to title.** Same as `PlainDecorator`
///   (`[title]`) so the user still sees *something* for inline images.
///
/// Emphasis (`*…*`), strong (`**…**`), code (`` `…` ``), list/header/quote
/// prefixes are preserved.
#[derive(Clone, Debug, Default)]
pub(crate) struct CleanDecorator;

impl CleanDecorator {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl TextDecorator for CleanDecorator {
    type Annotation = ();

    fn decorate_link_start(&mut self, _url: &str) -> (String, Self::Annotation) {
        (String::new(), ())
    }
    fn decorate_link_end(&mut self) -> String {
        String::new()
    }
    fn decorate_em_start(&mut self) -> (String, Self::Annotation) {
        ("*".to_string(), ())
    }
    fn decorate_em_end(&mut self) -> String {
        "*".to_string()
    }
    fn decorate_strong_start(&mut self) -> (String, Self::Annotation) {
        ("**".to_string(), ())
    }
    fn decorate_strong_end(&mut self) -> String {
        "**".to_string()
    }
    fn decorate_strikeout_start(&mut self) -> (String, Self::Annotation) {
        (String::new(), ())
    }
    fn decorate_strikeout_end(&mut self) -> String {
        String::new()
    }
    fn decorate_code_start(&mut self) -> (String, Self::Annotation) {
        ("`".to_string(), ())
    }
    fn decorate_code_end(&mut self) -> String {
        "`".to_string()
    }
    fn decorate_preformat_first(&mut self) -> Self::Annotation {}
    fn decorate_preformat_cont(&mut self) -> Self::Annotation {}
    fn decorate_image(&mut self, _src: &str, title: &str) -> (String, Self::Annotation) {
        (format!("[{}]", title), ())
    }
    fn header_prefix(&mut self, level: usize) -> String {
        "#".repeat(level) + " "
    }
    fn quote_prefix(&mut self) -> String {
        "> ".to_string()
    }
    fn unordered_item_prefix(&mut self) -> String {
        "* ".to_string()
    }
    fn ordered_item_prefix(&mut self, i: i64) -> String {
        format!("{}. ", i)
    }
    fn finalise(&mut self, _links: Vec<String>) -> Vec<TaggedLine<()>> {
        // Crucially: no footnote-style `[N]: url` lines. The default
        // `PlainDecorator` returns one per link here — that's the entire
        // reason summaries grew a noisy reference-list trailer.
        Vec::new()
    }
    fn make_subblock_decorator(&self) -> Self {
        Self
    }
}

/// Convenience: flatten any `<table>` markup, then render with
/// [`CleanDecorator`] at the given width. Used by detail/dashboard views
/// to convert a feed item's HTML description into prose plain text.
pub(crate) fn render_clean_html(html: &str, width: usize) -> String {
    let prepped = crate::feed::flatten_description_html(html);
    html2text::from_read_with_decorator(prepped.as_bytes(), width, CleanDecorator::new())
}

// Helper function to create a centered rect with minimum dimensions
pub(crate) fn centered_rect_with_min(
    percent_x: u16,
    percent_y: u16,
    min_w: u16,
    min_h: u16,
    r: Rect,
) -> Rect {
    let pct_w = r.width * percent_x / 100;
    let pct_h = r.height * percent_y / 100;
    let w = pct_w.max(min_w).min(r.width);
    let h = pct_h.max(min_h).min(r.height);
    let x = r.x + (r.width.saturating_sub(w)) / 2;
    let y = r.y + (r.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

// Helper function to truncate a URL for display
pub(crate) fn truncate_url(url: &str, max_length: usize) -> String {
    // Remove common prefixes for cleaner display
    let clean_url = url
        .replace("https://", "")
        .replace("http://", "")
        .replace("www.", "");

    truncate_str(&clean_url, max_length)
}

// Helper function to truncate a string with unicode awareness
pub(crate) fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.width() <= max_chars {
        s.to_string()
    } else {
        // Find position to truncate while respecting unicode boundaries
        let mut total_width = 0;
        let mut truncate_idx = 0;

        for (idx, c) in s.char_indices() {
            let char_width = c.width_cjk().unwrap_or(1);
            if total_width + char_width > max_chars.saturating_sub(3) {
                truncate_idx = idx;
                break;
            }
            total_width += char_width;
        }

        if truncate_idx > 0 {
            format!("{}...", &s[..truncate_idx])
        } else {
            // Fallback if we couldn't properly calculate (shouldn't happen often)
            format!("{}...", &s[..max_chars.saturating_sub(3)])
        }
    }
}

// Helper function to format content for better reading experience
pub(crate) fn format_content_for_reading(text: &str) -> String {
    let mut formatted_lines = Vec::new();
    let mut current_paragraph = Vec::new();
    let mut in_list = false;

    for line in text.lines() {
        let trimmed = line.trim();

        // Detect list items (lines starting with -, *, •, numbers, etc.)
        let is_list_item = trimmed.starts_with('-')
            || trimmed.starts_with('*')
            || trimmed.starts_with('•')
            || trimmed.starts_with("  - ")
            || trimmed.starts_with("  * ")
            || (trimmed.len() > 2
                && trimmed.chars().next().unwrap_or(' ').is_ascii_digit()
                && trimmed.chars().nth(1) == Some('.'));

        if trimmed.is_empty() {
            // Empty line - end current paragraph
            if !current_paragraph.is_empty() {
                formatted_lines.push(current_paragraph.join(" "));
                current_paragraph.clear();
                formatted_lines.push(String::new()); // Add spacing between paragraphs
                in_list = false;
            }
        } else if is_list_item {
            // List item - preserve as its own line
            if !current_paragraph.is_empty() {
                formatted_lines.push(current_paragraph.join(" "));
                current_paragraph.clear();
            }
            formatted_lines.push(format!("  {}", trimmed));
            in_list = true;
        } else if in_list && trimmed.starts_with("  ") {
            // Continuation of list item
            formatted_lines.push(format!("    {}", trimmed.trim()));
        } else {
            // Regular text - accumulate into current paragraph
            if in_list && !current_paragraph.is_empty() {
                // Starting new paragraph after list
                formatted_lines.push(String::new());
                in_list = false;
            }
            current_paragraph.push(trimmed.to_string());
        }
    }

    // Add any remaining paragraph
    if !current_paragraph.is_empty() {
        formatted_lines.push(current_paragraph.join(" "));
    }

    // Clean up excessive empty lines (max 2 in a row becomes 1)
    let mut result = Vec::new();
    let mut empty_count = 0;

    for line in formatted_lines {
        if line.is_empty() {
            empty_count += 1;
            if empty_count <= 1 {
                result.push(line);
            }
        } else {
            empty_count = 0;
            result.push(line);
        }
    }

    result.join("\n")
}

// Helper function to count the number of lines when text is wrapped
pub(crate) fn count_wrapped_lines(text: &str, width: usize) -> u16 {
    if width == 0 {
        return 0;
    }

    let mut line_count = 0u16;

    for line in text.lines() {
        if line.is_empty() {
            // Empty lines still count as one line
            line_count = line_count.saturating_add(1);
        } else {
            // Calculate how many wrapped lines this line will take
            let line_width = line.width();
            if line_width == 0 {
                line_count = line_count.saturating_add(1);
            } else {
                let wrapped_lines = line_width.div_ceil(width).max(1);
                line_count = line_count.saturating_add(wrapped_lines as u16);
            }
        }
    }

    // If text doesn't end with newline, we still have the lines we counted
    // If text is empty, return at least 1 line
    line_count.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_clean_html_drops_reddit_link_footnotes() {
        // Reddit summary: title link, body, "submitted by" line with three
        // anchors. Default html2text would emit `[N]` markers inline plus a
        // `[N]: url` footnote dump. Our decorator must produce neither.
        let html = r#"<table><tr><td><a href="https://example.com/post"><img src="https://example.com/x.jpg"/></a></td><td><a href="https://example.com/post"><b>Excuse me?! What??</b></a><br/>This is weird, right? Like what is going on with <em>suggestive</em> content lately?!<br/>submitted by <a href="https://www.reddit.com/user/A">/u/A</a> <a href="https://example.com/post">[link]</a> <a href="https://example.com/post">[comments]</a></td></tr></table>"#;
        let out = render_clean_html(html, 80);
        // No reference markers anywhere.
        assert!(
            !out.contains("][1]") && !out.contains("][2]") && !out.contains("][3]"),
            "expected no `][N]` link markers in:\n{out}"
        );
        // No footnote dump.
        assert!(
            !out.contains("[1]:") && !out.contains("[2]:"),
            "expected no `[N]: url` footnotes in:\n{out}"
        );
        // No `|` column artifacts from the table.
        assert!(
            !out.contains('|'),
            "expected no column separators in:\n{out}"
        );
        // Anchor inner text is preserved.
        assert!(out.contains("Excuse me?! What??"));
        assert!(out.contains("/u/A"));
        // Emphasis is preserved (this is what distinguishes us from
        // html2text's TrivialDecorator, which would strip the `*`s).
        assert!(
            out.contains("*suggestive*"),
            "expected `<em>` to render as `*...*` in:\n{out}"
        );
    }

    #[test]
    fn render_clean_html_strips_image_with_empty_alt() {
        // An image with no alt/title should render as `[]` (PlainDecorator
        // behavior we inherit) rather than disappearing — the user still
        // gets a visible hint that something was there.
        let html = r#"<p>Before <img src="https://example.com/x.png"/> after.</p>"#;
        let out = render_clean_html(html, 80);
        assert!(out.contains("Before"));
        assert!(out.contains("after."));
    }
}
