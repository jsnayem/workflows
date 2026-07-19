# Hindsight Live Metrics — TUI Visibility Proposal

Status: TIER A implemented in the `wf` TUI. TIER B deferred (needs hindsight-repo
backend changes; local-only, out of `wf` scope per repo boundary).

## Problem
Previously, the only window into what the local hindsight-api was doing was its
stdout log (run via `hindsight-api` in a terminal). Once `wf` took over
start/stop, that log is hidden unless you `tail -f /tmp/hindsight-api.log`. The
user lost realtime visibility into LLM token usage, operation counts, and what
the memory engine was doing right now.

## Key finding (analysis of the running service)
The hindsight slim API already runs an OpenTelemetry → Prometheus exporter at
`http://127.0.0.1:8888/metrics`. Almost everything the old log showed is already
collected there — the TUI just never read it. The endpoint exposes counters and
histograms with rich labels (model, provider, scope, operation, success).

Verified live metric names (from the running instance, api v0.8.4):
- `hindsight_llm_calls_total`       (labels: model, provider, scope, success)
- `hindsight_llm_tokens_input_tokens_total`   (labels: model, provider, scope, token_bucket)
- `hindsight_llm_tokens_output_tokens_total`
- `hindsight_llm_duration_seconds`   (histogram, per scope)
- `hindsight_operation_operations_total`  (labels: operation = retain/recall/reflect/consolidation/graph_maintenance)
- `hindsight_operation_duration_seconds`    (histogram, per operation)
- `hindsight_http_requests_total` / `_duration_seconds`
- `hindsight_db_pool_size` / `_idle` / `_min` / `_max`
- `hindsight_process_cpu_seconds` / `_memory_bytes` / `_open_fds` / `_threads`
- `hindsight.consolidation.backlog` (pending/processing/failed) — EXISTS but
  gated behind config `metrics_backlog_enabled`; OFF in the user's instance.

LLM `scope` values observed: verification, retain_extract_facts, consolidation,
consolidation_dedup.

## Coverage matrix (requested stat → source)
| Requested | Available now | Source |
|-----------|---------------|--------|
| LLM model + token usage | YES | /metrics (llm_calls_total, llm_tokens_*) |
| LLM api calls | YES | /metrics (llm_calls_total) |
| Embedding model usage + calls | NO | not instrumented anywhere |
| Reranker model usage + calls | NO | not instrumented anywhere |
| Retain / Recall / Reflect / Consolidation | YES | /metrics (operation_operations_total) |
| Observations count | YES | /stats (nodes_by_fact_type.observation) |
| "What is it doing right now" / queue depth | PARTIAL | backlog gauge (off) + log only |

## TIER A — TUI reads existing /metrics (DONE — implemented 2026-07-19)
No backend changes. The `wf` Hindsight tab polls `GET /metrics` every ~2s (reusing
the existing rescan worker) and parses the Prometheus text format client-side,
then renders a "Live metrics (from /metrics)" block:

- LLM: total calls, total input tokens, total output tokens (and optionally
  broken down by `scope`).
- Operations: counts for retain / recall / reflect / consolidation (+ duration
  p50/p95 from the histogram `_seconds` sum/count if desired).
- Process: resident memory (MB), CPU seconds, open FDs, threads, DB pool size.
- Observations: from `/stats` (already wired).
- A "since last scrape" delta (rate) is computed in the TUI so the user sees
  *activity per interval*, not just cumulative totals.

Implementation notes:
- Prometheus text parse is trivial: lines `name{labels} value`, skip `#`,
  sum counters across matching label sets.
- Counters are cumulative-from-process-start; TUI keeps previous sample to show
  per-interval deltas (rate).
- All reads are read-only HTTP GETs; no writes to the hindsight service.

### TIER A open considerations (chose at build)
1. Presentation: added a "Live Metrics" sub-section under the existing Hindsight
   tab (keeps a single tab surface; no 5th tab).
2. Delta vs cumulative: show BOTH — cumulative total + a "(+Δ/scan)" rate.
3. Backlog gauge: optional; requires enabling `metrics_backlog_enabled` in the
   hindsight `.env` (one-line config, local-only). Left OFF by default; the TUI
   renders it if present.

