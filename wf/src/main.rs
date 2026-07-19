//! wf — cross-project management TUI (Ratatui).
//!
//! Tabs: Projects | Secrets | Hindsight.
//! Headless: `wf --list`, `wf --secrets` (no TUI; for cron/CI).
mod git;
mod hindsight;
mod secrets;
mod theme;

use crossterm::event::{self, Event, KeyCode};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Table, Tabs},
    Terminal,
};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

/// Dev-only (debug + unix) hot-reload. A background thread watches the source
/// files; on change it rebuilds with `cargo build` and signals the main loop,
/// which tears down the TUI and re-execs the fresh binary in place. Disabled in
/// release builds and when WF_NO_WATCH is set.
#[cfg(all(unix, debug_assertions))]
mod dev_watch {
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    const SOURCES: &[&str] = &[
        "Cargo.toml",
        "Cargo.lock",
        "src/main.rs",
        "src/git.rs",
        "src/secrets.rs",
        "src/hindsight.rs",
    ];

    fn manifest_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// Newest mtime across the crate's source files.
    fn latest_source_mtime() -> SystemTime {
        let dir = manifest_dir();
        let mut latest = UNIX_EPOCH;
        for f in SOURCES {
            if let Ok(m) = std::fs::metadata(dir.join(f)).and_then(|m| m.modified()) {
                if m > latest {
                    latest = m;
                }
            }
        }
        latest
    }

    /// Spawn the watcher. On a successful rebuild it flips `reload` to true.
    ///
    /// We compare the current source mtime against the mtime we last *built*
    /// (not against the running binary's own mtime, which isn't reliably
    /// readable). A `building` guard prevents overlapping `cargo` runs.
    pub fn spawn(reload: Arc<AtomicBool>) {
        std::thread::spawn(move || {
            // Seed with the source mtime at startup so we don't rebuild immediately.
            let last_built: Arc<Mutex<SystemTime>> = Arc::new(Mutex::new(latest_source_mtime()));
            let building: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let now = latest_source_mtime();
                let mut last = last_built.lock().unwrap();
                let mut busy = building.lock().unwrap();
                if *busy || now <= *last {
                    continue;
                }
                *busy = true;
                drop(busy);
                let status = Command::new("cargo")
                    .arg("build")
                    .current_dir(manifest_dir())
                    .status();
                *last = latest_source_mtime();
                let mut busy = building.lock().unwrap();
                *busy = false;
                drop(busy);
                if let Ok(s) = status {
                    if s.success() {
                        reload.store(true, Ordering::SeqCst);
                    }
                    // On build failure: keep the old binary running.
                }
            }
        });
    }

    /// Tear down the TUI and re-exec the freshly built binary (self-replacing).
    /// Replaces the process image; only returns (as an error) on failure.
    ///
    /// We re-exec the stable `target/debug/wf` path (derived from the manifest
    /// dir) rather than `std::env::current_exe()`: `cargo build` *unlinks* the
    /// old binary and creates a new inode, so the running process's
    /// `/proc/exe` is a stale "(deleted)" path that can't be exec'd.
    pub fn reexec() -> std::io::Error {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        let exe = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/target/debug/wf"));
        if !exe.exists() {
            return std::io::Error::new(std::io::ErrorKind::NotFound, "target/debug/wf missing");
        }
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut cmd = Command::new(exe);
        cmd.args(args);
        cmd.exec() // never returns on success
    }
}

const PROJECTS_ROOT: &str = "/home/nayem/Projects";
const BACKUP_SH: &str = "/home/nayem/Projects/workflows/backup.sh";
const BACKUP_DIR: &str = "/home/nayem/Projects/Backups";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const HELP: &str =
    "1/2/3/4 tabs | ↑↓ select | Enter action | r refresh | R rebuild+restart | q quit | t theme  v verbose | Hindsight: Enter start / S stop";

#[derive(Debug, Clone)]
enum Tab {
    Projects,
    Secrets,
    Hindsight,
    Backup,
}

struct App {
    tab: Tab,
    repos: Vec<git::Repo>,
    selected: usize,
    // on-demand `make check` result for the selected repo
    check: Arc<Mutex<CheckState>>,
    secrets: Vec<secrets::Finding>,
    hindsight: hindsight::BankInfo,
    sweep_status: String,
    backup: Arc<Mutex<CheckState>>,
    backup_list: String,
    confirm_sweep: bool,
    confirm_stop: bool,
    status: String,
    // hindsight service control feedback (start/stop result)
    service_msg: String,
    // shared channel the start thread writes its outcome to (so the TUI
    // can pick it up on the next live-rescan tick)
    hindsight_status: Arc<Mutex<String>>,
    // previous scan's BankInfo — used to compute per-interval deltas (rates)
    prev_hindsight: hindsight::BankInfo,
    // wall-clock of the last live rescan (shown in the footer so you can see it tick)
    last_scan: String,
    // UI theme + verbosity (loaded from XDG config, live-toggleable)
    theme: theme::Theme,
    verbose: bool,
}

