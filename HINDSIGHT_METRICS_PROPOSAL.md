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

## TIER B — needs hindsight-repo backend work (DEFERRED / future)
Embeddings and reranker are NOT instrumented:
- `engine/embeddings.py` (ONNX provider, BAAI/bge-small) — emits no metrics.
- `engine/cross_encoder.py` (local reranker, ms-marco-MiniLM) — emits no metrics.

To surface them the hindsight repo needs (local-only, not committed into `wf`):
1. Add counters in `metrics.py`:
   - `hindsight.embeddings.calls.total` (labels: provider, model)
   - `hindsight.embeddings.tokens.total` (or units embedded)
   - `hindsight.reranker.calls.total` (labels: provider, model)
   - `hindsight.reranker.candidates.total` (candidates scored)
2. Emit them from the embedding/reranker call sites (wrap the inference).
3. Then the TUI reads them from `/metrics` exactly like TIER A stats.

Why deferred: crosses into the external hindsight service repo, which per the
established scope boundary is a separate local-only service — its hygiene/metrics
belong there, not folded into `wf`. Revisit when embedding/reranker cost visibility
becomes a real need (e.g. token-budget tuning).

## Verification
- `cargo test` regression guard `hindsight_panel_renders_status` covers the panel.
- Manual: run `wf`, tab 3, confirm Live Metrics block populates while the API is
  up and clears to "n/a (service down)" when stopped.
- Source of truth for available metrics: `curl -s localhost:8888/metrics`.
