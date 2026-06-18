# Vlk — Memory that actually helps your agent stop looping

**v0.5.0**

> Every coding agent I've used — Claude in Zed, Cursor, whatever — eventually hits the same wall: it sees the same error over and over, and instead of learning from it, the error *fills the context window* until the agent is functionally lobotomized. I built Vlk to fix this.

---

## The problem I kept running into

You're pair-programming with an agent. An API endpoint returns `503` five times in a row. The agent sees five identical errors in its context. It can't tell "this is the first 503" from "this is the fifth 503" — the attention mechanism just spreads evenly across the noise. The agent starts retrying the same thing, or hallucinating, or going in circles.

Every "memory" system I tried just dumped *more* data into the context. That's not memory — that's making the problem worse.

The core issue: **agents have no sense of subjective time**. They don't know what's "present" (happening now, needs attention), what's "past" (already learned from, should be archived), and what's "future" (a lesson to apply going forward). They treat everything as equally relevant.

---

## How Vlk solves it

Vlk gives your agent a sense of time. It splits every piece of context into one of three states:

| State | What it means | Shown to the agent? |
|-------|--------------|---------------------|
| **PRESENT** | Active — the agent is dealing with this now | ✅ Full `raw_log` |
| **PAST** | Archived — dead end the agent already learned from | ❌ Filtered out completely |
| **FUTURE** | A lesson extrapolated from past failure | ✅ As `[PREVENTIVE FUTURE CONSTRAINT]` |

When the agent hits a dead end (e.g., three identical `503` errors), you call `vlk_time_travel`. This:

1. Moves those errors from PRESENT → PAST (gone from context)
2. Injects a FUTURE constraint like: *"Use local cache — endpoint returned 503 five times"*

```
Before:  [PRESENT] Error 503: timeout  [PRESENT] Error 503: timeout  [PRESENT] Error 503: timeout
After:   [PAST]    Error 503: timeout  [PAST]    Error 503: timeout  [PAST]    Error 503: timeout
         [FUTURE]  Use local cache — endpoint is down
```

The agent never sees the raw errors again. It sees the lesson.

### Two kinds of FUTURE constraints

| Type | Origin | Example |
|------|--------|---------|
| **DERIVED** (default) | Scar tissue from a past failure | "Use local cache — endpoint returned 503 five times" |
| **PROSPECTIVE** | Actual future knowledge you provide | "Deploy scheduled at 3pm — expect instability" |

This distinction matters: DERIVED constraints are "I already broke this," while PROSPECTIVE constraints are "I know something about what's coming." Both live in the FUTURE timeline but come from completely different places.

---

## Architecture

```
                    ┌───────────────────────────┐
                    │    IDE (Zed / Cursor)      │
                    └─────────────┬─────────────┘
                                  │ stdio JSON-RPC
                                  ▼
                    ┌───────────────────────────┐
                    │      Vlk MCP Server       │
                    │   (Rust · sqlx · SQLite)  │
                    └──────┬─────────────┬──────┘
                           │             │
        ┌──────────────────┴──┐       ┌──┴──────────────────┐
        │  Content Store       │       │  Timeline            │
        │  (memory_contents)   │       │  (agent_timeline)    │
        ├─────────────────────┤       ├─────────────────────┤
        │ - raw_log           │       │ - temporal_state     │
        │ - file_context      │       │   PRESENT / PAST     │
        │ - tool_payload      │       │   / FUTURE           │
        └─────────────────────┘       │ - learning_summary   │
                                      │ - constraint_type    │
                                      │ - sequence_order     │
                                      │ - session_id         │
                                      └─────────────────────┘
```

The key insight: **content and time are separate tables**. The `memory_contents` table stores the raw data. The `agent_timeline` table decides *when* the agent should see it. This is how your own brain works — the hippocampus stores the memory, the parietal cortex decides whether it's "now" or "before." Vlk just does the same thing in SQLite.

---

## Tools

### `vlk_time_travel`

The main tool. Moves PRESENT slots to PAST and injects a FUTURE constraint.

```json
{
  "name": "vlk_time_travel",
  "arguments": {
    "target_timeline_ids": [5, 6, 7],
    "learning": "Use local cache — endpoint is down",
    "raw_log_excerpt": "POST /api/v2/agents → 503 Service Unavailable (5 consecutive attempts)",
    "constraint_type": "DERIVED"
  }
}
```