/// Snapshot of everything the panels render, recomputed by the background
/// rescan worker. Cloning is cheap (a handful of small vecs).
#[derive(Clone, Default)]
struct ScanData {
    repos: Vec<git::Repo>,
    secrets: Vec<secrets::Finding>,
    hindsight: hindsight::BankInfo,
    backup_list: String,
    stamp: String,
}

/// Full rescan of ~/Projects (repo health, secrets, hindsight, backup dir).
fn scan_all(root: &PathBuf) -> ScanData {
    let repos = load_repos(root);
    let secrets = git::discover(root)
        .iter()
        .flat_map(|p| secrets::scan_repo(p))
        .collect();
    ScanData {
        repos,
        secrets,
        hindsight: hindsight::info(),
        backup_list: backup_snapshot(),
        stamp: chrono_now(),
    }
}

/// `HH:MM:SS` local time, for the live-rescan footer stamp.
fn chrono_now() -> String {
    // std-only: format the unix timestamp as HH:MM:SS.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (h, m, s) = (secs / 3600 % 24, secs / 60 % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

#[derive(Debug, Default, Clone)]
struct CheckState {
    running: bool,
    output: String,
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let root = PathBuf::from(PROJECTS_ROOT);

    if args.iter().any(|a| a == "--list") {
        return headless_list(&root);
    }
    if args.iter().any(|a| a == "--secrets") {
        return headless_secrets(&root);
    }

    // ---- TUI ----
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    // Dev hot-reload: watch source, rebuild, re-exec this binary on change.
    #[cfg(all(unix, debug_assertions))]
    let reload_flag: std::sync::Arc<std::sync::atomic::AtomicBool> =
        if std::env::var_os("WF_NO_WATCH").is_none() {
            let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            dev_watch::spawn(std::sync::Arc::clone(&flag));
            flag
        } else {
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))
        };

    // Initial scan, plus a background worker that rescans ~/Projects every
    // few seconds so repo git-state changes (commit/push/dirty) show up
    // live without closing the TUI.
    let shared_scan: Arc<Mutex<ScanData>> = Arc::new(Mutex::new(scan_all(&root)));
    {
        let shared = Arc::clone(&shared_scan);
        let root2 = root.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let data = scan_all(&root2);
            *shared.lock().unwrap() = data;
        });
    }

    let init = shared_scan.lock().unwrap().clone();
    let cfg = theme::Config::load();
    let mut app = App {
        tab: Tab::Projects,
        repos: init.repos,
        selected: 0,
        check: Arc::new(Mutex::new(CheckState::default())),
        secrets: init.secrets,
        hindsight: init.hindsight,
        sweep_status: String::new(),
        backup: Arc::new(Mutex::new(CheckState::default())),
        backup_list: init.backup_list,
        confirm_sweep: false,
        confirm_stop: false,
        status: HELP.into(),
        service_msg: String::new(),
        hindsight_status: Arc::new(Mutex::new(String::new())),
        prev_hindsight: hindsight::BankInfo::default(),
        last_scan: init.stamp,
        theme: cfg.theme,
        verbose: cfg.verbose,
    };

    loop {
        term.draw(|f| draw(f, &app))?;

        // Live data refresh: copy the latest background rescan into `app`
        // every tick (~5x/sec), so a repo's git state changing on disk
        // (commit/push/dirty) shows up without closing the TUI. We only
        // swap when the stamp changed, to avoid churning `selected`.
        {
            let s = shared_scan.lock().unwrap();
            if s.stamp != app.last_scan {
                app.repos = s.repos.clone();
                if app.selected >= app.repos.len() {
                    app.selected = app.repos.len().saturating_sub(1);
                }
                app.secrets = s.secrets.clone();
                app.prev_hindsight = app.hindsight.clone();
                app.hindsight = s.hindsight.clone();
                app.backup_list = s.backup_list.clone();
                app.last_scan = s.stamp.clone();
            }
        }

        // Pick up the hindsight start outcome once the worker thread finishes.
        {
            let s = app.hindsight_status.lock().unwrap();
            if !s.is_empty() {
                app.service_msg = s.clone();
                app.status = s.clone();
                // refresh the panel's running flag immediately
                app.hindsight = hindsight::info();
                drop(s);
                *app.hindsight_status.lock().unwrap() = String::new();
            }
        }

        // Dev hot-reload: if the watcher rebuilt the binary, restart in place.
        // Checked on every tick (not just after a keypress) so the TUI reloads
        // in realtime while you watch — no keypress needed.
        #[cfg(all(unix, debug_assertions))]
        {
            if reload_flag.load(std::sync::atomic::Ordering::SeqCst) {
                dev_watch::reexec();
                // reexec replaces the process; we never reach here on success.
                return crossterm::terminal::disable_raw_mode();
            }
        }

        // Poll (non-blocking) with a short timeout so the loop wakes on its own
        // ~5x/sec to re-check the reload flag, instead of blocking forever on
        // event::read().
        if event::poll(std::time::Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Char('1') => {
                        app.tab = Tab::Projects;
                        app.confirm_sweep = false;
                    }
                    KeyCode::Char('2') => {
                        app.tab = Tab::Secrets;
                        app.confirm_sweep = false;
                    }
                    KeyCode::Char('3') => {
                        app.tab = Tab::Hindsight;
                        app.confirm_sweep = false;
                    }
                    KeyCode::Char('4') => {
                        app.tab = Tab::Backup;
                        app.confirm_sweep = false;
                    }
                    KeyCode::Left | KeyCode::Char('h') => {
                        app.tab = prev_tab(&app.tab);
                        app.confirm_sweep = false;
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        app.tab = next_tab(&app.tab);
                        app.confirm_sweep = false;
                    }
                    KeyCode::Up => {
                        if app.selected > 0 {
                            app.selected -= 1;
                        }
                    }
                    KeyCode::Down => {
                        let n = match app.tab {
                            Tab::Projects => app.repos.len(),
                            Tab::Secrets => app.secrets.len(),
                            Tab::Hindsight => 1,
                            Tab::Backup => 1,
                        };
                        if n > 0 && app.selected < n - 1 {
                            app.selected += 1;
                        }
                    }
                    KeyCode::Char('r') => refresh(&mut app, &root),
                    // Cycle the color theme and persist it.
                    KeyCode::Char('t') => {
                        app.theme = app.theme.next();
                        let cfg = theme::Config {
                            theme: app.theme,
                            verbose: app.verbose,
                        };
                        cfg.save();
                        app.status = format!("theme: {}", app.theme.name);
                    }
                    // Toggle verbose (explain technical headings) and persist it.
                    KeyCode::Char('v') => {
                        app.verbose = !app.verbose;
                        let cfg = theme::Config {
                            theme: app.theme,
                            verbose: app.verbose,
                        };
                        cfg.save();
                        app.status = format!("verbose: {}", if app.verbose { "on" } else { "off" });
                    }
                    // Dev: hard restart — rebuild now and re-exec the fresh binary.
                    #[cfg(all(unix, debug_assertions))]
                    KeyCode::Char('R') => {
                        let _ = std::process::Command::new("cargo")
                            .arg("build")
                            .current_dir(env!("CARGO_MANIFEST_DIR"))
                            .status();
                        dev_watch::reexec();
                        return crossterm::terminal::disable_raw_mode();
                    }
                    KeyCode::Char('s') | KeyCode::Char('S') => {
                        // hindsight service stop requires confirmation (destructive)
                        if let Tab::Hindsight = app.tab {
                            app.confirm_stop = true;
                            app.status =
                                "Press Y to STOP hindsight-api; any other key cancels".into();
                        }
                    }
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        if app.confirm_stop {
                            run_hindsight_stop(&mut app);
                            app.confirm_stop = false;
                        } else if app.confirm_sweep {
                            apply_sweep(&mut app);
                            app.confirm_sweep = false;
                        }
                    }
                    KeyCode::Enter => handle_enter(&mut app),
                    _ => {
                        // Any other key cancels a pending stop confirmation
                        // (Y is handled above; everything else dismisses it).
                        app.confirm_stop = false;
                    }
                }
            }
        }
    }

    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen)?;
    Ok(())
}

