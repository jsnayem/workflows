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

    let repos = git::discover(&root)
        .iter()
        .map(|p| git::health(p))
        .collect();
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
        status: "q: quit | ←→: tab | ↑↓: select | r: refresh | Enter: action".into(),
    };

    loop {
        term.draw(|f| draw(f, &app))?;
        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Char('q') => break,
                KeyCode::Left | KeyCode::Char('h') => app.tab = prev_tab(&app.tab),
                KeyCode::Right | KeyCode::Char('l') => app.tab = next_tab(&app.tab),
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
    app.repos = git::discover(root).iter().map(|p| git::health(p)).collect();
    app.secrets = git::discover(root)
        .iter()
        .flat_map(|p| secrets::scan_repo(p))
        .collect();
    app.hindsight = hindsight::info();
    app.status = "refreshed".into();
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
            // confirm-then-apply sweep
            let (ok, failed) = hindsight::apply_sweep();
            app.sweep_status = format!("sweep applied: invalidated={ok} failed={failed}");
            app.hindsight = hindsight::info();
            app.status = "hindsight sweep applied".into();
        }
    }
}

fn draw(f: &mut ratatui::Frame, app: &App) {
    let size = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(size);

    let titles = ["Projects", "Secrets", "Hindsight"];
    let tab_idx = match app.tab {
        Tab::Projects => 0,
        Tab::Secrets => 1,
        Tab::Hindsight => 2,
    };
    let tabs = Tabs::new(titles)
        .select(tab_idx)
        .block(Block::default().borders(Borders::ALL).title("wf"))
        .style(Style::default().fg(Color::Cyan));
    f.render_widget(tabs, chunks[0]);

    match app.tab {
        Tab::Projects => draw_projects(f, app, chunks[1]),
        Tab::Secrets => draw_secrets(f, app, chunks[1]),
        Tab::Hindsight => draw_hindsight(f, app, chunks[1]),
    }

    let status = Paragraph::new(app.status.clone()).block(Block::default().borders(Borders::ALL));
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
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("Projects ({})  Enter: make check", app.repos.len())),
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
        "Bank: hermes @ localhost:8888\nTotal memories: {}\nStale candidates (dry-run): {}\n\nObservations mission:\n{}\n\n{}\n\nEnter: APPLY sweep (invalidates stale world/experience memories)",
        hi.total_memories,
        hi.stale_candidates,
        hi.observations_mission,
        app.sweep_status,
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