## TIER B — local embedder & reranker visibility (NO hindsight-repo changes)
Constraint (user directive 2026-07-19): **do not modify the hindsight project.**
So we cannot add counters to `metrics.py` / `embeddings.py` / `cross_encoder.py`.
Instead, recover this data by parsing the log that the service already writes to
`/tmp/hindsight-api.log` (the path `wf` itself uses when it starts the service,
see `API_LOG` in `wf/src/hindsight.rs`). This is a pure-`wf` workaround: `wf`
tails/parses its own log file and extracts counters, showing per-scan deltas.

### Why this works (verified against the live log)
The reranker and embedder run locally (ONNX embedder `BAAI/bge-small-en-v1.5`,
local cross-encoder `ms-marco-MiniML-L-6-v2`) and emit NO `/metrics` counters —
but the recall/retain/consolidation paths DO log structured, parseable lines:

| Stat | Log line (regex-able) | Derived |
|------|----------------------|----------|
| Reranker calls | `  [4] Reranking [cross-encoder]: {N} candidates scored in {X}s` | calls, candidates, latency |
| Embed units (retain) | `STREAMING RETAIN COMPLETE: {U} units` / `DELTA RETAIN COMPLETE: {U} new units` | units embedded |
| Embed query (recall) | `  [1] Generate query embedding: {X}s` | query-embed calls + latency |
| Embed calls (consolidation) | `Timing breakdown: ... embedding={T}s ({C} calls, avg={A}ms)` | embed calls + time |
| Recall demand | `[RECALL ...] Complete: {F} facts (...)` | facts returned (proxy demand) |

So a `wf`-side parser can reconstruct:
- **Reranker**: total calls, total candidates scored, last/total latency.
- **Embedder**: total units embedded (from retain completions), total query
  embeds (from recall lines), total embed calls during consolidation, and
  aggregate embed latency. (Token counts are meaningless for a fixed-dim local
  ONNX model, so "units embedded" is the correct unit — consistent with the
  user's note that the embedder/reranker are local.)

### Implementation approach (all in `wf`, no hindsight touch)
1. In `wf/src/hindsight.rs` add `parse_activity_log(path) -> ActivityCounters`
   that reads `/tmp/hindsight-api.log` (last N KB is enough; the file is small
   and rotated/truncated on restart) and regex-counts the lines above.
2. Keep a previous-sample in `App` (like `prev_hindsight`) to show per-scan
   deltas, exactly like TIER A's LLM deltas.
3. When the service is started by `wf`, the log already goes to `API_LOG`. When
   the user starts it manually in a terminal, the log is wherever they put it —
   so make the path configurable (env `HINDSIGHT_API_LOG` defaulting to
   `/tmp/hindsight-api.log`), and show "log: n/a" if unreadable.
4. Render a "Local models (from log)" sub-block in the Hindsight panel:
   reranker calls/candidates/latency + embedder units/query-embeds/calls.

### Caveats (honest)
- Log-format coupled: if upstream changes the log strings, the parser needs a
  tweak. Mitigation: anchor on stable tokens (`Reranking [cross-encoder]:`,
  `RETAIN COMPLETE:`, `Generate query embedding:`).
- Only meaningful while the log is retained. A restart truncates counters to 0
  (handled by the delta saturating at 0, same as TIER A metrics reset).
- Reranker lines only appear on recall with `reranking=cross_encoder` (not on
  rrf/interleave passthrough) — that's correct, those paths don't run the model.
- This recovers ACTIVITY, not model-internal state. That is exactly what the old
  stdout log gave you, now rendered in the TUI.

This is strictly better than editing the hindsight repo: zero risk to the
service, zero scope breach, and it covers both embedder and reranker using data
that already exists.

## Verification
- `cargo test` regression guard `hindsight_panel_renders_status` covers the panel.
- Manual: run `wf`, tab 3, confirm Live Metrics block populates while the API is
  up and clears to "n/a (service down)" when stopped.
- Source of truth for available metrics: `curl -s localhost:8888/metrics`.
- Source of truth for local-model activity: `tail -f /tmp/hindsight-api.log`.