**`raw_log_excerpt` is required.** This grounds the lesson in actual evidence — you can't inject a lesson without showing the error that produced it. Prevents a confused agent from making things up.

**`constraint_type`** defaults to `"DERIVED"`. Set to `"PROSPECTIVE"` when you have genuine foresight (user-provided deadlines, known maintenance windows, etc.).

### `vlk_fetch_context`

Returns the **clean active context** (PRESENT + FUTURE only). PAST is filtered out.

This is what your IDE should call before every agent iteration. It also runs three automatic checks:

1. **Loop detection (Level 1):** Scans PRESENT slots and groups them by **semantic fingerprint** — not just exact string match. Catches near-duplicates (same error code with different timestamps, same compiler error on different line numbers).

2. **Constraint consolidation (Level 2):** When FUTURE constraints exceed 5 entries, they auto-merge into one. Prevents constraint accumulation from becoming its own form of noise.

3. **Conflict detection (Level 3):** Detects contradictory FUTURE constraints (e.g., "retry on 503" vs "never retry on 503") and surfaces them with ⚠ warnings.

### `vlk_get_history`

Full timeline for a session, including PAST entries. Use this to audit, debug, or find timeline IDs for `vlk_time_travel`.

### `vlk_search_memory`

Keyword search across raw_logs and learning_summaries. Finds both active and archived content.

### `vlk_summarize_session`

Counts of PRESENT / PAST / FUTURE slots + estimated token usage.

### `vlk_revoke_future`

Revoke a FUTURE constraint by moving it to PAST. Use when a constraint was wrong — e.g., the agent misdiagnosed an error and injected the wrong lesson.

```json
{
  "name": "vlk_revoke_future",
  "arguments": {
    "session_id": "default",
    "timeline_id": 42
  }
}
```

---

## Session Lifecycle

A **session** is identified by a `session_id` string. Default is `"default"`.

Sessions are created implicitly — just call a tool with a new `session_id` and the database starts tracking it.

**Tip:** One session per distinct task. Working on a Rust refactor? Use `"rust-refactor"`. Switched to a Python API? Use `"python-api"`. This keeps FUTURE constraints from one task from leaking into another.

**Sessions persist across restarts** (SQLite with WAL mode). This is a double-edged sword:

- ✅ Lessons learned yesterday still work today.
- ⚠️ Stale constraints can poison new tasks. If you start a completely different task under the same `session_id`, yesterday's constraints will still be active.

**Best practice:** Create a new `session_id` for each distinct coding session, or call `vlk_revoke_future` to clear old constraints. Or start fresh:

```bash
export DATABASE_URL="sqlite:/tmp/vlk-session-$(date +%s).db?mode=rwc"
```

---

## Benchmarks

> The real value isn't token savings — it's eliminating the noise that kills the LLM's reasoning. Token savings are a bonus.

All benchmarks are automated tests. Run them yourself:

```bash
cd vlk-core && cargo test bench_ -- --nocapture
```

**Token estimation:** Character count ÷ 3.8 (approximation for GPT-4 tokenizer). Good enough for comparing relative savings.

### Single error (real Rust compiler error, 629 chars)

```
╔═══ BENCH: Single Error Token Savings ═══╗
║ Raw log size:                629 chars   ║
║ Tokens per raw log:          166 tk      ║
║ Tokens saved (reported):     166 tk      ║
║ Context BEFORE (PRESENT):     208 tk     ║
║ Context AFTER (FUTURE):       53 tk      ║
║ Reduction:                   74%         ║
╚══════════════════════════════════════════╝
```

**74% context reduction** from a single `vlk_time_travel` call.

### Cumulative: 5 different real-world errors

The agent hits 5 distinct failures. Without Vlk all raw_logs pile up. With Vlk each error gets archived and only its lesson remains.

```
╔═══ BENCH: Cumulative Token Savings (5 errors) ═══╗
║                                                       ║
║ Without Vlk (all logs in context):                   ║
║   Total tokens accumulated:        409 tk             ║
║                                                       ║
║ With Vlk (logs → PAST + lessons → FUTURE):           ║
║   Tokens saved (reported):         408 tk             ║
║   Active context final:           155 tk              ║
║                                                       ║
║   Compression ratio:             2.6x                 ║
║   Total reduction:               62%                  ║
╚═══════════════════════════════════════════════════════╝
```

