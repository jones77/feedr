use crate::app::{App, View};
use crate::events::handle_events;
use crate::feed::{ExtractedArticle, Feed};
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
use std::panic::AssertUnwindSafe;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::{io, time::Duration};

/// Hard cap on extraction worker threads in flight. The previous code spawned
/// one thread per queued request unconditionally, which fanned out as 20+
/// concurrent HTTP requests on a refresh of several `fulltext = true` feeds —
/// often to the same host. A small pool plus per-domain throttling matches
/// the politeness budget the feed-refresh path already uses.
const EXTRACTION_MAX_INFLIGHT: usize = 4;

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
/// spawns detached. On the first failure of any step the rest of the queue
/// is dropped — a "failure" is any condition that surfaces `app.error`
/// (spawn failures, missing article context, or an Action that sets the
/// error itself, e.g. `open-in-browser` with no URL). PipeTo non-zero exits
/// are not treated as failures: a script that returns 1 to mean "skip" is a
/// reasonable usage, and the user already saw the child's output.
fn drain_macro_steps<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) {
    use crate::keybindings::MacroStep;
    while let Some(step) = app.pending_macro_steps.pop_front() {
        // Track whether this step introduced a new error so a chain like
        // `open-in-browser ; toggle-star` halts when `open-in-browser` fails,
        // matching the contract documented above and the PipeTo / Exec paths.
        let pre_error = app.error.is_some();
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
                    app.pending_macro_steps.clear();
                    break;
                }
            }
        }
        if !pre_error && app.error.is_some() {
            app.pending_macro_steps.clear();
            break;
        }
    }
}

/// Build the argv list to spawn for each genuinely-new item on this feed.
/// `newly_seen` must come from `mark_feed_seen` — this helper consumes the
/// pre-computed indices so multiple consumers (exec_on_new + fulltext) can
/// share one `mark_feed_seen` call per feed arrival instead of double-marking.
///
/// Returns Vec rather than spawning + saving inline so a refresh batch can
/// `save_data` ONCE before spawning ANY child, instead of once per feed
/// arrival (write amplification of N for N bookmarks). Crash semantics still
/// AT-MOST-ONCE: see [`flush_exec_on_new`] for the persistence ordering.
///
/// No-op when the hook is not configured.
fn collect_exec_on_new(app: &App, feed: &Feed, newly_seen: &[usize]) -> Vec<Vec<String>> {
    let argv_template = match app.exec_on_new_template.as_ref() {
        Some(t) => t.clone(),
        None => return Vec::new(),
    };
    if newly_seen.is_empty() {
        return Vec::new();
    }
    newly_seen
        .iter()
        .filter_map(|&idx| {
            let item = feed.items.get(idx)?;
            let ctx = crate::app::article_context_from(feed, item);
            Some(crate::app::expand_argv_template(&argv_template, &ctx))
        })
        .collect()
}

/// For feeds with `fulltext = true`, queue an extraction request for each
/// newly-seen item. Uses the same newly-seen set as `exec_on_new` so an
/// item is only auto-extracted once (and not on the first ever fetch of a
/// feed — that one seeds the seen-set silently, matching the exec_on_new
/// no-firehose contract).
///
/// Auto-mode applies a scheme + private-IP allowlist (`is_safe_auto_url`):
/// the URL comes from feed XML, which a hostile feed could populate with
/// `http://169.254.169.254/...` or other internal targets to trick the
/// reader into probing the local network. Manual `Shift+F` is the user's
/// explicit action and bypasses this gate.
fn enqueue_fulltext_for_new(app: &mut App, feed: &Feed, newly_seen: &[usize]) {
    if !app.fulltext_feeds.contains(&feed.url) {
        return;
    }
    for &idx in newly_seen {
        let Some(item) = feed.items.get(idx) else {
            continue;
        };
        let Some(url) = item.link.as_deref() else {
            continue;
        };
        if !crate::feed::is_safe_auto_url(url) {
            continue;
        }
        let id = crate::app::make_item_id(feed, item);
        // Auto path → use the safe-redirect client. The URL came from feed
        // XML, so a hostile feed could publish a public-looking link that
        // 302s to RFC1918 / metadata endpoints; revalidating each hop closes
        // that window.
        app.request_extraction(id, url.to_string(), true);
    }
}

