# 🧠 Agora — Vlk MemAct

> *"If an AI cannot remember its own mistakes, it is not intelligent — it is amnesic. Chronesthesia is the substrate of learning."*

**Vlk MemAct** is a neuro-inspired memory layer for AI agents that implements **chronesthesia** — the brain's ability to mentally travel through subjective time. It separates *what happened* (content) from *when it matters* (temporal state), allowing agents to learn from failure, project preventive constraints forward, and stop repeating the same mistakes.

---

## ⚡ The Problem

Current AI agents suffer from **context amnesia**:

| Symptom | Root Cause |
|---------|------------|
| Repeating the same error 5+ times | No failure memory |
| Context window bloat from stale logs | No PAST archiving |
| Contradictory directives accumulate | No conflict detection |
| "Learned lessons" evaporate between sessions | No FUTURE projection |

The result: wasted tokens, frustrated users, and agents that look intelligent but behave like they have anterograde amnesia.

---

## 🧬 First Principles

The architecture is based on **Nyberg & Tulving (2010)** — *"Consciousness of subjective time in the brain"* (PNAS). Their key finding: the brain does **not** use the hippocampus (content memory) for mental time travel. It uses a **separate network in the left lateral parietal cortex**.

We emulate this separation:

```
┌─────────────────────────────────────────────────┐
│           HIPPOCAMPAL LAYER                      │
│           (memory_contents)                      │
│  Immutable. Pure content. No time awareness.     │
│  • raw_log        — the error / stacktrace       │
│  • file_context   — source file location         │
│  • tool_payload   — serialized tool call         │
└──────────────┬──────────────────────────────────┘
               │ content_id (FK)
┌──────────────▼──────────────────────────────────┐
│           PARIETAL LAYER                        │
│           (agent_timeline)                      │
│  Temporal awareness. Subjective chronology.     │
│                                                 │
│  ┌──────────┐   ┌──────┐   ┌──────────┐         │
│  │ PRESENT  │ → │ PAST │   │  FUTURE  │         │
│  │ active   │   │archvd│   │preventive│         │
│  │ context  │   │hidden│   │constraint│         │
│  └──────────┘   └──────┘   └──────────┘         │
│     injected      hidden      injected           │
│     to LLM        from LLM    as [CONSTRAINT]   │
└─────────────────────────────────────────────────┘
```

**Three temporal states.** No more, no less:

- **PRESENT** — What the agent processes *now*. Full content injected into the LLM.
- **PAST** — Dead ends. Already learned from. Hidden from context to save tokens.
- **FUTURE** — Preventive constraints extrapolated from experience. Injected as `[PREVENTIVE FUTURE CONSTRAINT]` or `[PROSPECTIVE CONSTRAINT]` headers.

---

## 🔧 Capabilities

### 1. Mental Time Travel (`vlk_time_travel`)

The computational equivalent of closing a stuck present and projecting forward:

```
PRESENT slots (error loop)  ──►  PAST (archived, ~N tokens saved)
                                    │
                          FUTURE constraint injected:
                          "Never retry this endpoint without auth token"
```

**Requires evidence.** Every constraint must be grounded in a `raw_log_excerpt` — the original error that justifies the lesson. No unverified constraints.

### 2. Automatic Loop Interception

The system **detects and mitigates error loops without human intervention**:

- Scans PRESENT slots for repeated errors
- **Fingerprint-based grouping** catches near-duplicates (same error code, different timestamps)
- Rust error code extraction: `error[E0277]` → `error[E0597]`
- HTTP status extraction: `503 Service Unavailable` → `429 Too Many Requests`
- Timestamp normalization for generic logs
- At ≥3 repetitions → auto-archives to PAST + injects `[SYSTEM ANCHOR]` constraint

```
[AUTO-INTERCEPT] Session 'default': mitigated 5 looped slots,
detection=fingerprint, ~1,200 tokens saved.
```

### 3. FUTURE Constraint Consolidation

Constraints are learning — but too many become noise. When FUTURE entries exceed **5**, they auto-merge into a single consolidated constraint:

```
[CONSOLIDATED CONSTRAINTS from 7 prior lessons]:
  Use cache | Never retry HTTP 429 | Avoid format! in hot path | ...
```

### 4. Conflict Detection

Lightweight, rule-based detection of contradictory directives. No ML required — just targeted heuristics:

```
⚠ Constraint #12 vs #17: retry vs. never retry
⚠ Constraint #8 vs #23: exponential vs. linear backoff
```

Detected pairs are surfaced so the agent (or user) can revoke the wrong one with `vlk_revoke_future`.

### 5. Clean Context Fetching

