//! Git repo discovery + health, read-only.
use std::path::{Path, PathBuf};
use std::process::Command;

/// A discovered git repository and its (cheap) health snapshot.
#[derive(Debug, Clone)]
pub struct Repo {
    pub path: PathBuf,
    pub name: String,
    pub branch: String,
    pub ahead: u32,
    pub behind: u32,
    pub dirty: bool,
    pub last_commit: String,
    pub has_makefile: bool,
}

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Recursively find git repos under `root`, stopping at the first `.git`
/// (so we never descend into a repo's internals / nested `target`).
pub fn discover(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(root, &mut found);
    found.sort();
    found
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let git_dir = dir.join(".git");
    if git_dir.exists() {
        out.push(dir.to_path_buf());
        return; // do not descend into the repo
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let base = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        // skip heavy / irrelevant dirs
        if base == "target" || base == "node_modules" || base == ".git" || base == ".venv" {
            continue;
        }
        walk(&p, out);
    }
}

/// Snapshot cheap health for one repo (no `make check`).
pub fn health(repo: &Path) -> Repo {
    let name = repo
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string();
    let branch = git(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "?".into());
    let (ahead, behind) = match git(
        repo,
        &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"],
    ) {
        Some(s) => {
            let mut it = s.split('\t');
            let a = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let b = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            (a, b)
        }
        None => (0, 0),
    };
    let dirty = git(repo, &["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let last_commit = git(repo, &["log", "-1", "--format=%h %s"]).unwrap_or_default();
    let has_makefile = repo.join("Makefile").exists();
    Repo {
        path: repo.to_path_buf(),
        name,
        branch,
        ahead,
        behind,
        dirty,
        last_commit,
        has_makefile,
    }
}

/// Run `make check` (or `cargo check`) in the repo; returns combined stdout/stderr.
/// Blocking — call from a worker thread in the TUI.
pub fn run_check(repo: &Path) -> String {
    let cmd = if repo.join("Makefile").exists() {
        vec!["check"]
    } else if repo.join("Cargo.toml").exists() {
        vec!["check"]
    } else {
        return "(no Makefile/Cargo.toml — nothing to check)".into();
    };
    let tool = if repo.join("Makefile").exists() {
        "make"
    } else {
        "cargo"
    };
    let out = Command::new(tool).current_dir(repo).args(&cmd).output();
    match out {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).to_string();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            if s.trim().is_empty() {
                s.push_str(if o.status.success() {
                    "OK (exit 0, no output)"
                } else {
                    "FAILED (exit non-zero)"
                });
            }
            s
        }
        Err(e) => format!("failed to run {tool}: {e}"),
    }
}