fn prev_tab(t: &Tab) -> Tab {
    match t {
        Tab::Projects => Tab::Backup,
        Tab::Secrets => Tab::Projects,
        Tab::Hindsight => Tab::Secrets,
        Tab::Backup => Tab::Hindsight,
    }
}
fn next_tab(t: &Tab) -> Tab {
    match t {
        Tab::Projects => Tab::Secrets,
        Tab::Secrets => Tab::Hindsight,
        Tab::Hindsight => Tab::Backup,
        Tab::Backup => Tab::Projects,
    }
}

fn refresh(app: &mut App, root: &PathBuf) {
    let data = scan_all(root);
    app.repos = data.repos;
    if app.selected >= app.repos.len() {
        app.selected = app.repos.len().saturating_sub(1);
    }
    app.secrets = data.secrets;
    app.hindsight = data.hindsight;
    app.backup_list = data.backup_list;
    app.last_scan = data.stamp;
    app.status = "refreshed".into();
}

/// Discover + health-check + sort (dirty first, then behind, then ahead, then name).
fn load_repos(root: &PathBuf) -> Vec<git::Repo> {
    let mut repos: Vec<git::Repo> = git::discover(root).iter().map(|p| git::health(p)).collect();
    repos.sort_by(|a, b| {
        b.dirty
            .cmp(&a.dirty)
            .then(b.behind.cmp(&a.behind))
            .then(b.ahead.cmp(&a.ahead))
            .then(a.name.cmp(&b.name))
    });
    repos
}