Active context after 5 errors (only FUTURE constraints, all raw_logs archived):

```
=== VLK CHRONESTHESIA LAYER ===
Active context: PRESENT (live) + FUTURE (constraints). PAST is archived.

[PREVENTIVE FUTURE CONSTRAINT]: Use format!() or push_str() to concatenate strings.
[PREVENTIVE FUTURE CONSTRAINT]: Implement retry with exponential backoff and circuit breaker.
[PREVENTIVE FUTURE CONSTRAINT]: Annotate variable types explicitly or use .parse() for conversions.
[PREVENTIVE FUTURE CONSTRAINT]: Respect rate limit headers. Add local rate limiter before sending requests.
[PREVENTIVE FUTURE CONSTRAINT]: Verify module paths. Run `cargo check` after adding new modules.
```

### Bounded growth: 10 iterations of the same error

This is the key benchmark. The exact scenario that kills every agent: the same API error 10 times in a row.

| Iteration | Without Vlk (linear) | With Vlk (bounded) | Ratio |
|-----------|---------------------|---------------------|-------|
| 1 | 96 tk | 50 tk | 1.9x |
| 2 | 192 tk | 72 tk | 2.7x |
| 3 | 288 tk | 94 tk | 3.1x |
| 4 | 384 tk | 116 tk | 3.3x |
| 5 | 480 tk | 138 tk | 3.5x |
| 6 | 576 tk | 159 tk | 3.6x |
| 7 | 672 tk | 181 tk | 3.7x |
| 8 | 768 tk | 203 tk | 3.8x |
| 9 | 864 tk | 225 tk | 3.8x |
| 10 | 960 tk | 247 tk | **3.9x** |

**Without Vlk:** context grows linearly — O(n). Every iteration adds the full raw_log. After 10 iterations: 960 tokens of identical noise.

**With Vlk:** context grows sub-linearly. Raw_logs get archived to PAST. Only FUTURE constraints accumulate. After 10 iterations: 247 tokens of distilled lessons instead of 960 tokens of noise.

**But sub-linear is still unbounded.** Vlk v0.5 adds automatic consolidation: when FUTURE constraints exceed 5, they merge into one (see Level 2 above).

---

## Data Flow

When a real error loop hits, the system acts before the LLM ever sees the noise:

```
[IDE Terminal / Compiler Output]
              │
              │  (3x same error pattern — fingerprint match)
              ▼
 ┌─────────────────────────────────────┐
 │ Vlk Interceptor (Rust Backend)      │ ──► Moves 3 noisy logs to PAST
 │ auto_detect_and_mitigate_loops()    │     inside a single SQLite txn
 │ (fingerprint + exact match)         │
 └─────────────────┬───────────────────┘
                   │
                   │  (atomic commit)
                   ▼
 ┌─────────────────────────────────────┐
 │  Timeline (SQLite)                │ ──► Creates FUTURE constraint
 │  agent_timeline table              │     with [SYSTEM ANCHOR] directive
 └─────────────────┬───────────────────┘
                   │
                   │  (fetch_clean_context)
                   ▼
 ┌─────────────────────────────────────┐
 │  Context Injected to LLM          │ ──► Sees only the system directive
 │  [PREVENTIVE FUTURE CONSTRAINT]    │     Raw logs are invisible
 └─────────────────────────────────────┘
```

This isn't prompting the LLM to behave differently. It's **changing the environment** so the LLM *can't* loop on the same failure twice.

---

## Automatic Loop Interceptor (Level 1)

Vlk's `fetch_clean_context` runs detection **before** returning context to the agent:

1. Scan all PRESENT timeline slots for the session
2. Compute a **semantic fingerprint** for each `raw_log`:
   - Extract Rust error codes (`error[E0277]`)
   - Extract HTTP status + message prefix (`503 Service Unavailable`)
   - Strip timestamps (`14:32:01` → `[TIME]`)
   - Fall back to first 80 normalized characters
3. Group by fingerprint first, then by exact match (preferring exact when available)
4. If any group ≥ 3 occurrences → auto-execute `vlk_time_travel`
5. Inject `[SYSTEM ANCHOR]` with imperative instruction
6. Return clean context — the LLM never sees the repeated errors

**What this catches that exact match alone misses:**

