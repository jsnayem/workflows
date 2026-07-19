# wf TUI — Progress Tracker

Cross-project management TUI (Ratatui, Rust). Tabs: Projects | Secrets | Hindsight | Backup.
Local repo: ~/Projects/workflows/wf. Run: `wf` (symlink ~/bin/wf -> target/release/wf),
or debug build `cd ~/Projects/workflows/wf && ./target/debug/wf`.

## Status legend
DONE · IN-PROGRESS · PLANNED · BLOCKED

## Features
| # | Feature | Status | Notes |
|---|---------|--------|-------|
| 1 | Projects panel (git health, ahead/behind/dirty, make check) | DONE | Enter runs make check in popup |
| 2 | Secrets panel (read-only scan) | DONE | |
| 3 | Hindsight panel — service status + start/stop | DONE | see "Hindsight panel" section |
| 4 | Backup panel (backup.sh runner) | DONE | |
| 5 | Live auto-refresh (~2s background rescan) | DONE | footer shows `autoscan HH:MM:SS` |
| 6 | Dev hot-reload (re-exec on source change) | DONE | debug builds only; WF_NO_WATCH=1 disables |
| 7 | Hindsight live statistics | DONE | graph size, documents, fact-type breakdown (see plan) |
| 8 | Hindsight stale-memory sweep (dry-run + apply) | DONE | apply gated behind Y confirm |

## Hindsight panel
Shows a live **running/stopped** status (polls `GET /health` every ~2s via the
background rescan), the API version, and live bank statistics. Controls:
- Service STOPPED -> `Enter` starts `hindsight-api` (detached, logs to
  `/tmp/hindsight-api.log`; spawns a thread, polls up to 45s for readiness).
- Service RUNNING -> `Enter` begins the stale-memory sweep (Y to confirm);
  `S` arms a stop, `Y` confirms (any other key cancels).
- Start/stop use the venv console script
  `/home/nayem/Projects/hindsight/.venv/bin/hindsight-api` (run from the
  hindsight dir so its `.env` is picked up). Stop kills only the process whose
  `/proc/<pid>/cmdline` contains that venv path, so the wf TUI itself is never
  matched. The embedded Postgres is a separate reparented daemon and is left
  running.

### Statistics — what is shown, where the data comes from, how it populates
All stats are read live from the local slim API at `http://127.0.0.1:8888`
(hermes bank). No data is cached in the TUI; each ~2s rescan re-fetches.

| Stat field | Source endpoint | Population |
|------------|----------------|-----------|
| running (bool) | `GET /health` | true if the API answers; drives the whole panel |
| version | `GET /version` -> `api_version` | string, e.g. `0.8.4` |
| total_memories | `GET /v1/default/banks/hermes/memories/list?limit=1` -> `total` | u64 node count |
| total_links | `GET /v1/default/banks/hermes/stats` -> `total_links` | u64 graph edge count |
| total_documents | `.../stats` -> `total_documents` | u64 ingested source docs |
| fact_world / fact_experience / fact_observation | `.../stats` -> `nodes_by_fact_type` | u64 per fact type |
| stale_candidates (dry-run) | computed client-side | counts list items matching stale-wrong patterns with NO correction signal (read-only; never mutates) |
| observations_mission | `GET /v1/default/banks/hermes/config` -> `config.observations_mission` | string mission text |

Code: `wf/src/hindsight.rs` (`BankInfo`, `info()`); render: `wf/src/main.rs`
`draw_hindsight()`. Verified by `tests::hindsight_panel_renders_status` (renders
RUNNING + stats into the buffer) and `wf --hindsight` headless mode.

## Verification
- `cargo test` — render regression tests (tab labels + Hindsight status).
- `cargo build --release` — produces the binary `~/bin/wf` points at.
- `wf --hindsight` — headless print of running/version/all stats.
- Real stop->start cycle exercised against the live service (self-restoring).
- IMPORTANT: `~/bin/wf` symlinks the **release** binary. After changing wf
  source, rebuild release (`cargo build --release`) or the TUI won't reflect
  edits. (This was the root cause of a "panel not updating" report on 2026-07-19.)

## Known gaps / next
- [ ] Add a per-fact-type "last retained/consolidated" timestamp if the API
      exposes it (not yet in /stats).
- [ ] Optional: a one-shot `wf hindsight restart` headless action.
- [ ] Decide whether to push local commits (currently ahead of origin/main).