fn handle_enter(app: &mut App) {
    match app.tab {
        Tab::Projects => {
            if let Some(repo) = app.repos.get(app.selected) {
                if repo.has_makefile || repo.path.join("Cargo.toml").exists() {
                    let path = repo.path.clone();
                    let check = Arc::clone(&app.check);
                    {
                        let mut st = check.lock().unwrap();
                        st.running = true;
                        st.output.clear();
                    }
                    app.status = format!("running make check in {} …", repo.name);
                    thread::spawn(move || {
                        let out = git::run_check(&path);
                        let mut st = check.lock().unwrap();
                        st.running = false;
                        st.output = out;
                    });
                } else {
                    app.status = format!("{}: no Makefile/Cargo.toml", repo.name);
                }
            }
        }
        Tab::Secrets => {
            app.status = format!(
                "{} secret finding(s) — review; fix by gitignoring/removing",
                app.secrets.len()
            );
        }
        Tab::Hindsight => {
            // Enter on the Hindsight tab starts the service if it is down
            // (non-destructive: just launches the API). Stopping is a
            // deliberate S/Y action, confirmed before it runs.
            let hi = app.hindsight.clone();
            if !hi.running {
                run_hindsight_start(app);
            } else {
                // already running: reuse the two-step sweep confirmation
                app.confirm_sweep = true;
                app.status =
                    "Press Y to APPLY sweep (invalidates stale memories); any other key cancels"
                        .into();
            }
        }
        Tab::Backup => {
            let sh = BACKUP_SH.to_string();
            let cmd = Arc::clone(&app.backup);
            {
                let mut st = cmd.lock().unwrap();
                st.running = true;
                st.output.clear();
            }
            app.status = "running backup.sh (pulls cs/ss -> ~/Projects/Backups)…".into();
            thread::spawn(move || {
                let out = std::process::Command::new("bash").arg(&sh).output();
                let text = match out {
                    Ok(o) => {
                        let mut s = String::from_utf8_lossy(&o.stdout).to_string();
                        s.push_str(&String::from_utf8_lossy(&o.stderr));
                        if s.trim().is_empty() {
                            "done (no output)".into()
                        } else {
                            s
                        }
                    }
                    Err(e) => format!("failed to run backup.sh: {e}"),
                };
                let mut st = cmd.lock().unwrap();
                st.running = false;
                st.output = text;
            });
        }
    }
}

/// Mutating: PATCH-invalidate stale world/experience memories (never deletes).
fn apply_sweep(app: &mut App) {
    let (ok, failed) = hindsight::apply_sweep();
    app.sweep_status = format!("sweep applied: invalidated={ok} failed={failed}");
    app.hindsight = hindsight::info();
    app.status = format!("hindsight sweep applied (invalidated={ok}, failed={failed})");
}

/// Start the hindsight-api service (detached). Runs on a worker thread while
/// polling for readiness (startup can take ~30s: ONNX model + migrations). The
/// outcome string is written to a shared channel the main loop picks up.
fn run_hindsight_start(app: &mut App) {
    app.confirm_sweep = false;
    app.service_msg = "starting hindsight-api…".into();
    app.status = "starting hindsight-api (this can take ~30s)".into();
    let shared: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    app.hindsight_status = Arc::clone(&shared);
    thread::spawn(move || {
        let msg = hindsight::start();
        let mut s = shared.lock().unwrap();
        *s = msg;
    });
}

/// Stop the hindsight-api service. Runs synchronously (kill is fast).
fn run_hindsight_stop(app: &mut App) {
    let msg = hindsight::stop();
    app.service_msg = msg.clone();
    app.hindsight = hindsight::info();
    app.status = msg;
}

