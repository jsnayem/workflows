//! Secret-scan across repos: filename + light content heuristics. Read-only.
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct Finding {
    pub repo: String,
    pub file: String,
    pub reason: String,
}

/// Filenames that are almost always secrets / credentials.
const SECRET_FILES: &[&str] = &[
    ".env",
    ".env.bak",
    ".sesskey",
    "credentials.json",
    "credentials.yml",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    "client_secret.json",
    "service-account.json",
    ".netrc",
    "*.pem",
    "*.key",
];

fn name_hits_secret(name: &str) -> bool {
    let n = name.to_lowercase();
    if SECRET_FILES.iter().any(|p| {
        if p.starts_with('*') {
            n.ends_with(&p[1..])
        } else {
            n == *p
        }
    }) {
        return true;
    }
    // basename-only loose checks, and skip doc/code extensions to avoid path-noise
    // (e.g. .../openapi/.../count-tokens.yaml, okta-oidc-api-token.png)
    let base = n.rsplit('/').last().unwrap_or(&n);
    let doc_ext = [
        ".png", ".jpg", ".jpeg", ".svg", ".md", ".mdx", ".yaml", ".yml", ".go", ".ts", ".py",
        ".html", ".txt",
    ];
    let is_doc = doc_ext.iter().any(|e| base.ends_with(e));
    !is_doc && (base.starts_with(".env") || (base.contains("token") && base.contains("api")))
}

/// Light content heuristic: `KEY=value` where value looks secret (long,
/// high-entropy, or contains key-ish words). This is intentionally conservative
/// to avoid false positives on normal config.
fn content_looks_secret(line: &str) -> bool {
    let line = line.trim();
    if let Some((k, v)) = line.split_once('=') {
        let k = k.to_lowercase();
        let v = v.trim().trim_matches('"').trim_matches('\'');
        if k.contains("key")
            || k.contains("secret")
            || k.contains("password")
            || k.contains("token")
            || k.contains("credential")
        {
            // ignore obvious placeholders
            if v.len() >= 16
                && !v.starts_with("${")
                && !v.starts_with("<")
                && v != "changeme"
                && v != "example"
            {
                return true;
            }
        }
    }
    false
}

/// Scan one repo for tracked (committed) + untracked secret-looking files.
pub fn scan_repo(repo: &Path) -> Vec<Finding> {
    let mut out = Vec::new();
    let repo_name = repo
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string();

    // committed files
    if let Some(tracked) = git_ls(repo) {
        for f in tracked {
            if name_hits_secret(&f) {
                out.push(Finding {
                    repo: repo_name.clone(),
                    file: f.clone(),
                    reason: "tracked secret-like filename".into(),
                });
            }
        }
    }
    // untracked / modified (porcelain): lines like "?? path" or " M path"
    if let Some(porcelain) = git_status(repo) {
        for line in porcelain.lines() {
            let path = line.get(3..).unwrap_or("").trim();
            if path.is_empty() {
                continue;
            }
            if name_hits_secret(path) {
                out.push(Finding {
                    repo: repo_name.clone(),
                    file: path.into(),
                    reason: "untracked/modified secret-like filename".into(),
                });
            } else if let Ok(content) = std::fs::read_to_string(repo.join(path)) {
                if content.lines().any(content_looks_secret) {
                    out.push(Finding {
                        repo: repo_name.clone(),
                        file: path.into(),
                        reason: "content looks like a secret assignment".into(),
                    });
                }
            }
        }
    }
    out
}

fn git_ls(repo: &Path) -> Option<Vec<String>> {
    let o = Command::new("git")
        .current_dir(repo)
        .args(["ls-files"])
        .output()
        .ok()?;
    if !o.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|s| s.to_string())
            .collect(),
    )
}

fn git_status(repo: &Path) -> Option<String> {
    let o = Command::new("git")
        .current_dir(repo)
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    if !o.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&o.stdout).to_string())
}
