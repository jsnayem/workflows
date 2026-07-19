# wf — workflows TUI

Cross-project management tool (Ratatui). Lives in the `workflows` repo
(meta-repo for managing projects, not part of any single code project).

## Build
```
cargo build --release
```

## Run (TUI)
```bash
cargo build        # debug build (enables dev hot-reload)
./target/debug/wf
```
> Note: run the **debug** binary (`target/debug/wf`) for development. The
> auto-reload described below is compiled in only for `debug` builds.

### Dev hot-reload (debug builds only)
When you run `./target/debug/wf`, a background thread watches the crate
source (`Cargo.toml`, `Cargo.lock`, `src/*.rs`). On any save it rebuilds
with `cargo build` and — on your **next keypress** — re-execs the freshly
built binary in place (terminal-safe: it leaves raw mode / the alt screen
first, then `execve`s the new binary, so your TTY is never corrupted).
- No extra tooling needed (`cargo-watch`/`entr`/`inotifywait` not required).
- Press **`R`** for a manual hard restart: rebuild *now* and re-exec immediately.
- Set `WF_NO_WATCH=1 ./target/debug/wf` to disable the watcher.
- Release builds (`cargo build --release`) exclude all of this — zero overhead.

Tabs: **[1] Projects | [2] Secrets | [3] Hindsight | [4] Backup** (press the digit to jump)
- `←/→` or `h/l` — cycle tabs
- `↑/↓` — move selection
- `Enter` — action:
  - Projects: run `make check` (or `cargo check`) on the selected repo (in a worker thread; result pops up)
  - Secrets: show scan summary
  - Hindsight: **apply** the stale-memory sweep (PATCH-invalidates uncorrected stale world/experience memories)
  - Backup: **run** `backup.sh` (pulls remote `cs`/`ss` → `~/Projects/Backups`; output pops up)
- `r` — refresh all panels
- `R` — rebuild + restart (dev hot-reload)
- `q` — quit

### Live auto-refresh
Beyond the dev hot-reload above, the TUI also keeps its **data** fresh on
its own: a background worker rescans `~/Projects` (repo health, secrets,
Hindsight, backup dir) every ~2s and the UI picks it up within a tick —
so when you `git commit`/`push`/edit a repo, its dirty / ahead / behind
state updates in the panels **without closing the TUI** (no `r` needed).
The footer shows `autoscan HH:MM:SS` so you can see it ticking.

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
- **Backup**: launches the existing `backup.sh` SOP (rsync media + sqlite
  `VACUUM` dump from remote servers `cs`/`ss` into `~/Projects/Backups`).
  Shows a snapshot of the local backup dir; `Enter` runs the script and
  streams its output into a popup. The remote IP/SSH config stays in the
  (untracked) `backup.sh` — never hardcoded here.

## Scope / safety
- Read-only by default. Mutating actions are gated: Hindsight sweep needs a
  confirm (`Y`), and Backup runs the existing trusted `backup.sh` on demand.
- No network beyond localhost:8888 (Hindsight) and localhost git / SSH to the
  configured backup hosts.
- The `workflows` repo's own `.gitignore` / secrets are respected.
