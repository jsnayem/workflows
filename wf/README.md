# wf — workflows TUI

Cross-project management tool (Ratatui). Lives in the `workflows` repo
(meta-repo for managing projects, not part of any single code project).

## Build
```
cargo build --release
```

## Run (TUI)
```
./target/debug/wf
```
Tabs: **Projects | Secrets | Hindsight**
- `←/→` or `h/l` — switch tab
- `↑/↓` — move selection
- `Enter` — action:
  - Projects: run `make check` (or `cargo check`) on the selected repo (in a worker thread; result pops up)
  - Secrets: show scan summary
  - Hindsight: **apply** the stale-memory sweep (PATCH-invalidates uncorrected stale world/experience memories)
- `r` — refresh all panels
- `q` — quit

## Headless (for cron / CI)
No TUI — prints to stdout:
```
wf --list        # repo | branch | ↑ahead/↓behind | clean/dirty | make?
wf --secrets     # repo | file | reason   (stderr: total count)
wf --hindsight   # bank totals + dry-run stale count + observations_mission
```

## What it does
- **Projects**: recursively discovers git repos under `~/Projects` (stops at the
  first `.git`, so it never descends into a repo's internals / `target`), and
  reports branch, ahead/behind vs upstream, dirty state, last commit, and whether
  a `Makefile`/`Cargo.toml` exists.
- **Secrets**: scans every repo for tracked + untracked secret-like files
  (`.env`, `.sesskey`, `*.key`, `*.pem`, `client_secret.json`, `id_*`,
  dotfile `*.env*`, and `KEY=/SECRET=/PASSWORD=/TOKEN=` assignments with
  long non-placeholder values). Read-only.
- **Hindsight**: reads the local `hermes` bank at `localhost:8888`; shows
  totals and a **dry-run** stale count. Apply (Enter) PATCH-invalidates stale
  world/experience memories using the correction-aware logic (it never deletes
  observations — those regenerate per the `observations_mission`).

## Scope / safety
- Read-only by default. The only mutating action is the Hindsight sweep
  (Enter on that tab), which invalidates — never deletes — memories.
- No network beyond localhost:8888 (Hindsight) and localhost git.
- The `workflows` repo's own `.gitignore` / secrets are respected.
