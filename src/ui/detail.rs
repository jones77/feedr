use crate::app::{find_article_matches, App, ArticleMatch, ExtractionState, InputMode};
use crate::keybindings::{key_display, KeyAction};
use crate::ui::utils::{count_wrapped_lines, format_content_for_reading, truncate_url};
use crate::ui::ColorScheme;
use html2text::from_read;
use ratatui::{
    backend::Backend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap},
    Frame,
};

/// Which body the detail view is currently showing. Owns its strings so
/// the immutable borrow on `app.extracted` is released before we call any
/// `&mut App` method later in the render (e.g. `update_detail_max_scroll`).
enum BodySource {
    Summary,
    Extracted(String),
    Extracting,
    /// Failed extraction — body falls back to the summary, and `reason` is
    /// shown as a small dim hint above it.
    Failed(String),
}

pub(super) fn render_item_detail<B: Backend>(
    f: &mut Frame<B>,
    app: &mut App,
    area: Rect,
    colors: &ColorScheme,
) {
    // Reset the `show_extracted` toggle on article focus change. Done at
    // the top so we hold no immutable borrow on `app` (the `current_item`
    // call below would conflict). Logic lives on `App` so it's
    // unit-testable without spinning up a TestBackend.
    app.sync_detail_focus_anchor();
    // The search footer is visible whenever the user is actively typing a
    // query OR a committed query has highlights still pinned. Computing
    // this once up front lets the layout split, footer render, and title
    // suffix all agree on visibility.
    let search_footer_visible =
        matches!(app.input_mode, InputMode::ArticleSearch) || !app.article_search_query.is_empty();
    if let Some(item) = app.current_item() {
        // Split the area into header, content, and (optionally) a 1-row
        // search footer. The footer is omitted when in-article search is
        // inactive so quiet reading is unchanged.
        let chunks = if search_footer_visible {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(9), // Header
                    Constraint::Min(0),    // Content
                    Constraint::Length(1), // Search footer
                ])
                .split(area)
        } else {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(9), // Header
                    Constraint::Min(0),    // Content
                ])
                .split(area)
        };

        // Create header with enhanced typography
        let mut header_lines = vec![
            // Title with better emphasis
            Line::from(vec![Span::styled(
                &item.title,
                Style::default()
                    .fg(colors.text)
                    .add_modifier(Modifier::BOLD),
            )]),
            // Add spacing after title
            Line::from(""),
        ];

        // Build metadata line with enhanced formatting
        let mut metadata_parts = Vec::new();

        // Add read status with icon
        if let (Some(feed_idx), Some(item_idx)) = (app.selected_feed, app.selected_item) {
            let is_read = app.is_item_read(feed_idx, item_idx);
            metadata_parts.push(Span::styled(
                if is_read { "✓ Read" } else { "○ Unread" },
                Style::default().fg(if is_read {
                    colors.success
                } else {
                    colors.highlight
                }),
            ));
        }

        // Add author with emphasis
        if let Some(author) = &item.author {
            if !metadata_parts.is_empty() {
                metadata_parts.push(Span::styled(" · ", Style::default().fg(colors.muted)));
            }
            metadata_parts.push(Span::styled(
                author,
                Style::default()
                    .fg(colors.secondary)
                    .add_modifier(Modifier::ITALIC),
            ));
        }

        // Add date
        if let Some(date) = &item.formatted_date {
            if !metadata_parts.is_empty() {
                metadata_parts.push(Span::styled(" · ", Style::default().fg(colors.muted)));
            }
            metadata_parts.push(Span::styled(
                date,
                Style::default().fg(colors.text_secondary),
            ));
        }

        if !metadata_parts.is_empty() {
            header_lines.push(Line::from(metadata_parts));
        }

        // Add subtle separator before link
        header_lines.push(Line::from(""));

        // Add link if available with better styling
        if let Some(link) = &item.link {
            header_lines.push(Line::from(vec![
                Span::styled("🔗 ", Style::default().fg(colors.muted)),
                Span::styled(
                    truncate_url(link, 70),
                    Style::default()
                        .fg(colors.primary)
                        .add_modifier(Modifier::UNDERLINED),
                ),
            ]));
        }

        let article_icon = colors.get_icon_article();
        let header = Paragraph::new(header_lines)
            .block(
                Block::default()
                    .title(format!(" {} Article ", article_icon))
                    .title_alignment(Alignment::Center)
                    .borders(Borders::ALL)
                    .border_type(colors.border_normal)
                    .border_style(Style::default().fg(colors.border))
                    .style(Style::default().bg(colors.surface))
                    .padding(Padding::new(3, 3, 1, 1)), // Increased horizontal padding
            )
            .style(Style::default().fg(colors.text))
            .alignment(Alignment::Left);

        f.render_widget(header, chunks[0]);

        // Decide which body to render: the original summary, the extracted
        // full-text, or a placeholder for in-flight / failed extractions.
        // Uses `current_article_indices()` (rather than the raw selection
        // fields) so the lookup matches the resolution logic that
        // `toggle_or_request_fulltext` already uses — keeps the two sides
        // in lockstep if detail rendering ever leaks out of FeedItemDetail.
        let body_source: BodySource = if app.show_extracted {
            match app
                .current_article_indices()
                .and_then(|(fi, ii)| app.extraction_state_for(fi, ii))
            {
                Some(ExtractionState::Ready(article)) => {
                    BodySource::Extracted(article.plain_text.clone())
                }
                Some(ExtractionState::Pending { .. }) => BodySource::Extracting,
                Some(ExtractionState::Failed(reason)) => BodySource::Failed(reason.clone()),
                None => BodySource::Summary,
            }
        } else {
            BodySource::Summary
        };

        // Computed once and shared by the Summary / Failed branches below.
        // Lifted out of the match so the description-formatting logic isn't
        // duplicated when extraction fails and we fall back to the summary.
        let summary_text = || -> String {
            if let Some(desc) = &item.description {
                let raw_text = from_read(desc.as_bytes(), 100);
                format_content_for_reading(&raw_text)
            } else {
                "No description available".to_string()
            }
        };

        // The retry / toggle hints embed the *current* binding for
        // FetchFullText so they stay correct if the user remaps it.
        let fulltext_key = key_display(&KeyAction::FetchFullText, &app.keybindings);

        let description = match &body_source {
            BodySource::Summary => summary_text(),
            BodySource::Failed(reason) => {
                // Surface the failure reason at the top, then fall back to
                // the original summary so the user still has something to
                // read. Press the FetchFullText key again to retry.
                format!(
                    "[Full-text extraction failed: {}]\n[Press {} to retry — showing summary]\n\n{}",
                    reason,
                    fulltext_key,
                    summary_text()
                )
            }
            BodySource::Extracted(text) => format_content_for_reading(text),
            BodySource::Extracting => {
                // Keep the summary visible while the worker is in flight —
                // a slow site can take several seconds, and replacing the
                // body with "Fetching…" leaves the user with nothing to
                // read AND no way to revert (the FetchFullText key is a
                // no-op on Pending). The scroll-indicator title (see
                // body_label below) carries the "Extracting…" status.
                format!(
                    "[Fetching full text… — showing summary]\n\n{}",
                    summary_text()
                )
            }
        };

        // Calculate the viewport height (accounting for borders and padding)
        let viewport_height = chunks[1]
            .height
            .saturating_sub(2) // borders (top and bottom)
            .saturating_sub(4); // increased padding (top and bottom)

        // Calculate the content width (accounting for borders and padding)
        let content_width = chunks[1]
            .width
            .saturating_sub(2) // borders (left and right)
            .saturating_sub(8) // increased padding for better reading width
            as usize;

        // Calculate the number of lines the wrapped content will take
        let content_lines = count_wrapped_lines(&description, content_width);

        // Update the max scroll value
        app.update_detail_max_scroll(content_lines, viewport_height);
        app.clamp_detail_scroll();

        // Cache the rendered body and content width so `n` / `N` (handled
        // in event dispatch) can resolve a match line back to a wrapped-row
        // scroll target without re-deriving the body source. Re-derivation
        // here is the source of truth — these writes happen once per frame.
        app.article_body_cache = description.clone();
        app.article_body_width_cache = content_width;

        // Recompute matches against the freshly rendered body. Clamp the
        // current-match cursor so it stays valid as the query shrinks or
        // the body changes (e.g. after `Shift+F` toggles full-text).
        let matches = find_article_matches(&description, &app.article_search_query);
        if matches.is_empty() {
            app.article_search_current = None;
        } else {
            let cur = app
                .article_search_current
                .unwrap_or(0)
                .min(matches.len() - 1);
            app.article_search_current = Some(cur);
        }
        let current_idx = app.article_search_current;
        app.article_search_matches = matches.clone();

        // Create theme-specific scroll indicator
        let scroll_arrows = if colors.border_normal == BorderType::Double {
            ("▼", "▲") // Dark: solid arrows
        } else {
            ("↓", "↑") // Light: simple arrows
        };

        let body_label = match &body_source {
            BodySource::Summary => "Summary",
            BodySource::Extracted(_) => "Full-text",
            BodySource::Extracting => "Extracting…",
            BodySource::Failed(_) => "Full-text failed",
        };

        let match_suffix = if app.article_search_query.is_empty() {
            String::new()
        } else if matches.is_empty() {
            " · no matches".to_string()
        } else {
            format!(
                " · {}/{} matches",
                current_idx.map(|i| i + 1).unwrap_or(0),
                matches.len()
            )
        };

        let scroll_indicator = if app.detail_max_scroll > 0 {
            let scroll_pct =
                (app.detail_vertical_scroll as f32 / app.detail_max_scroll as f32 * 100.0) as u16;
            if app.detail_vertical_scroll == 0 {
                format!(
                    " {} {} · Scroll {} for more{} ",
                    article_icon, body_label, scroll_arrows.0, match_suffix
                )
            } else if app.detail_vertical_scroll >= app.detail_max_scroll {
                format!(
                    " {} {} · End of article{} ",
                    article_icon, body_label, match_suffix
                )
            } else {
                format!(
                    " {} {} · {}%{} ",
                    article_icon, body_label, scroll_pct, match_suffix
                )
            }
        } else {
            format!(" {} {}{} ", article_icon, body_label, match_suffix)
        };

        // Build the rendered body as styled lines so match ranges can be
        // highlighted. `Wrap { trim: true }` on the Paragraph preserves
        // intra-line span styling under soft wrap, so we don't need to
        // pre-wrap ourselves. Lines without matches collapse to a single
        // `Span::raw` to avoid unnecessary allocations.
        let body_lines = build_styled_body(&description, &matches, current_idx, colors);

        // Create content paragraph with theme-specific styling
        let content = Paragraph::new(body_lines)
            .block(
                Block::default()
                    .title(scroll_indicator)
                    .title_alignment(Alignment::Center)
                    .borders(Borders::ALL)
                    .border_type(colors.border_normal)
                    .border_style(Style::default().fg(colors.border))
                    .style(Style::default().bg(colors.surface))
                    .padding(Padding::new(4, 4, 2, 2)), // Generous padding for reading comfort
            )
            .style(Style::default().fg(colors.text))
            .scroll((app.detail_vertical_scroll, 0))
            .wrap(Wrap { trim: true })
            .alignment(Alignment::Left);

        f.render_widget(content, chunks[1]);

        if search_footer_visible {
            render_search_footer(f, app, chunks[2], colors);
        }
    }
}