/// Snapshot of the local backup dir for the Backup tab (read-only `ls`).
fn backup_snapshot() -> String {
    let out = std::process::Command::new("bash")
        .args([
            "-c",
            &format!("ls -lt {} 2>/dev/null | head -8", BACKUP_DIR),
        ])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => "(backup dir unreadable)".into(),
    }
}

fn draw(f: &mut ratatui::Frame, app: &App) {
    let size = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .split(size);

    let titles = ["[1] Projects", "[2] Secrets", "[3] Hindsight", "[4] Backup"];
    let tab_idx = match app.tab {
        Tab::Projects => 0,
        Tab::Secrets => 1,
        Tab::Hindsight => 2,
        Tab::Backup => 3,
    };
    // Tabs renders its labels on a single line. No bordered Block wrapper:
    // a bordered Block would consume the row and clip the labels.
    let tabs = Tabs::new(titles)
        .select(tab_idx)
        .style(if tab_idx == 0 {
            app.theme.accent()
        } else {
            app.theme.muted()
        })
        .highlight_style(app.theme.selected());
    f.render_widget(tabs, chunks[0]);

    match app.tab {
        Tab::Projects => draw_projects(f, app, chunks[1]),
        Tab::Secrets => draw_secrets(f, app, chunks[1]),
        Tab::Hindsight => draw_hindsight(f, app, chunks[1]),
        Tab::Backup => draw_backup(f, app, chunks[1]),
    }

    let footer_style = if app.status.contains("RUNNING") || app.status.contains("stopped") {
        app.theme.bad()
    } else if app.status.contains("failed") || app.status.contains("error") {
        app.theme.bad()
    } else {
        app.theme.muted()
    };
    let footer = format!(
        "wf v{VERSION} (theme: {})  |  autoscan {}\n{}",
        app.theme.name, app.last_scan, app.status
    );
    let status = Paragraph::new(footer)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(app.theme.border()),
        )
        .style(footer_style);
    f.render_widget(status, chunks[2]);
}

fn draw_projects(f: &mut ratatui::Frame, app: &App, area: ratatui::layout::Rect) {
    let rows: Vec<Row> = app
        .repos
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let flag = if r.dirty { "●" } else { " " };
            let ab = format!("↑{}↓{}", r.ahead, r.behind);
            let style = if i == app.selected {
                app.theme.selected()
            } else {
                Style::default()
            };
            // color the dirty flag + ahead/behind as caution
            let flag_style = if r.dirty {
                app.theme.warn()
            } else {
                app.theme.good()
            };
            let ab_style = if r.ahead > 0 || r.behind > 0 {
                app.theme.warn()
            } else {
                app.theme.muted()
            };
            Row::new(vec![
                Span::styled(flag.to_string(), flag_style),
                Span::styled(r.name.clone(), app.theme.value()),
                Span::styled(r.branch.clone(), app.theme.muted()),
                Span::styled(ab, ab_style),
                Span::styled(r.last_commit.clone(), app.theme.muted()),
                Span::styled(
                    if r.has_makefile { "make" } else { "-" }.to_string(),
                    app.theme.label(),
                ),
            ])
            .style(style)
        })
        .collect();
    let widths = [
        Constraint::Length(1),
        Constraint::Length(20),
        Constraint::Length(20),
        Constraint::Length(10),
        Constraint::Min(20),
        Constraint::Length(6),
    ];
    let table = Table::new(rows, widths)
        .header(Row::new(vec![
            Span::styled("", app.theme.label()),
            Span::styled("repo", app.theme.label()),
            Span::styled("branch", app.theme.label()),
            Span::styled("a/b", app.theme.label()),
            Span::styled("last commit", app.theme.label()),
            Span::styled("chk", app.theme.label()),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(app.theme.border())
                .title(Span::styled(
                    format!(
                        "Projects ({})  ●dirty:{}  ↓behind:{}  Enter: make check",
                        app.repos.len(),
                        app.repos.iter().filter(|r| r.dirty).count(),
                        app.repos.iter().filter(|r| r.behind > 0).count()
                    ),
                    app.theme.heading(),
                )),
        );
    f.render_widget(table, area);

    let st = app.check.lock().unwrap();
    if st.running || !st.output.is_empty() {
        let popup = Paragraph::new(format!(
            "{}\n{}",
            if st.running {
                "running make check…"
            } else {
                "make check result:"
            },
            st.output
        ))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(app.theme.border())
                .title(Span::styled("Check", app.theme.heading())),
        )
        .style(app.theme.value());
        let pop = centered_rect(60, 60, area);
        f.render_widget(popup, pop);
    }
}