/// Persist the updated seen-set, then spawn all collected exec_on_new
/// children. The "save then spawn" order is the AT-MOST-ONCE guarantee:
/// a kill between save and spawn loses notifications, but a restart never
/// re-fires the same item. Matters for side-effecting hooks like
/// `wallabag-cli add` where a duplicate is actively wrong.
fn flush_exec_on_new(app: &mut App, pending: Vec<Vec<String>>) {
    if pending.is_empty() {
        return;
    }
    let _ = app.save_data();
    let mut first_failure_recorded = false;
    for argv in pending {
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

/// Drain pending extraction requests up to the concurrency budget, applying
/// per-domain throttling that matches the existing feed-refresh policy.
/// Each spawned worker:
///   * fetches the article URL with the existing reqwest blocking client
///     (no per-feed auth headers — see `extract_article`)
///   * runs Readability and posts the result back through `tx`
///   * catches panics from `dom_smoothie` on hostile HTML and surfaces them
///     as a `Failed` result so the slot doesn't strand on `Pending` forever
///   * decrements `inflight` regardless of success / failure / panic, so the
///     pool can't deadlock when a worker dies
///
/// Requests deferred for throttling or budget reasons stay on the queue;
/// `run_app` calls this every loop iteration so they fire on the next tick.
fn spawn_pending_extractions(
    app: &mut App,
    tx: &mpsc::Sender<(String, Result<ExtractedArticle>)>,
    inflight: &Arc<AtomicUsize>,
) {
    if app.pending_extraction_requests.is_empty() {
        return;
    }
    let timeout = app.config.network.http_timeout;
    let user_agent = app.config.network.user_agent.clone();
    // Two clients: the permissive one for manual `Shift+F` (user-explicit,
    // same redirect policy as feed fetches) and the safe-redirect one for
    // the auto path, which revalidates every hop with `is_safe_auto_url`.
    // Built lazily — if a tick has only manual requests we never construct
    // the safe client, and vice versa.
    let mut manual_client: Option<reqwest::blocking::Client> = None;
    let mut safe_client: Option<reqwest::blocking::Client> = None;
    let rate_limit = Duration::from_millis(app.config.general.refresh_rate_limit_delay);
    let now = std::time::Instant::now();

    // Pop only as many as the concurrency budget AND per-domain spacing
    // allow. Anything we can't run right now gets pushed back to the front
    // in original order so it retries next tick.
    let mut deferred: Vec<crate::app::ExtractionRequest> = Vec::new();
    loop {
        let in_use = inflight.load(Ordering::SeqCst);
        if in_use >= EXTRACTION_MAX_INFLIGHT {
            break;
        }
        let Some(req) = app.pending_extraction_requests.pop_front() else {
            break;
        };
        // Drop stale requests whose slot was evicted by LRU or removed
        // because their feed was deleted — running them would burn an HTTP
        // round-trip only to have `record_extraction_result` discard the
        // outcome. Cheap monotonic-correctness gate; must come before the
        // throttle defer so a stale id doesn't bounce back onto the queue.
        if !matches!(
            app.extracted.get(&req.id),
            Some(crate::app::ExtractionState::Pending)
        ) {
            continue;
        }
        let domain = App::extract_domain_from_url(&req.url);
        let too_soon = !rate_limit.is_zero()
            && app
                .last_domain_fetch
                .get(&domain)
                .map(|last| now.duration_since(*last) < rate_limit)
                .unwrap_or(false);
        if too_soon {
            deferred.push(req);
            continue;
        }
        // Build (or reuse) the client that matches this request's redirect
        // policy. A build failure here is permanent for this tick — re-queue
        // the request and stop.
        let client = if req.safe_redirects {
            match safe_client {
                Some(ref c) => c.clone(),
                None => match Feed::build_safe_redirect_client(timeout) {
                    Ok(c) => {
                        safe_client = Some(c.clone());
                        c
                    }
                    Err(_) => {
                        app.pending_extraction_requests.push_front(req);
                        break;
                    }
                },
            }
        } else {
            match manual_client {
                Some(ref c) => c.clone(),
                None => match Feed::build_client(timeout) {
                    Ok(c) => {
                        manual_client = Some(c.clone());
                        c
                    }
                    Err(_) => {
                        app.pending_extraction_requests.push_front(req);
                        break;
                    }
                },
            }
        };
        // Claim the budget slot and stamp the domain BEFORE spawning so a
        // concurrent caller (or the very next loop iteration) sees the slot
        // taken / the domain freshly used.
        inflight.fetch_add(1, Ordering::SeqCst);
        if !domain.is_empty() {
            app.last_domain_fetch.insert(domain, now);
        }
        let ua = user_agent.clone();
        let tx = tx.clone();
        let inflight_for_worker = Arc::clone(inflight);
        let req_id = req.id.clone();
        let req_url = req.url.clone();
        // `thread::Builder::spawn` returns `io::Result` instead of panicking
        // on OS-level thread-creation failure (rare, but a panic from the
        // main TUI loop would crash the app). On failure, release the slot
        // and re-queue the request so the next tick retries.
        let spawn_result = std::thread::Builder::new().spawn(move || {
            // `extract_article` and (via dom_smoothie) Readability touch
            // arbitrary HTML; a panic from the worker would otherwise leave
            // the slot stuck on `Pending` for the rest of the session and
            // there'd be no way for the user to retry.
            let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                crate::feed::extract_article(&req_url, &client, Some(&ua))
            }));
            let result = match outcome {
                Ok(r) => r,
                Err(_) => Err(anyhow::anyhow!(
                    "extraction worker panicked (hostile HTML?)"
                )),
            };
            let _ = tx.send((req_id, result));
            inflight_for_worker.fetch_sub(1, Ordering::SeqCst);
        });
        if spawn_result.is_err() {
            inflight.fetch_sub(1, Ordering::SeqCst);
            app.pending_extraction_requests.push_front(req);
            break;
        }
    }
    // Re-enqueue throttled requests at the front, preserving their original
    // FIFO order. Without `.rev()` we'd flip them.
    for req in deferred.into_iter().rev() {
        app.pending_extraction_requests.push_front(req);
    }
}

