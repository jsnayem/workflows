//! Hindsight bank introspection, service control, and dry-run sweep.
//! Talks to the local slim API on localhost:8888 via curl.
use serde_json::Value;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const BASE: &str = "http://127.0.0.1:8888";
/// Public URL of the local slim API (used by the TUI status line).
pub const API_URL: &str = BASE;
const BANK: &str = "hermes";
const VENV_BIN: &str = "/home/nayem/Projects/hindsight/.venv/bin";
const HINDSIGHT_DIR: &str = "/home/nayem/Projects/hindsight";
/// Where the `hindsight-api` process logs once we detach it from the TUI.
const API_LOG: &str = "/tmp/hindsight-api.log";

#[derive(Debug, Clone, Default)]
pub struct BankInfo {
    /// True when the API answers /health.
    pub running: bool,
    /// API version reported by /version (empty when down).
    pub version: String,
    pub total_memories: u64,
    pub observations_mission: String,
    pub stale_candidates: usize,
}

/// Run curl, return stdout text on HTTP success (else None).
fn curl_text(url: &str) -> Option<String> {
    let out = Command::new("curl")
        .args(["-s", "-m", "2", url])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

fn curl_json(url: &str) -> Option<Value> {
    let t = curl_text(url)?;
    serde_json::from_str::<Value>(&t).ok()
}

/// The slim API answers /health while it is up.
pub fn is_running() -> bool {
    curl_text(&format!("{BASE}/health")).is_some()
}

/// API version string from /version, e.g. "0.8.4".
pub fn api_version() -> Option<String> {
    curl_text(&format!("{BASE}/version")).and_then(|t| {
        serde_json::from_str::<Value>(&t).ok().and_then(|v| {
            v.get("api_version")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
        })
    })
}

/// Redirect a stdio to the API log file, or to /dev/null if it can't be opened.
fn file_or_null(path: &str) -> Stdio {
    match std::fs::File::create(path) {
        Ok(f) => Stdio::from(f),
        Err(_) => Stdio::null(),
    }
}

/// Start the hindsight-api server detached from the TUI (own process group,
/// log to API_LOG). Reuses a still-running embedded Postgres if present.
/// Returns a human-readable status string.
pub fn start() -> String {
    if is_running() {
        return "hindsight-api already running".into();
    }
    let api = PathBuf::from(VENV_BIN).join("hindsight-api");
    if !api.exists() {
        return format!("hindsight-api not found at {}", api.display());
    }
    let mut cmd = Command::new(&api);
    cmd.current_dir(HINDSIGHT_DIR)
        .stdout(file_or_null(API_LOG))
        .stderr(file_or_null(API_LOG))
        .env(
            "PATH",
            format!("{}:{}", VENV_BIN, std::env::var("PATH").unwrap_or_default()),
        )
        // New process group so it outlives / is independent of the TUI.
        .process_group(0);
    match cmd.spawn() {
        Ok(_) => {}
        Err(e) => return format!("failed to launch hindsight-api: {e}"),
    }
    // Poll for readiness (server initializes embeddings + migrations, can take a bit).
    for _ in 0..45 {
        std::thread::sleep(std::time::Duration::from_secs(1));
        if is_running() {
            let v = api_version().unwrap_or_default();
            return format!("started — listening on {BASE} (v{v}); log: {API_LOG}");
        }
    }
    format!("launched but not responding after 45s; check {API_LOG}")
}

/// Stop the hindsight-api server by PID (matched via /proc cmdline, so the
/// TUI/wf process is never matched). Leaves the embedded Postgres daemon alone.
/// Returns a human-readable status string.
pub fn stop() -> String {
    if !is_running() {
        return "hindsight-api not running".into();
    }
    let target = PathBuf::from(VENV_BIN)
        .join("hindsight-api")
        .to_string_lossy()
        .to_string();
    let mut killed = 0;
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for e in entries.flatten() {
            let pid_s = e.file_name().to_string_lossy().to_string();
            if !pid_s.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let cmd = std::fs::read(format!("/proc/{pid_s}/cmdline")).unwrap_or_default();
            let cmd_str = String::from_utf8_lossy(&cmd).replace('\0', " ");
            if cmd_str.contains(&target) {
                if let Ok(_) = Command::new("kill").arg(&pid_s).status() {
                    killed += 1;
                }
            }
        }
    }
    if killed == 0 {
        return "could not find hindsight-api process to stop".into();
    }
    // Poll for shutdown.
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if !is_running() {
            return format!("stopped ({killed} process killed)");
        }
    }
    "sent stop signal but API still responding; may need manual kill".into()
}