fn draw_secrets(f: &mut ratatui::Frame, app: &App, area: ratatui::layout::Rect) {
    let rows: Vec<Row> = app
        .secrets
        .iter()
        .enumerate()
        .map(|(i, s)| {
            // Tracked secrets are the real risk (bright red); untracked = caution.
            let tracked = s.reason.contains("tracked");
            let base = if tracked {
                app.theme.bad()
            } else {
                app.theme.warn()
            };
            let style = if i == app.selected {
                app.theme.selected()
            } else {
                base
            };
            Row::new(vec![
                Span::styled(s.repo.clone(), app.theme.label()),
                Span::styled(s.file.clone(), app.theme.value()),
                Span::styled(s.reason.clone(), base),
            ])
            .style(style)
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(20),
            Constraint::Min(20),
            Constraint::Min(20),
        ],
    )
    .header(Row::new(vec![
        Span::styled("repo", app.theme.label()),
        Span::styled("file", app.theme.label()),
        Span::styled("reason", app.theme.label()),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(app.theme.border())
            .title(Span::styled(
                format!("Secrets ({})", app.secrets.len()),
                app.theme.heading(),
            )),
    );
    f.render_widget(table, area);
}

fn draw_hindsight(f: &mut ratatui::Frame, app: &App, area: ratatui::layout::Rect) {
    let th = app.theme;
    let hi = &app.hindsight;
    let v = app.verbose;
    let mut lines: Vec<Line> = Vec::new();

    // in verbose mode, append a muted one-line plain-English caption
    let push_cap = |lines: &mut Vec<Line>, s: &str| {
        if v {
            lines.push(Line::from(Span::styled(s.to_string(), th.muted())));
        }
    };

    // --- status ---
    let state_line = if hi.running {
        Span::styled(format!("● RUNNING  (v{})", hi.version), th.good())
    } else {
        Span::styled("○ STOPPED", th.bad())
    };
    lines.push(Line::from(vec![
        Span::styled("Status: ", th.label()),
        state_line,
    ]));
    lines.push(Line::from(vec![
        Span::styled("URL: ", th.label()),
        Span::styled(format!("{}/health", hindsight::API_URL), th.value()),
    ]));
    push_cap(
        &mut lines,
        "The local hindsight-api is a process; this shows whether it answers health checks.",
    );
    lines.push(Line::from(""));

    // --- bank stats (from /stats) ---
    lines.push(Line::from(Span::styled(
        "Bank hermes @ localhost:8888",
        th.heading(),
    )));
    let stat = |label: &str, val: String| {
        Line::from(vec![
            Span::styled(format!("{label}: "), th.label()),
            Span::styled(val, th.value()),
        ])
    };
    if hi.running {
        lines.push(stat("Total memories", hi.total_memories.to_string()));
        lines.push(stat("Graph links", hi.total_links.to_string()));
        lines.push(stat("Documents", hi.total_documents.to_string()));
        lines.push(stat(
            "By type",
            format!(
                "world={} experience={} observation={}",
                hi.fact_world, hi.fact_experience, hi.fact_observation
            ),
        ));
    } else {
        lines.push(stat("Total memories", "-".into()));
    }
    push_cap(
        &mut lines,
        "Counts of stored memory units + the relationship graph the engine retrieves over.",
    );
    lines.push(Line::from(""));

    // --- stale candidates (dry-run preview) ---
    lines.push(Line::from(vec![
        Span::styled("Stale candidates: ", th.label()),
        Span::styled(hi.stale_candidates.to_string(), th.warn()),
    ]));
    push_cap(&mut lines, "Memories matching stale-wrong patterns with no correction signal. This is a DRY-RUN preview of the sweep — nothing has changed. Press Enter then Y to actually invalidate them.");

    // --- live metrics (from /metrics) ---
    lines.push(Line::from(Span::styled(
        "Live metrics (from /metrics):",
        th.heading(),
    )));
    push_cap(&mut lines, "Cloud LLM + operation counters, scraped read-only from the service's Prometheus endpoint. Per-scan deltas show activity since the last refresh.");
    if hi.running {
        let p = &app.prev_hindsight;
        let dd = |cur: u64, prev: u64| {
            if cur > prev {
                format!(" (+{} /scan)", cur - prev)
            } else {
                String::new()
            }
        };
        lines.push(Line::from(vec![
            Span::styled("  LLM calls: ", th.label()),
            Span::styled(hi.llm_calls.to_string(), th.value()),
            Span::styled(dd(hi.llm_calls, p.llm_calls), th.muted()),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  in/out tokens: ", th.label()),
            Span::styled(hi.llm_in_tokens.to_string(), th.value()),
            Span::styled("/", th.muted()),
            Span::styled(hi.llm_out_tokens.to_string(), th.value()),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  by scope: ", th.label()),
            Span::styled(
                format!(
                    "retain={} consol={} verify={}",
                    hi.llm_calls_retain, hi.llm_calls_consolidation, hi.llm_calls_verification
                ),
                th.value(),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  Ops: ", th.label()),
            Span::styled(
                format!(
                    "retain={}{} recall={}{} reflect={}{} consol={}{}",
                    hi.op_retain,
                    dd(hi.op_retain, p.op_retain),
                    hi.op_recall,
                    dd(hi.op_recall, p.op_recall),
                    hi.op_reflect,
                    dd(hi.op_reflect, p.op_reflect),
                    hi.op_consolidation,
                    dd(hi.op_consolidation, p.op_consolidation),
                ),
                th.value(),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  Process: ", th.label()),
            Span::styled(
                format!(
                    "rss={} MB cpu={:.0}s fds={} dbpool={}",
                    hi.proc_rss_mb, hi.proc_cpu_secs, hi.proc_open_fds, hi.db_pool_size
                ),
                th.value(),
            ),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "  LLM/process metrics: n/a (service down)",
            th.muted(),
        )));
    }
    lines.push(Line::from(""));

    // --- local models (from log) ---
    lines.push(Line::from(Span::styled(
        "Local models (from log):",
        th.heading(),
    )));
    push_cap(&mut lines, "The embedder + reranker run ON your machine (not the cloud) and emit no /metrics. wf recovers their activity from the service log instead.");
    if !hi.running {
        lines.push(Line::from(Span::styled("  n/a (service down)", th.muted())));
    } else if !hi.log_readable {
        lines.push(Line::from(Span::styled(
            "  log n/a (set HINDSIGHT_API_LOG if started outside wf)",
            th.muted(),
        )));
    } else {
        let p = &app.prev_hindsight;
        let dd = |cur: u64, prev: u64| {
            if cur > prev {
                format!(" (+{} /scan)", cur - prev)
            } else {
                String::new()
            }
        };
        lines.push(Line::from(vec![
            Span::styled("  Reranker: ", th.label()),
            Span::styled(format!("{} calls", hi.rerank_calls), th.value()),
            Span::styled(dd(hi.rerank_calls, p.rerank_calls), th.muted()),
            Span::styled(format!(" {} candidates", hi.rerank_candidates), th.value()),
            Span::styled(format!(" last {:.1}s", hi.rerank_last_secs), th.muted()),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  Embedder: ", th.label()),
            Span::styled(
                format!("{} retain units", hi.embed_retain_units),
                th.value(),
            ),
            Span::styled(dd(hi.embed_retain_units, p.embed_retain_units), th.muted()),
            Span::styled(
                format!(" {} query embeds", hi.embed_query_calls),
                th.value(),
            ),
            Span::styled(dd(hi.embed_query_calls, p.embed_query_calls), th.muted()),
            Span::styled(
                format!(" {} consol calls", hi.embed_consolidation_calls),
                th.value(),
            ),
        ]));
    }
    lines.push(Line::from(""));

    // --- observations mission ---
    lines.push(Line::from(Span::styled(
        "Observations mission:",
        th.heading(),
    )));
    for w in wrap(&hi.observations_mission, 72).split('\n') {
        lines.push(Line::from(Span::styled(w.to_string(), th.muted())));
    }
    lines.push(Line::from(""));

    // --- service msg / sweep status / controls ---
    if !app.service_msg.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("Service: {}", app.service_msg),
            th.accent(),
        )));
    }
    if !app.sweep_status.is_empty() {
        lines.push(Line::from(Span::styled(
            app.sweep_status.clone(),
            th.good(),
        )));
    }
    let bottom = if app.confirm_stop {
        "CONFIRM STOP: press Y to STOP hindsight-api.\nAny other key cancels."
    } else if app.confirm_sweep {
        "WARNING: press Y to APPLY sweep (invalidates stale\nworld/experience memories). Any other key cancels."
    } else if hi.running {
        "Enter: run stale-memory sweep\nS: stop service"
    } else {
        "Enter: START hindsight-api\n(needs the local service up for memory ops)"
    };
    for bl in bottom.split('\n') {
        lines.push(Line::from(Span::styled(
            bl.to_string(),
            if app.confirm_stop {
                th.bad()
            } else {
                th.label()
            },
        )));
    }

    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(th.border())
            .title(Span::styled("Hindsight", th.heading())),
    );
    f.render_widget(p, area);
}

