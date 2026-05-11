use crate::app::{App, View};
use crate::events::handle_events;
use crate::feed::Feed;
use crate::ui;
use anyhow::{Context, Result};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    Terminal,
};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::{io, time::Duration};

/// RAII guard that re-enters the alt-screen + raw mode + mouse capture on drop,
/// so a panic during a pipe-to child invocation cannot leave the terminal in
/// a broken state.
struct TerminalRestoreGuard;

impl TerminalRestoreGuard {
    fn enter() -> io::Result<Self> {
        // Leave the TUI before handing the tty to the child.
        let mut stdout = io::stdout();
        execute!(stdout, LeaveAlternateScreen, DisableMouseCapture)?;
        disable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        // Best-effort restore — ignore errors during unwind.
        let _ = enable_raw_mode();
        let _ = execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture);
    }
}

/// Suspend the TUI, run argv with the given stdin payload (or inherited stdin
/// if `stdin_payload` is None), then re-enter the TUI and force a redraw.
/// Drains any pending terminal events so terminal-probe responses from the
/// child don't leak into our key handler.
pub fn suspend_for_command<B: Backend>(
    terminal: &mut Terminal<B>,
    argv: &[String],
    stdin_payload: Option<&[u8]>,
) -> Result<std::process::ExitStatus> {
    if argv.is_empty() {
        anyhow::bail!("empty command");
    }
    let status = {
        let _guard = TerminalRestoreGuard::enter()?;
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        if stdin_payload.is_some() {
            cmd.stdin(Stdio::piped());
        }
        // stdout/stderr inherited so user sees output of the child.
        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn '{}'", argv[0]))?;
        // Write the stdin payload from a dedicated thread so a payload larger
        // than the pipe buffer (typically 64 KiB on Linux) cannot deadlock the
        // parent against a child that hasn't started reading yet.
        let writer = match (stdin_payload, child.stdin.take()) {
            (Some(payload), Some(mut stdin)) => {
                let payload = payload.to_vec();
                Some(std::thread::spawn(move || {
                    let _ = stdin.write_all(&payload);
                    // stdin drops here, closing the pipe so the child sees EOF.
                }))
            }
            _ => None,
        };
        let status = child.wait().context("failed to wait for child")?;
        if let Some(w) = writer {
            let _ = w.join();
        }
        status
    };
    // Drain any pending input that might have arrived during the child's run
    // (e.g. terminal-probe responses from less/vim).
    while event::poll(Duration::from_millis(0))? {
        let _ = event::read();
    }
    terminal.clear()?;
    Ok(status)
}

/// Spawn a child detached: stdio nulled, parent does not block. A reaper
/// thread waits on the child so it doesn't linger as a zombie for the
/// lifetime of the TUI session — `Child` left undropped on Unix would
/// otherwise stay `<defunct>` until parent exit. Used by exec_on_new where
/// we may fan out many children and must not block the main loop.
pub fn spawn_detached(argv: &[String]) -> Result<()> {
    if argv.is_empty() {
        anyhow::bail!("empty command");
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn '{}'", argv[0]))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub fn run(mut app: App) -> Result<()> {
    // Set up terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Run the main application loop
    let result = run_app(&mut terminal, &mut app);

    // Clean up terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    // Handle any errors from the application
    if let Err(err) = result {
        println!("Error: {:?}", err);
    }

    Ok(())
}

/// Drain all queued macro steps in FIFO order. Action steps mutate app
/// state inline, PipeTo suspends the terminal for a blocking child, Exec
/// spawns detached. On the first error the rest of the queue is dropped
/// to avoid running a follow-up step in an undefined state.
fn drain_macro_steps<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) {
    use crate::keybindings::MacroStep;
    while let Some(step) = app.pending_macro_steps.pop_front() {
        match step {
            MacroStep::Action(a) => crate::events::dispatch_action(app, a),
            MacroStep::PipeTo {
                argv_template,
                stdin,
            } => {
                let (argv, payload) =
                    match crate::events::build_pipe_invocation(app, &argv_template, stdin) {
                        Some(v) => v,
                        None => {
                            app.error = Some("pipe-to: no article in focus".to_string());
                            app.pending_macro_steps.clear();
                            break;
                        }
                    };
                if let Err(e) = suspend_for_command(terminal, &argv, Some(&payload)) {
                    app.error = Some(format!("pipe-to: {}", e));
                    app.pending_macro_steps.clear();
                    break;
                }
            }
            MacroStep::Exec { argv_template } => {
                let argv = match crate::events::build_exec_invocation(app, &argv_template) {
                    Some(v) => v,
                    None => {
                        app.error = Some("exec: no article in focus".to_string());
                        app.pending_macro_steps.clear();
                        break;
                    }
                };
                if let Err(e) = spawn_detached(&argv) {
                    app.error = Some(format!("exec: {}", e));
                }
            }
        }
    }
}