pub fn info() -> BankInfo {
    let running = is_running();
    if !running {
        return BankInfo {
            running: false,
            version: String::new(),
            ..Default::default()
        };
    }
    let version = api_version().unwrap_or_default();
    let mut info = BankInfo {
        running: true,
        version,
        ..Default::default()
    };
    if let Some(v) = curl_json(&format!(
        "{BASE}/v1/default/banks/{BANK}/memories/list?limit=1"
    )) {
        info.total_memories = v.get("total").and_then(|t| t.as_u64()).unwrap_or(0);
    }
    if let Some(v) = curl_json(&format!("{BASE}/v1/default/banks/{BANK}/config")) {
        if let Some(cfg) = v.get("config") {
            info.observations_mission = cfg
                .get("observations_mission")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
        }
    }
    info.stale_candidates = dry_run_sweep_count();
    info
}

/// Dry-run: list memories, count those matching stale-wrong patterns that carry
/// NO correction/truth signal. Mirrors the correction-aware logic used to purge.
/// Read-only — never invalidates.
pub fn dry_run_sweep_count() -> usize {
    let url = format!("{BASE}/v1/default/banks/{BANK}/memories/list?limit=600");
    let v = match curl_json(&url) {
        Some(v) => v,
        None => return 0,
    };
    let items = v
        .get("items")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default();
    let stale = [
        "406 tests",
        "410 tests",
        "still NULL",
        "not yet utilized",
        "remain unused",
        "one commit ahead",
        "on-demand",
        "no cron",
        "human-dependent",
        "failing test",
        "red pre-push",
    ];
    let keep = [
        "415",
        "41/45",
        "merged",
        "verified",
        "proven",
        "not NULL",
        "no failing",
        "SUPERSEDED",
        "stale",
        "false",
        "correct",
        "not used",
    ];
    let mut count = 0;
    for m in &items {
        let text = m
            .get("text")
            .or_else(|| m.get("content"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let is_stale = stale.iter().any(|s| text.contains(s));
        let is_kept = keep.iter().any(|s| text.contains(s));
        if is_stale && !is_kept {
            count += 1;
        }
    }
    count
}

/// Apply the sweep: PATCH-invalidate the uncorrected stale world/experience
/// memories. DESTRUCTIVE — only call when the user confirms in the TUI.
/// Returns (invalidated, failed).
pub fn apply_sweep() -> (usize, usize) {
    let url = format!("{BASE}/v1/default/banks/{BANK}/memories/list?limit=600");
    let v = match curl_json(&url) {
        Some(v) => v,
        None => return (0, 0),
    };
    let items = v
        .get("items")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default();
    let stale = [
        "406 tests",
        "410 tests",
        "still NULL",
        "not yet utilized",
        "remain unused",
        "one commit ahead",
        "on-demand",
        "no cron",
        "human-dependent",
        "failing test",
        "red pre-push",
    ];
    let keep = [
        "415",
        "41/45",
        "merged",
        "verified",
        "proven",
        "not NULL",
        "no failing",
        "SUPERSEDED",
        "stale",
        "false",
        "correct",
        "not used",
    ];
    let mut ok = 0;
    let mut failed = 0;
    for m in &items {
        let id = m.get("id").and_then(|t| t.as_str()).unwrap_or("");
        if id.is_empty() {
            continue;
        }
        let fact_type = m.get("fact_type").and_then(|t| t.as_str()).unwrap_or("");
        if fact_type == "observation" {
            continue; // observations regenerate; handled by observations_mission
        }
        let text = m
            .get("text")
            .or_else(|| m.get("content"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let is_stale = stale.iter().any(|s| text.contains(s));
        let is_kept = keep.iter().any(|s| text.contains(s));
        if is_stale && !is_kept {
            let body = serde_json::json!({"state": "invalidated", "reason": "stale-wrong: superseded by live-verified fact"});
            let out = Command::new("curl")
                .args([
                    "-s",
                    "-m",
                    "15",
                    "-X",
                    "PATCH",
                    &format!("{BASE}/v1/default/banks/{BANK}/memories/{id}"),
                    "-H",
                    "Content-Type: application/json",
                    "-d",
                    &body.to_string(),
                ])
                .output();
            match out {
                Ok(o) if o.status.success() => ok += 1,
                _ => failed += 1,
            }
        }
    }
    (ok, failed)
}