/// Hard-wrap `text` to lines of at most `width` chars (graceful on long words).
fn wrap(text: &str, width: usize) -> String {
    let mut out = String::new();
    for word in text.split_whitespace() {
        if out.is_empty() {
            out.push_str(word);
            continue;
        }
        let last = out
            .rfind('\n')
            .map(|i| out.len() - i - 1)
            .unwrap_or(out.len());
        if last + 1 + word.len() > width {
            out.push('\n');
            out.push_str(word);
        } else {
            out.push(' ');
            out.push_str(word);
        }
    }
    out
}

fn draw_backup(f: &mut ratatui::Frame, app: &App, area: ratatui::layout::Rect) {
    let th = app.theme;
    let body = format!(
        "Backup SOP: {}\n\nRuns the existing backup.sh — rsync media + sqlite VACUUM dump\nfrom remote servers cs/ss -> {}.\n\nEnter: run backup (pulls remote -> local)\n\nLocal backups in {}:\n{}",
        BACKUP_SH,
        BACKUP_DIR,
        BACKUP_DIR,
        app.backup_list,
    );
    let p = Paragraph::new(body)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(th.border())
                .title(Span::styled(
                    "[4] Backup  Enter: run backup.sh",
                    th.heading(),
                )),
        )
        .style(th.value());
    f.render_widget(p, area);

    let st = app.backup.lock().unwrap();
    if st.running || !st.output.is_empty() {
        let head = if st.running {
            "running backup.sh…"
        } else {
            "backup.sh output:"
        };
        let popup = Paragraph::new(format!("{head}\n{}", st.output))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(th.border())
                    .title(Span::styled("Backup", th.heading())),
            )
            .style(th.value());
        f.render_widget(popup, centered_rect(70, 70, area));
    }
}

