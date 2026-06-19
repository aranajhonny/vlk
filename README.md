# Agora — Vlk MemAct

> *"If an AI cannot remember its own mistakes, it is not intelligent — it is amnesic."*

**Vlk MemAct** is a stateful memory layer for AI agents. It separates *what happened* (content) from *when it matters* (temporal state), so agents can learn from failure, project preventive rules forward, and stop repeating mistakes.

---

## The Problem

Current AI agents suffer from **context amnesia**:

| Symptom | Root Cause |
|---------|------------|
| Repeating the same error 5+ times | No failure memory |
| Context window bloat from stale logs | No archiving |
| Contradictory directives accumulate | No conflict detection |
| "Learned lessons" evaporate between sessions | No persistent projections |

The result: wasted tokens, frustrated users, and agents that cannot improve.

---

## Architecture

Two tables, three states.

```
┌─────────────────────────────────────────────────┐
│             memory_contents                      │
│  Immutable. Pure content. No temporal logic.     │
│  • raw_log        — the error / stacktrace       │
│  • file_context   — source file location         │
│  • tool_payload   — serialized tool call         │
└──────────────┬──────────────────────────────────┘
               │ content_id (FK)
┌──────────────▼──────────────────────────────────┐
│             agent_timeline                       │
│  Each slot = content + temporal state + position │
│                                                  │
│  ┌──────────┐   ┌──────┐   ┌──────────┐         │
│  │ PRESENT  │ → │ PAST │   │  FUTURE  │         │
│  │ active   │   │archvd│   │preventive│         │
│  │ context  │   │hidden│   │constraint│         │
│  └──────────┘   └──────┘   └──────────┘         │
│     injected      hidden      injected           │
│     to LLM        from LLM    as [CONSTRAINT]    │
└─────────────────────────────────────────────────┘
```

**Three temporal states:**

- **PRESENT** — What the agent processes *now*. Full content injected into the LLM.
- **PAST** — Dead ends. Already learned from. Hidden from context to save tokens.
- **FUTURE** — Preventive constraints extrapolated from experience, injected as `[PREVENTIVE FUTURE CONSTRAINT]` or `[PROSPECTIVE CONSTRAINT]` headers.

---

## Capabilities

### 1. State Recording (`vlk_record_state`)

Push errors, logs, or observations into the system as PRESENT timeline slots:

```
📝 [VLK RECORD] PRESENT state recorded as content_id=42 in session 'default'
```

Once recorded, the loop detector and time travel can operate on this data. Without this, the memory tables stay empty.

### 2. Time Travel (`vlk_time_travel`)

Archive PRESENT slots to PAST and inject a FUTURE constraint:

```
PRESENT slots (error loop)  ──►  PAST (archived, ~N tokens saved)
                                    │
                          FUTURE constraint injected:
                          "Never retry this endpoint without auth token"
```

**Requires evidence.** Every constraint must be grounded in a `raw_log_excerpt` — the original error that justifies the lesson. No unverified constraints.

### 3. Automatic Loop Interception

Scans PRESENT slots for repeated errors and **auto-mitigates without human intervention**:

- Fingerprint-based grouping catches near-duplicates (same error code, different timestamps/parameters)
- Recognizes errors from multiple languages and runtimes:
  - **Rust** — `error[E0277]`, `error[E0597]`
  - **TypeScript/JS** — `TS2345`, `TypeError`, `SyntaxError`, `ReferenceError`
  - **Python** — `ValueError`, `KeyError`, `ImportError`, `AttributeError`, etc.
  - **Go** — `panic: nil pointer dereference`, `fatal error: concurrent map writes`
  - **HTTP** — `503 Service Unavailable`, `429 Too Many Requests`, `Error 500`
  - **Test failures** — `expected: X, got: Y`, `AssertionError`, `assert_eq!`, `expect().toBe()`
  - **Fallback** — first 80 chars with timestamps stripped
- At ≥3 repetitions → auto-archives to PAST + injects `[SYSTEM ANCHOR]` constraint

```
[AUTO-INTERCEPT] Session 'default': mitigated 5 looped slots,
detection=fingerprint, ~1,200 tokens saved.
```

### 4. FUTURE Constraint Consolidation

When FUTURE entries exceed **5**, they auto-merge into a single consolidated constraint. To prevent unbounded growth, duplicates are removed and the string is capped at 2000 characters (~500 tokens), keeping the newest lessons.

```
[CONSOLIDATED CONSTRAINTS from 7 prior lessons]:
  Use cache | Never retry HTTP 429 | Avoid format! in hot path | ...
```

### 5. Conflict Detection