/// Diff a freshly-arrived feed against `app.seen_items` and fire the
/// `[hooks].exec_on_new` template (if configured) for each genuinely-new item.
/// On the first successful fetch of a feed (URL not yet in `feeds_seeded`),
/// seed the seen set silently to suppress the firehose.
///
/// Persistence order is "mark-seen, persist, spawn" so semantics on crash are
/// AT MOST ONCE: a kill between persist and spawn loses a notification, but a
/// later restart never re-fires the same item. This matters for side-effecting
/// hooks like `wallabag-cli add`.
///
/// No-op when the hook is not configured. seen_items / feeds_seeded only
/// exist to drive this hook, so users who never opt in pay neither the
/// memory cost nor the per-feed save_data round-trip.
fn fire_exec_on_new(app: &mut App, feed: &Feed) {
    let argv_template = match app.exec_on_new_template.as_ref() {
        Some(t) => t.clone(),
        None => return,
    };
    let newly_seen_idx = crate::app::mark_feed_seen(app, feed);
    if newly_seen_idx.is_empty() {
        return;
    }
    // Persist the updated seen set before spawning. If we crash before
    // spawning, the user misses a notification — preferable to a duplicate
    // for side-effecting hooks.
    let _ = app.save_data();
    let mut first_failure_recorded = false;
    for idx in newly_seen_idx {
        let item = match feed.items.get(idx) {
            Some(i) => i,
            None => continue,
        };
        let ctx = crate::app::article_context_from(feed, item);
        let argv = crate::app::expand_argv_template(&argv_template, &ctx);
        if let Err(e) = spawn_detached(&argv) {
            if !first_failure_recorded {
                app.error = Some(format!("exec_on_new: {}", e));
                first_failure_recorded = true;
            }
        }
    }
}

/// Spawn background threads to fetch all bookmarked feeds, sending results through the channel.
/// Returns the sender's pending count and the receiver.
fn spawn_feed_refresh(app: &mut App) -> (usize, mpsc::Receiver<(usize, Result<Feed>)>) {
    let (feed_tx, feed_rx) = mpsc::channel::<(usize, Result<Feed>)>();
    let mut pending_count: usize = 0;

    if !app.bookmarks.is_empty() {
        let timeout = app.config.network.http_timeout;
        let user_agent = app.config.network.user_agent.clone();
        let all_headers = app.feed_headers.clone();

        if let Ok(client) = Feed::build_client(timeout) {
            pending_count = app.bookmarks.len();
            app.is_loading = true;
            app.refresh_in_progress = true;
            for (idx, url) in app.bookmarks.iter().enumerate() {
                let client = client.clone();
                let url = url.clone();
                let ua = user_agent.clone();
                let tx = feed_tx.clone();
                let hdrs = all_headers.get(&url).cloned();
                std::thread::spawn(move || {
                    let result = Feed::fetch_url(&url, &client, Some(&ua), hdrs.as_ref())
                        .and_then(|r| r.into_feed());
                    let _ = tx.send((idx, result));
                });
            }
        }
    }

    (pending_count, feed_rx)
}