/// Render the 1-row search footer at the bottom of the detail view.
/// Shows `/<query>` and (when not actively typing) the match indicator.
/// Sets the terminal cursor on the line when in `ArticleSearch` input
/// mode so the user sees where input is going.
fn render_search_footer<B: Backend>(f: &mut Frame<B>, app: &App, area: Rect, colors: &ColorScheme) {
    let typing = matches!(app.input_mode, InputMode::ArticleSearch);
    let match_count = app.article_search_matches.len();
    let current = app.article_search_current;

    let mut spans = vec![
        Span::styled(
            "/",
            Style::default()
                .fg(colors.highlight)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            app.article_search_query.clone(),
            Style::default().fg(colors.text),
        ),
    ];

    if !typing {
        let status = if app.article_search_query.is_empty() {
            String::new()
        } else if match_count == 0 {
            "  no matches".to_string()
        } else {
            format!(
                "  {}/{} (n/N to jump · ESC to clear)",
                current.map(|i| i + 1).unwrap_or(0),
                match_count
            )
        };
        if !status.is_empty() {
            spans.push(Span::styled(status, Style::default().fg(colors.muted)));
        }
    }

    let footer = Paragraph::new(Line::from(spans))
        .style(Style::default().bg(colors.surface))
        .alignment(Alignment::Left);
    f.render_widget(footer, area);

    if typing {
        // Cursor sits just past the rendered query: 1 column for "/" plus
        // the display width of the query. Clamp to the visible row width.
        let q_width =
            unicode_width::UnicodeWidthStr::width(app.article_search_query.as_str()) as u16;
        let cursor_x = area.x + 1 + q_width;
        f.set_cursor(cursor_x.min(area.x + area.width.saturating_sub(1)), area.y);
    }
}

