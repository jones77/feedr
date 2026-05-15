use crate::app::{App, ExtractionState};
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
    if let Some(item) = app.current_item() {
        // Split the area into header and content with better proportions
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(9), // Header - increased for better spacing
                Constraint::Min(0),    // Content
            ])
            .split(area);

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
                Some(ExtractionState::Pending) => BodySource::Extracting,
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

        let description = match &body_source {
            BodySource::Summary => summary_text(),
            BodySource::Failed(reason) => {
                // Surface the failure reason at the top, then fall back to
                // the original summary so the user still has something to
                // read. Press F again to retry.
                format!(
                    "[Full-text extraction failed: {}]\n[Press F to retry — showing summary]\n\n{}",
                    reason,
                    summary_text()
                )
            }
            BodySource::Extracted(text) => format_content_for_reading(text),
            BodySource::Extracting => "Fetching full text…".to_string(),
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

        let scroll_indicator = if app.detail_max_scroll > 0 {
            let scroll_pct =
                (app.detail_vertical_scroll as f32 / app.detail_max_scroll as f32 * 100.0) as u16;
            if app.detail_vertical_scroll == 0 {
                format!(
                    " {} {} · Scroll {} for more ",
                    article_icon, body_label, scroll_arrows.0
                )
            } else if app.detail_vertical_scroll >= app.detail_max_scroll {
                format!(" {} {} · End of article ", article_icon, body_label)
            } else {
                format!(" {} {} · {}% ", article_icon, body_label, scroll_pct)
            }
        } else {
            format!(" {} {} ", article_icon, body_label)
        };

        // Create content paragraph with theme-specific styling
        let content = Paragraph::new(description)
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
    }
}