fn run_app<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()> {
    let mut last_tick = std::time::Instant::now();
    let tick_rate = Duration::from_millis(app.config.ui.tick_rate);
    let error_timeout = Duration::from_millis(app.config.ui.error_display_timeout);

    // Initial load of bookmarked feeds
    let (mut pending_count, mut feed_rx) = spawn_feed_refresh(app);

    loop {
        terminal.draw(|f| {
            app.update_compact_mode(f.size().height);
            ui::render(f, app);
        })?;

        // Check if a refresh was requested (by 'r' key or auto-refresh)
        if app.refresh_requested {
            app.refresh_requested = false;
            if !app.refresh_in_progress {
                app.feeds.clear();
                app.update_dashboard();
                app.rebuild_feed_tree();
                let (count, rx) = spawn_feed_refresh(app);
                pending_count = count;
                feed_rx = rx;
            }
        }

        // Drain any feeds that arrived from background threads
        if pending_count > 0 {
            while let Ok((idx, result)) = feed_rx.try_recv() {
                if let Ok(feed) = result {
                    fire_exec_on_new(app, &feed);
                    // Insert at the correct position to maintain bookmark order,
                    // or append if earlier feeds haven't arrived yet
                    let insert_pos = app
                        .feeds
                        .iter()
                        .position(|f| {
                            app.bookmarks
                                .iter()
                                .position(|b| b == &f.url)
                                .unwrap_or(usize::MAX)
                                > idx
                        })
                        .unwrap_or(app.feeds.len());
                    app.feeds.insert(insert_pos, feed);
                    app.update_dashboard();
                    app.rebuild_feed_tree();
                }
                pending_count -= 1;
                if pending_count == 0 {
                    app.is_loading = false;
                    app.refresh_in_progress = false;
                    let now = std::time::Instant::now();
                    app.last_refresh = Some(now);
                    for url in &app.bookmarks {
                        app.last_feed_refresh.insert(url.clone(), now);
                    }
                    app.update_dashboard();
                    app.rebuild_feed_tree();
                    // Show summary view if there are new items since last session
                    if app.show_summary {
                        app.show_summary = false;
                        let (total, _) = app.get_summary_stats();
                        if total > 0 {
                            app.view = View::Summary;
                        }
                    }
                    // Save current time as session time now that feeds are loaded
                    let _ = app.save_data();
                }
            }
        }

        // If loading, use a shorter timeout for animation
        let timeout = if app.is_loading {
            tick_rate
                .checked_sub(last_tick.elapsed())
                .unwrap_or_else(|| Duration::from_secs(0))
        } else if app.error.is_some() {
            error_timeout
        } else {
            tick_rate
        };

        if event::poll(timeout)? {
            // Handle user input
            if handle_events(app)? {
                return Ok(());
            }
            drain_macro_steps(terminal, app);
        } else if last_tick.elapsed() >= tick_rate {
            // Update animation frame on tick
            if app.is_loading {
                app.update_loading_indicator();
            }

            // Clear error after timeout
            if app.error.is_some() && last_tick.elapsed() >= error_timeout {
                app.error = None;
            }

            // Clear success message after a shorter timeout (1.5 seconds)
            let success_timeout = Duration::from_millis(1500);
            if let Some(msg_time) = app.success_message_time {
                if app.success_message.is_some() && msg_time.elapsed() >= success_timeout {
                    app.success_message = None;
                    app.success_message_time = None;
                }
            }

            // Time out a stranded macro-prefix wait so the next unrelated
            // keystroke isn't silently consumed as a macro follow-up.
            if let Some(since) = app.awaiting_macro_key_since {
                if since.elapsed() >= success_timeout {
                    app.awaiting_macro_key = false;
                    app.awaiting_macro_key_since = None;
                }
            }

            // Check if auto-refresh should trigger
            if app.should_auto_refresh() {
                app.refresh_requested = true;
            }

            last_tick = std::time::Instant::now();
        }
    }
}
