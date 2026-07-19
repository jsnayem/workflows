//! wf — cross-project management TUI (Ratatui).
//!
//! Tabs: Projects | Secrets | Hindsight.
//! Headless: `wf --list`, `wf --secrets`, `wf --hindsight` (no TUI; for cron/CI).
mod git;
mod hindsight;
mod secrets;

use crossterm::event::{self, Event, KeyCode};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
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
    "1/2/3/4 tabs | ↑↓ select | Enter action | r refresh | R rebuild+restart | q quit | Hindsight: Enter start / S stop";

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
    // wall-clock of the last live rescan (shown in the footer so you can see it tick)
    last_scan: String,
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
    if args.iter().any(|a| a == "--hindsight") {
        return headless_hindsight();
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
        last_scan: init.stamp,
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
        .style(Style::default().fg(Color::Cyan));
    f.render_widget(tabs, chunks[0]);

    match app.tab {
        Tab::Projects => draw_projects(f, app, chunks[1]),
        Tab::Secrets => draw_secrets(f, app, chunks[1]),
        Tab::Hindsight => draw_hindsight(f, app, chunks[1]),
        Tab::Backup => draw_backup(f, app, chunks[1]),
    }

    let footer = format!(
        "wf v{VERSION}  |  autoscan {}\n{}",
        app.last_scan, app.status
    );
    let status = Paragraph::new(footer).block(Block::default().borders(Borders::ALL));
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
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            Row::new(vec![
                flag.to_string(),
                r.name.clone(),
                r.branch.clone(),
                ab,
                r.last_commit.clone(),
                if r.has_makefile { "make" } else { "-" }.to_string(),
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
            "",
            "repo",
            "branch",
            "a/b",
            "last commit",
            "chk",
        ]))
        .block(Block::default().borders(Borders::ALL).title(format!(
            "Projects ({})  ●dirty:{}  ↓behind:{}  Enter: make check",
            app.repos.len(),
            app.repos.iter().filter(|r| r.dirty).count(),
            app.repos.iter().filter(|r| r.behind > 0).count()
        )));
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
        .block(Block::default().borders(Borders::ALL).title("Check"));
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
            let style = if i == app.selected {
                Style::default()
                    .add_modifier(Modifier::REVERSED)
                    .fg(Color::Red)
            } else {
                Style::default().fg(Color::Red)
            };
            Row::new(vec![s.repo.clone(), s.file.clone(), s.reason.clone()]).style(style)
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
    .header(Row::new(vec!["repo", "file", "reason"]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("Secrets ({})", app.secrets.len())),
    );
    f.render_widget(table, area);
}

fn draw_hindsight(f: &mut ratatui::Frame, app: &App, area: ratatui::layout::Rect) {
    let hi = &app.hindsight;
    let state_line = if hi.running {
        format!("● RUNNING  (v{})", hi.version)
    } else {
        "○ STOPPED".to_string()
    };
    let controls = if hi.running {
        "Enter: run stale-memory sweep   S: stop service"
    } else {
        "Enter: START hindsight-api   (needs the local service up for memory ops)"
    };
    let stats = if hi.running {
        format!(
            "Total memories: {}\nGraph links: {}\nDocuments: {}\nBy type: world={} experience={} observation={}\nStale candidates (dry-run): {}",
            hi.total_memories,
            hi.total_links,
            hi.total_documents,
            hi.fact_world,
            hi.fact_experience,
            hi.fact_observation,
            hi.stale_candidates,
        )
    } else {
        "Total memories: -\nStale candidates (dry-run): -".to_string()
    };
    let text = format!(
        "Status: {state_line}\nURL: {}/health\n\nBank hermes @ localhost:8888\n{stats}\n\nObservations mission:\n{}\n\n{}\n\n{}\n\n{}\n\n{}",
        hindsight::API_URL,
        hi.observations_mission,
        if !app.service_msg.is_empty() {
            format!("Service: {}", app.service_msg)
        } else {
            String::new()
        },
        app.sweep_status,
        if app.confirm_stop {
            "CONFIRM STOP: press Y to STOP hindsight-api. Any other key cancels."
        } else if app.confirm_sweep {
            "WARNING: press Y to APPLY sweep (invalidates stale world/experience memories). Any other key cancels."
        } else {
            controls
        },
        controls,
    );
    let p = Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("Hindsight"));
    f.render_widget(p, area);
}

fn draw_backup(f: &mut ratatui::Frame, app: &App, area: ratatui::layout::Rect) {
    let body = format!(
        "Backup SOP: {}\n\nRuns the existing backup.sh — rsync media + sqlite VACUUM dump\nfrom remote servers cs/ss -> {}.\n\nEnter: run backup (pulls remote -> local)\n\nLocal backups in {}:\n{}",
        BACKUP_SH,
        BACKUP_DIR,
        BACKUP_DIR,
        app.backup_list,
    );
    let p = Paragraph::new(body).block(
        Block::default()
            .borders(Borders::ALL)
            .title("[4] Backup  Enter: run backup.sh"),
    );
    f.render_widget(p, area);

    let st = app.backup.lock().unwrap();
    if st.running || !st.output.is_empty() {
        let head = if st.running {
            "running backup.sh…"
        } else {
            "backup.sh output:"
        };
        let popup = Paragraph::new(format!("{head}\n{}", st.output))
            .block(Block::default().borders(Borders::ALL).title("Backup"));
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

fn headless_hindsight() -> io::Result<()> {
    let hi = hindsight::info();
    println!("running: {}", hi.running);
    println!("version: {}", hi.version);
    println!("total_memories: {}", hi.total_memories);
    println!("total_links: {}", hi.total_links);
    println!("total_documents: {}", hi.total_documents);
    println!(
        "by_type: world={} experience={} observation={}",
        hi.fact_world, hi.fact_experience, hi.fact_observation
    );
    println!("stale_candidates_dry_run: {}", hi.stale_candidates);
    println!("observations_mission: {}", hi.observations_mission);
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
            last_scan: String::new(),
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