| Pattern | Exact match | Fingerprint |
|---------|-------------|-------------|
| `Error 503: timeout` × 3 | ✅ | ✅ |
| `Error 503: timeout at 14:32:01` × 3 | ❌ | ✅ |
| `error[E0277]: String: From<usize>` × 3 | ✅ | ✅ |
| `error[E0277]: String: From<i32>` × 3 | ❌ | ✅ |

This runs server-side, in Rust, inside the SQLite transaction. The LLM can't opt out — it simply receives a FUTURE constraint instead of three identical errors.

---

## Quickstart

### Build

```bash
cd vlk-core && cargo build --release
```

### Run (stdio MCP server)

The binary reads JSON-RPC from stdin and writes to stdout. SQLite database (`vlk.db`) is created automatically.

```bash
# Set database path (optional — defaults to ./vlk.db)
export DATABASE_URL="sqlite:/path/to/vlk.db?mode=rwc"

# Start server
./target/release/vlk-core
```

### Integrate with Zed

```json
// .zed/settings.json
{
  "context_servers": {
    "vlk": {
      "command": "/path/to/vlk-core",
      "args": [],
      "env": {
        "DATABASE_URL": "sqlite:/path/to/vlk.db?mode=rwc"
      }
    }
  }
}
```

### Integrate with Cursor / Claude Desktop