/// Build the body as styled `Line`s, applying highlight spans for each
/// match. The match at `current_idx` (when set) is rendered with a
/// stronger style so the user can tell which one `n`/`N` will move from.
fn build_styled_body<'a>(
    body: &'a str,
    matches: &[ArticleMatch],
    current_idx: Option<usize>,
    colors: &ColorScheme,
) -> Vec<Line<'a>> {
    // Non-current matches: foreground tinted only, no background fill, no
    // bold — present but quiet. The match the `n` cursor sits on gets the
    // attention: filled `highlight` background, surface-colored text, bold.
    let highlight_style = Style::default().fg(colors.highlight);
    let current_style = Style::default()
        .bg(colors.highlight)
        .fg(colors.surface)
        .add_modifier(Modifier::BOLD);

    // Group matches by source-line index so we can walk each line in one
    // pass. Matches are already in line-major order (find_article_matches
    // iterates lines outer, matches inner), so a simple split-by-line
    // works. Track absolute match index so we can mark the "current" one.
    let mut by_line: std::collections::HashMap<usize, Vec<(usize, &ArticleMatch)>> =
        std::collections::HashMap::new();
    for (i, m) in matches.iter().enumerate() {
        by_line.entry(m.line).or_default().push((i, m));
    }

    body.split('\n')
        .enumerate()
        .map(|(line_idx, line)| {
            let line_matches = by_line.get(&line_idx);
            match line_matches {
                None => Line::from(line.to_string()),
                Some(ms) => {
                    let mut spans: Vec<Span<'a>> = Vec::new();
                    let mut cursor = 0usize;
                    for (abs_idx, m) in ms {
                        if m.start > cursor {
                            spans.push(Span::raw(line[cursor..m.start].to_string()));
                        }
                        let matched = line[m.start..m.end].to_string();
                        let style = if Some(*abs_idx) == current_idx {
                            current_style
                        } else {
                            highlight_style
                        };
                        spans.push(Span::styled(matched, style));
                        cursor = m.end;
                    }
                    if cursor < line.len() {
                        spans.push(Span::raw(line[cursor..].to_string()));
                    }
                    Line::from(spans)
                }
            }
        })
        .collect()
}
