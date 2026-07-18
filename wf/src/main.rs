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

const PROJECTS_ROOT: &str = "/home/nayem/Projects";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const HELP: &str = "1/2/3 tabs | ↑↓ select | Enter action | r refresh | q quit";

#[derive(Debug, Clone)]
enum Tab {
    Projects,
    Secrets,
    Hindsight,
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
    confirm_sweep: bool,
    status: String,
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

    let repos = load_repos(&root);
    let secrets_findings = git::discover(&root)
        .iter()
        .flat_map(|p| secrets::scan_repo(p))
        .collect();
    let hi = hindsight::info();

    let mut app = App {
        tab: Tab::Projects,
        repos,
        selected: 0,
        check: Arc::new(Mutex::new(CheckState::default())),
        secrets: secrets_findings,
        hindsight: hi,
        sweep_status: String::new(),
        confirm_sweep: false,
        status: HELP.into(),
    };

    loop {
        term.draw(|f| draw(f, &app))?;
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
                    };
                    if n > 0 && app.selected < n - 1 {
                        app.selected += 1;
                    }
                }
                KeyCode::Char('r') => refresh(&mut app, &root),
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if app.confirm_sweep {
                        apply_sweep(&mut app);
                        app.confirm_sweep = false;
                    }
                }
                KeyCode::Enter => handle_enter(&mut app),
                _ => {}
            }
        }
    }

    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen)?;
    Ok(())
}

fn prev_tab(t: &Tab) -> Tab {
    match t {
        Tab::Projects => Tab::Hindsight,
        Tab::Secrets => Tab::Projects,
        Tab::Hindsight => Tab::Secrets,
    }
}
fn next_tab(t: &Tab) -> Tab {
    match t {
        Tab::Projects => Tab::Secrets,
        Tab::Secrets => Tab::Hindsight,
        Tab::Hindsight => Tab::Projects,
    }
}

fn refresh(app: &mut App, root: &PathBuf) {
    app.repos = load_repos(root);
    if app.selected >= app.repos.len() {
        app.selected = app.repos.len().saturating_sub(1);
    }
    app.secrets = git::discover(root)
        .iter()
        .flat_map(|p| secrets::scan_repo(p))
        .collect();
    app.hindsight = hindsight::info();
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
            // two-step: Enter prompts, Y confirms (mutating action)
            app.confirm_sweep = true;
            app.status =
                "Press Y to APPLY sweep (invalidates stale memories); any other key cancels".into();
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

    let titles = ["[1] Projects", "[2] Secrets", "[3] Hindsight"];
    let tab_idx = match app.tab {
        Tab::Projects => 0,
        Tab::Secrets => 1,
        Tab::Hindsight => 2,
    };
    let tabs = Tabs::new(titles)
        .select(tab_idx)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("wf v{VERSION}")),
        )
        .style(Style::default().fg(Color::Cyan));
    f.render_widget(tabs, chunks[0]);

    match app.tab {
        Tab::Projects => draw_projects(f, app, chunks[1]),
        Tab::Secrets => draw_secrets(f, app, chunks[1]),
        Tab::Hindsight => draw_hindsight(f, app, chunks[1]),
    }

    let footer = format!("{}\n{}", app.status, HELP);
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
    let text = format!(
        "Bank: hermes @ localhost:8888\nTotal memories: {}\nStale candidates (dry-run): {}\n\nObservations mission:\n{}\n\n{}\n\n{}",
        hi.total_memories,
        hi.stale_candidates,
        hi.observations_mission,
        app.sweep_status,
        if app.confirm_sweep {
            "WARNING: press Y to APPLY sweep (invalidates stale world/experience memories). Any other key cancels."
        } else {
            "Enter: review the stale-memory sweep (asks for confirmation before applying)."
        },
    );
    let p = Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("Hindsight"));
    f.render_widget(p, area);
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
    println!("total_memories: {}", hi.total_memories);
    println!("stale_candidates_dry_run: {}", hi.stale_candidates);
    println!("observations_mission: {}", hi.observations_mission);
    Ok(())
}