Lightweight, rule-based detection of contradictory directives. No ML required:

```
⚠ Constraint #12 vs #17: retry vs. never retry
⚠ Constraint #8 vs #23: exponential vs. linear backoff
```

### 6. Clean Context Fetching

`vlk_fetch_context` runs the full pipeline before returning context to the LLM:

```
Loop Interception → Consolidation → Conflict Detection → Clean Context
```

The LLM never sees repeated errors. Only PRESENT + sanitized FUTURE constraints.

---

## Protocol

Vlk MemAct is an **MCP (Model Context Protocol) server** operating over **JSON-RPC 2.0 via stdio**.

```
IDE / Agent Host  ──stdin──►  Vlk MCP Server  ──►  SQLite (WAL)
                  ◄─stdout──                    ◄──
```

**7 exposed tools:**

| Tool | Purpose |
|------|---------|
| `vlk_record_state` | Push a log/error as a PRESENT timeline slot |
| `vlk_time_travel` | PRESENT → PAST + FUTURE injection |
| `vlk_get_history` | Full timeline audit |
| `vlk_search_memory` | Keyword search across logs + constraints |
| `vlk_summarize_session` | State counts + token estimates |
| `vlk_fetch_context` | Clean active context (interceptor + consolidation pipeline) |
| `vlk_revoke_future` | Remove incorrectly learned constraints |

---

## Project Structure

```
vlk-core/
├── src/
│   ├── main.rs                    # JSON-RPC stdio loop, MCP tool definitions
│   └── memory/
│       ├── mod.rs                 # Module root
│       └── chronesthesia.rs       # Core temporal engine (~1,025 lines)
├── Cargo.toml                     # Rust 2021, tokio, sqlx, serde
├── benches/                       # Benchmarks (TODO)
└── tests/                         # Integration tests (TODO)
```

### Dependencies

| Crate | Why |
|-------|-----|
| `tokio` | Async I/O for stdio + DB |
| `sqlx` (sqlite) | Embedded DB, no external process |
| `serde` / `serde_json` | JSON-RPC serialization |
| `uuid` | Future session identity |
| `chrono` | Temporal ordering |
| `regex` | Timestamp stripping, fingerprint extraction |
| `tracing` | Structured logging to stderr |

### Database

SQLite with WAL journal mode. Two tables:

```sql
memory_contents
  id, raw_log, file_context, tool_payload

agent_timeline
  id, content_id(FK), session_id, sequence_order,
  temporal_state CHECK('PRESENT','PAST','FUTURE'),
  learning_summary, constraint_type CHECK('DERIVED','PROSPECTIVE'),
  created_at
```

Indexed on `(session_id, temporal_state, sequence_order)` for zero-overhead active context queries.

---

## Quick Start

```bash
# Build
cd vlk-core
cargo build --release

# Run (stdio MCP server)
DATABASE_URL="sqlite:vlk.db?mode=rwc" ./target/release/vlk-core

# Or with default path
./target/release/vlk-core
```

The server connects via stdio — your IDE or agent host sends JSON-RPC lines on stdin, reads responses on stdout. All logging goes to stderr.

---

## Current Status

| Area | Status |
|------|--------|
| Core temporal engine | ✅ Complete |
| MCP protocol | ✅ JSON-RPC 2.0, protocol 2024-11-05 |
| 7 MCP tools | ✅ All implemented |
| Loop detection (fingerprint-based) | ✅ 11+ error patterns across Rust, TS/JS, Python, Go, HTTP, test frameworks |
| FUTURE consolidation | ✅ Auto-merges at 5+ constraints, bounded at 2000 chars |
| Conflict detection | ✅ 10 keyword-pair heuristics |
| Constraint revocation | ✅ `vlk_revoke_future` |
| Automated tests | ❌ TODO — `tests/` directory exists, empty |
| Benchmarks | ❌ TODO — `benches/` directory exists, empty |
| UUID-based session identity | ⚠️ `uuid` crate imported, not yet integrated |
| CI/CD | ❌ Not configured |

---

## Design Philosophy

- **Separation of content and state.** Logs live in one table, temporal metadata in another. Decoupled by foreign key.
- **Evidence over speculation.** Every FUTURE constraint requires a `raw_log_excerpt`. No ungrounded lessons.
- **Automatic over manual.** Loop detection, consolidation, conflict detection — the system cleans itself.
- **Minimal over maximal.** 7 tools, 2 tables, 0 external services. Does one thing: temporal awareness.
- **Tokens are the unit of cost.** Every decision optimizes for token efficiency in the LLM context window.

---

*"The only mistake you can truly learn from is the one you remember. Everything else is just noise."*