`vlk_fetch_context` runs the full pipeline before returning context to the LLM:

```
Loop Interception → Consolidation → Conflict Detection → Clean Context
```

The LLM never sees repeated errors. It only sees PRESENT + sanitized FUTURE constraints.

---

## 📡 Protocol

Vlk MemAct is an **MCP (Model Context Protocol) server** operating over **JSON-RPC 2.0 via stdio**.

```
IDE / Agent Host  ──stdin──►  Vlk MCP Server  ──►  SQLite (WAL)
                  ◄─stdout──                    ◄──
```

**6 exposed tools:**

| Tool | Purpose |
|------|---------|
| `vlk_time_travel` | PRESENT → PAST + FUTURE injection |
| `vlk_get_history` | Full timeline audit |
| `vlk_search_memory` | Keyword search across logs + constraints |
| `vlk_summarize_session` | State counts + token estimates |
| `vlk_fetch_context` | Clean active context (interceptor pipeline) |
| `vlk_revoke_future` | Remove incorrectly learned constraints |

---

## 🏗️ Architecture

```
vlk-core/
├── src/
│   ├── main.rs                    # JSON-RPC stdio loop, MCP tool definitions
│   └── memory/
│       ├── mod.rs                 # Module root
│       └── chronesthesia.rs       # Core temporal engine (1,025 lines)
├── Cargo.toml                     # Rust 2021, tokio, sqlx, serde
├── benches/                       # Benchmarks (TODO)
└── tests/                         # Integration tests (TODO)
```

### Dependencies (zero bloat)

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
memory_contents (hippocampal)
  id, raw_log, file_context, tool_payload

agent_timeline (parietal)
  id, content_id(FK), session_id, sequence_order,
  temporal_state CHECK('PRESENT','PAST','FUTURE'),
  learning_summary, constraint_type CHECK('DERIVED','PROSPECTIVE'),
  created_at
```

Indexed on `(session_id, temporal_state, sequence_order)` for zero-overhead active context queries.

---

## 🚀 Quick Start

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

## 📊 Current Status

| Area | Status |
|------|--------|
| Core chronesthesia engine | ✅ Complete (v0.4.0-chronesthesia) |
| MCP protocol | ✅ JSON-RPC 2.0, protocol 2024-11-05 |
| 6 MCP tools | ✅ All implemented |
| Loop detection (fingerprint-based) | ✅ 3 detection strategies |
| FUTURE consolidation | ✅ Auto-merges at 5+ constraints |
| Conflict detection | ✅ 10 keyword-pair heuristics |
| Constraint revocation | ✅ `vlk_revoke_future` |
| Legacy `agent_history` table | ⚠️ Deprecated, kept for compatibility |
| Automated tests | ❌ TODO — `tests/` directory exists, empty |
| Benchmarks | ❌ TODO — `benches/` directory exists, empty |
| UUID-based session identity | ⚠️ `uuid` crate imported, not yet integrated |
| CI/CD | ❌ Not configured |

---

## 🔮 Roadmap

1. **Test suite** — Unit tests for fingerprint extraction, conflict detection, consolidation logic
2. **Benchmarks** — Measure context fetch latency, loop detection throughput
3. **Persistent session identity** — Integrate `uuid` for cross-restart session continuity
4. **Semantic conflict detection** — Replace keyword heuristics with embedding-based similarity for contradiction detection
5. **Multi-agent support** — Shared FUTURE constraints across agent sessions
6. **Constraint decay** — FUTURE constraints that expire after N successful iterations without re-triggering

---

## 🧪 Scientific Foundation

> Nyberg, L., & Tulving, E. (2010). *Consciousness of subjective time in the brain.*  
> Proceedings of the National Academy of Sciences, 107(51), 21773–21774.  
> https://doi.org/10.1073/pnas.1016823108

The key insight applied here: **consciousness of time is not a byproduct of memory — it is a separate cognitive function**. An agent that stores everything in one undifferentiated log is like a brain with no parietal cortex. It has content but no temporal awareness. Vlk gives agents that awareness.

---

## ⚙️ Design Philosophy

- **First principles over pattern matching.** The brain's separation of content and time is the architecture. Not "best practices."
- **Evidence over speculation.** Every FUTURE constraint requires a `raw_log_excerpt`. No ungrounded lessons.
- **Automatic over manual.** Loop detection, consolidation, conflict detection — the system cleans itself.
- **Minimal over maximal.** 6 tools, 2 tables, 0 external services. Does one thing: temporal awareness.
- **Tokens are the unit of cost.** Every decision optimizes for token efficiency in the LLM context window.

---

*"The only mistake you can truly learn from is the one you remember. Everything else is just noise."*
