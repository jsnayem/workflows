//! Hindsight bank introspection + dry-run sweep. Talks to localhost:8888 via curl.
use serde_json::Value;

const BASE: &str = "http://127.0.0.1:8888";
const BANK: &str = "hermes";

#[derive(Debug, Clone, Default)]
pub struct BankInfo {
    pub total_memories: u64,
    pub observations_mission: String,
    pub stale_candidates: usize,
}

fn curl_json(url: &str) -> Option<Value> {
    let out = std::process::Command::new("curl")
        .args(["-s", "-m", "12", url])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

pub fn info() -> BankInfo {
    let mut info = BankInfo::default();
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
            let out = std::process::Command::new("curl")
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