fn run_app<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()> {
    let mut last_tick = std::time::Instant::now();
    let tick_rate = Duration::from_millis(app.config.ui.tick_rate);
    let error_timeout = Duration::from_millis(app.config.ui.error_display_timeout);

    // Long-lived channel for extraction worker completions. Outlives any
    // individual refresh because manual `F` extractions can fire at any time.
    let (extract_tx, extract_rx) = mpsc::channel::<(String, Result<ExtractedArticle>)>();
    // Bounds concurrent extraction workers. Each spawned worker bumps this
    // on entry and drops it on exit (even on panic), so the pool never
    // deadlocks even if `dom_smoothie` blows up on hostile HTML.
    let extract_inflight = Arc::new(AtomicUsize::new(0));

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
            // Accumulate exec_on_new spawns across every feed arriving in this
            // tick, so we only `save_data` once before spawning. Saving per
            // feed (the previous shape) was N whole-file JSON writes per
            // refresh.
            let mut pending_exec: Vec<Vec<String>> = Vec::new();
            while let Ok((idx, result)) = feed_rx.try_recv() {
                if let Ok(feed) = result {
                    // Compute newly-seen ONCE per feed arrival, shared
                    // between exec_on_new and fulltext consumers. Skip the
                    // mark entirely when neither consumer needs it — keeps
                    // `seen_items` empty for users without hooks, matching
                    // the original zero-cost contract.
                    let needs_newly_seen = app.exec_on_new_template.is_some()
                        || app.fulltext_feeds.contains(&feed.url);
                    let newly_seen = if needs_newly_seen {
                        crate::app::mark_feed_seen(app, &feed)
                    } else {
                        Vec::new()
                    };
                    pending_exec.extend(collect_exec_on_new(app, &feed, &newly_seen));
                    enqueue_fulltext_for_new(app, &feed, &newly_seen);
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
            // Persist seen-set ONCE for the whole batch, then spawn. Done
            // outside the per-feed loop so a refresh that brings back items
            // for many feeds writes the JSON file once instead of N times.
            flush_exec_on_new(app, pending_exec);
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

        // Spawn workers for any extraction requests queued by events or
        // by the feed-refresh path above. Run every iteration so manual
        // `F` requests don't have to wait for a tick boundary.
        spawn_pending_extractions(app, &extract_tx, &extract_inflight);

        // Drain completed extractions back into app state. This is what
        // flips a `Pending` entry to `Ready` / `Failed`, which the detail
        // view reads from on the next draw.
        while let Ok((id, result)) = extract_rx.try_recv() {
            app.record_extraction_result(id, result);
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings::{KeyAction, MacroStep};
    use ratatui::backend::TestBackend;

    fn test_terminal() -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(80, 24)).unwrap()
    }

    #[test]
    fn test_drain_halts_when_action_sets_error() {
        // A chain where step 1 errors must not run step 2. MoveUp is not
        // supported in macros (sets app.error to a "not supported" message);
        // OpenInBrowser with no focused article sets a different error. With
        // halt-on-error the first message survives, because step 2 never runs.
        // Without the halt, step 2 would overwrite app.error and a follow-up
        // side-effecting step would run silently.
        let mut app = App::new();
        app.error = None;
        app.pending_macro_steps
            .push_back(MacroStep::Action(KeyAction::MoveUp));
        app.pending_macro_steps
            .push_back(MacroStep::Action(KeyAction::OpenInBrowser));

        let mut terminal = test_terminal();
        drain_macro_steps(&mut terminal, &mut app);

        let err = app.error.as_deref().unwrap_or("");
        assert!(
            err.contains("move-up") && err.contains("not supported"),
            "first error must survive; got: {}",
            err
        );
        assert!(
            !err.contains("open-in-browser"),
            "step 2 must not have run (would have overwritten the error); got: {}",
            err
        );
        assert!(app.pending_macro_steps.is_empty());
    }

    #[test]
    fn test_drain_continues_when_steps_succeed() {
        // Sanity: a clean chain runs every step. Two `help` steps are no-ops
        // that don't set app.error; both must execute and leave the queue empty.
        let mut app = App::new();
        app.error = None;
        app.show_help_overlay = false;
        app.pending_macro_steps
            .push_back(MacroStep::Action(KeyAction::Help));
        app.pending_macro_steps
            .push_back(MacroStep::Action(KeyAction::Help));

        let mut terminal = test_terminal();
        drain_macro_steps(&mut terminal, &mut app);

        assert!(app.pending_macro_steps.is_empty());
        assert!(app.error.is_none());
        assert!(app.show_help_overlay);
    }

    #[test]
    fn test_record_extraction_result_flips_pending_to_ready() {
        use crate::app::ExtractionState;
        let mut app = App::new();
        app.request_extraction("id1".to_string(), "https://ex.com/a".to_string(), false);
        // simulate a worker thread completing successfully
        app.record_extraction_result(
            "id1".to_string(),
            Ok(ExtractedArticle {
                title: "T".to_string(),
                plain_text: "body".to_string(),
                byline: None,
                site_name: None,
                source_url: "https://ex.com/a".to_string(),
            }),
        );
        match app.extracted.get("id1") {
            Some(ExtractionState::Ready(a)) => assert_eq!(a.plain_text, "body"),
            other => panic!("expected Ready, got {:?}", other),
        }
    }

    fn build_feed_with_items(url: &str, items: Vec<(&str, Option<&str>)>) -> Feed {
        Feed {
            url: url.to_string(),
            title: "T".to_string(),
            title_lower: "t".to_string(),
            items: items
                .into_iter()
                .map(|(title, link)| crate::feed::FeedItem {
                    title: title.to_string(),
                    link: link.map(|s| s.to_string()),
                    description: None,
                    pub_date: None,
                    author: None,
                    formatted_date: None,
                    parsed_date: None,
                    plain_text: None,
                    title_lower: title.to_lowercase(),
                })
                .collect(),
        }
    }

    #[test]
    fn test_enqueue_fulltext_for_new_skips_when_feed_not_opted_in() {
        let mut app = App::new();
        let feed = build_feed_with_items(
            "https://ex.com/feed.xml",
            vec![("Post", Some("https://ex.com/a"))],
        );
        // Not in fulltext_feeds → must be a no-op even with newly-seen indices.
        enqueue_fulltext_for_new(&mut app, &feed, &[0]);
        assert!(app.pending_extraction_requests.is_empty());
        assert!(app.extracted.is_empty());
    }

    #[test]
    fn test_enqueue_fulltext_for_new_enqueues_safe_urls_with_safe_flag() {
        let mut app = App::new();
        let feed = build_feed_with_items(
            "https://ex.com/feed.xml",
            vec![("Post", Some("https://ex.com/a"))],
        );
        app.fulltext_feeds.insert(feed.url.clone());
        enqueue_fulltext_for_new(&mut app, &feed, &[0]);
        assert_eq!(app.pending_extraction_requests.len(), 1);
        // Auto path must always tag the request safe_redirects=true so the
        // worker picks the redirect-revalidating client.
        assert!(app.pending_extraction_requests[0].safe_redirects);
    }

    #[test]
    fn test_enqueue_fulltext_for_new_drops_unsafe_urls() {
        let mut app = App::new();
        let feed = build_feed_with_items(
            "https://ex.com/feed.xml",
            vec![
                ("Internal", Some("http://192.168.1.1/admin")),
                ("Metadata", Some("http://169.254.169.254/")),
                ("Bad scheme", Some("javascript:alert(1)")),
                ("Localhost", Some("http://localhost/x")),
                ("No link", None),
                ("Public", Some("https://ex.com/a")),
            ],
        );
        app.fulltext_feeds.insert(feed.url.clone());
        enqueue_fulltext_for_new(&mut app, &feed, &[0, 1, 2, 3, 4, 5]);
        // Only the public https URL should make it through.
        assert_eq!(app.pending_extraction_requests.len(), 1);
        assert_eq!(app.pending_extraction_requests[0].url, "https://ex.com/a");
    }

    #[test]
    fn test_drain_preserves_preexisting_error_and_still_runs_step() {
        // Edge case: if `app.error` is already set before a step runs, the
        // step itself didn't set it — we must not treat it as a step failure.
        // (In practice this can't happen because handle_events clears app.error
        // on the next keypress, but the drain function shouldn't depend on that
        // invariant.)
        let mut app = App::new();
        app.error = Some("stale error".to_string());
        app.show_help_overlay = false;
        app.pending_macro_steps
            .push_back(MacroStep::Action(KeyAction::Help));
        app.pending_macro_steps
            .push_back(MacroStep::Action(KeyAction::Help));

        let mut terminal = test_terminal();
        drain_macro_steps(&mut terminal, &mut app);

        // Both steps ran despite the preexisting error.
        assert!(app.pending_macro_steps.is_empty());
        assert!(app.show_help_overlay);
    }
}