Same pattern — point `command` at the binary. See [MCP documentation](https://modelcontextprotocol.io/docs).

---

## Smoke Test

```bash
# List available tools
echo '{"jsonrpc":"2.0","method":"tools/list","id":1}' | ./target/release/vlk-core

# Fetch clean context for default session
echo '{"jsonrpc":"2.0","method":"tools/call","params":{"name":"vlk_fetch_context","arguments":{}},"id":2}' | ./target/release/vlk-core
```

Expected output:
```
=== VLK CHRONESTHESIA LAYER ===
Active context: PRESENT (live) + FUTURE (constraints). PAST is archived.

(No active PRESENT or FUTURE entries for this session.)
```

---

## How to Use with an Agent

1. **Before each agent action**, call `vlk_fetch_context` to get clean context
2. **Agent works normally** — runs tools, sees errors, iterates
3. **When the agent detects a loop or dead end**, call `vlk_time_travel`:
   - Pass the timeline IDs of the dead-end slots
   - Write a concise lesson as `learning`
   - **Required:** provide a `raw_log_excerpt` — the original error that grounds the lesson
4. **On the next iteration**, `vlk_fetch_context` returns only the FUTURE constraint — the dead logs are gone
5. **If a constraint was wrong**, call `vlk_revoke_future` with the timeline ID to remove it

Additionally, **Vlk auto-mitigates loops of 3+ identical or near-identical errors** without waiting for the agent.

### Prompt template for the agent

```
You have access to persistent memory via the Vlk MCP server.

AVAILABLE TOOLS:
- vlk_time_travel(session_id, target_timeline_ids, learning, raw_log_excerpt, constraint_type):
    Archive dead-end PRESENT slots to PAST. Inject a FUTURE constraint.
    Call this when you detect a failure loop or want to consolidate context.
    raw_log_excerpt is REQUIRED — provide 1-2 sentences of the original error
    as evidence. constraint_type defaults to "DERIVED".

- vlk_fetch_context(session_id):
    Get clean active context (PRESENT + FUTURE only).
    Call this before every action. Built-in: loop detection, constraint
    consolidation, and conflict detection.

- vlk_get_history(session_id, limit):
    View full timeline including PAST entries and constraint types.

- vlk_search_memory(session_id, query, limit):
    Search across all memory contents.

- vlk_summarize_session(session_id):
    Get PRESENT/PAST/FUTURE counts and token estimates.

- vlk_revoke_future(session_id, timeline_id):
    Revoke a FUTURE constraint (move to PAST). Use when a constraint
    was incorrectly learned.
```

---

## Schema

```sql
-- Content store: immutable raw data
CREATE TABLE memory_contents (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    raw_log     TEXT NOT NULL,
    file_context TEXT,
    tool_payload TEXT
);

-- Timeline: what the agent sees and when
CREATE TABLE agent_timeline (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    content_id      INTEGER REFERENCES memory_contents(id),
    session_id      TEXT NOT NULL,
    sequence_order  INTEGER NOT NULL,
    temporal_state  TEXT CHECK(temporal_state IN ('PRESENT','PAST','FUTURE')),
    learning_summary TEXT,
    constraint_type TEXT CHECK(constraint_type IN ('DERIVED','PROSPECTIVE')),
    created_at      DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- Optimized for fetch_active_context queries
CREATE INDEX idx_timeline_active_context
    ON agent_timeline(session_id, temporal_state, sequence_order);
```

---

## Test Suite

```bash
cd vlk-core && cargo test
```

36 tests covering:

- Schema creation, CHECK constraints, FOREIGN KEY integrity
- PRESENT → PAST transitions
- FUTURE constraint injection with evidence validation
- Constraint type handling (DERIVED / PROSPECTIVE)
- Context filtering (PAST excluded, FUTURE shown)
- Session isolation
- Automatic loop detection and mitigation (exact + fingerprint)
- Constraint consolidation (5+ entries → merged)
- Conflict detection for contradictory FUTURE constraints
- FUTURE constraint revocation
- Token estimation accuracy
- Cumulative savings benchmarks
- Bounded growth over 10 iterations
- Multi-group frequency selection
- Whitespace normalization
- Empty session handling

---

## Why it works — the principle behind it

Vlk is inspired by a finding from neuroscience: the hippocampus stores *what* happened, but the parietal cortex decides *when* it happened — whether it's "now," "before," or "about to happen." People with parietal damage can remember events but can't place them in time. They're stuck in the present.

That's exactly the problem with LLM agents. They can recall errors (the context window has them), but they can't distinguish "this error is happening now" from "this error already happened and I should move on." Vlk gives agents that parietal function — a timeline that separates present from past from future — at the database level.

| Brain | Vlk | What it does |
|-------|-----|--------------|
| Hippocampus | `memory_contents` | Stores raw content (logs, errors, payloads) |
| Parietal cortex | `agent_timeline` | Manages subjective time (PRESENT/PAST/FUTURE states) |
| Prefrontal integration | `fetch_clean_context` | Assembles only the relevant context for action |
| Subcortical reflex | `auto_detect_and_mitigate_loops` | Proactive loop detection (happens before you even ask) |
| Garbage collection | `consolidate_future_constraints` | Merges accumulated constraints when there are too many |

Reference: Nyberg, L., & Tulving, E. (2010). *Consciousness of subjective time in the brain.* PNAS, 107(51), 21773–21774.

---

## What makes this production-grade

| Property | How Vlk guarantees it |
|----------|----------------------|
| **Atomic state transitions** | Every `execute_time_travel` runs inside a SQLite transaction. PRESENT→PAST update + FUTURE insert either both commit or neither. |
| **Blind backend heuristic** | Loop detection runs in Rust before the LLM sees context. The agent can't opt out — the raw logs are already in PAST. |
| **Fingerprint-based detection** | Catches near-duplicates (same error, different timestamps/params) — not just exact matches. |
| **Constraint quality gate** | Agent-initiated `vlk_time_travel` requires `raw_log_excerpt` as evidence. Confused agents can't inject unverified lessons. |
| **Conflict detection** | Keyword-based heuristics flag contradictory FUTURE constraint pairs before the agent acts on them. |
| **Bounded constraint growth** | FUTURE constraints auto-consolidate when exceeding 5 entries. Sub-linear growth is guaranteed even in long sessions. |
| **Imperative frontend prompt** | The `[SYSTEM ANCHOR]` directive is formatted as a system-level constraint. Modern LLMs prioritize these over their own execution history. |
| **Session isolation** | Each session_id is a separate timeline. One agent's loops never leak into another's context. |
| **Survives restarts** | SQLite with WAL mode persists across IDE restarts. Yesterday's lessons still work today. **Use distinct session_ids per task to avoid stale constraint poisoning.** |
| **Model-independent** | Plain JSON-RPC over stdio. Works with any LLM that supports MCP tools. |

---

## Reproduce the benchmarks

```bash
cd vlk-core && cargo test bench_ -- --nocapture
```

The key number: **3.9x compression ratio at 10 iterations**, with guaranteed sub-linear growth and automatic consolidation preventing unbounded accumulation. Every agent framework I've tried — LangChain, CrewAI, AutoGPT, you name it — suffers from context window saturation. Vlk eliminates it at the database level.

---

## License

MIT