fn centered_rect(
    percent_x: u16,
    percent_y: u16,
    area: ratatui::layout::Rect,
) -> ratatui::layout::Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(v[1])[1]
}

// ---- headless modes ----
fn headless_list(root: &PathBuf) -> io::Result<()> {
    let repos = git::discover(root);
    for p in &repos {
        let h = git::health(p);
        println!(
            "{}\t{}\t↑{}/↓{}\t{}\t{}",
            h.name,
            h.branch,
            h.ahead,
            h.behind,
            if h.dirty { "dirty" } else { "clean" },
            if h.has_makefile { "make" } else { "-" }
        );
    }
    Ok(())
}

fn headless_secrets(root: &PathBuf) -> io::Result<()> {
    let repos = git::discover(root);
    let mut total = 0;
    for p in &repos {
        for f in secrets::scan_repo(p) {
            println!("{}\t{}\t{}", f.repo, f.file, f.reason);
            total += 1;
        }
    }
    eprintln!("total secret findings: {total}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn sample_app() -> App {
        App {
            tab: Tab::Projects,
            repos: vec![],
            selected: 0,
            check: Arc::new(Mutex::new(CheckState::default())),
            secrets: vec![],
            hindsight: hindsight::BankInfo::default(),
            sweep_status: String::new(),
            backup: Arc::new(Mutex::new(CheckState::default())),
            backup_list: String::new(),
            confirm_sweep: false,
            confirm_stop: false,
            status: String::new(),
            service_msg: String::new(),
            hindsight_status: Arc::new(Mutex::new(String::new())),
            prev_hindsight: hindsight::BankInfo::default(),
            last_scan: String::new(),
            theme: theme::DARK,
            verbose: false,
        }
    }

    /// Render the Hindsight tab and confirm the service status + running
    /// controls actually appear (catches a silently-empty panel).
    #[test]
    fn hindsight_panel_renders_status() {
        let mut app = sample_app();
        app.tab = Tab::Hindsight;
        // Simulate a running service so the panel shows the RUNNING marker
        // and the start/stop control line.
        app.hindsight = hindsight::BankInfo {
            running: true,
            version: "0.8.4".into(),
            total_memories: 1339,
            total_links: 100705,
            total_documents: 17,
            fact_world: 1136,
            fact_experience: 230,
            fact_observation: 246,
            stale_candidates: 9,
            ..Default::default()
        };
        let backend = TestBackend::new(100, 40);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| draw(f, &app)).expect("draw");

        let rendered: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();

        assert!(
            rendered.contains("RUNNING"),
            "Hindsight status (RUNNING) missing from render:\n{rendered}"
        );
        assert!(
            rendered.contains("stop service"),
            "service controls missing from render:\n{rendered}"
        );
        assert!(
            rendered.contains("Graph links:"),
            "bank statistics missing from render:\n{rendered}"
        );
    }
}
