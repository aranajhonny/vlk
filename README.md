# Vlk — Memory-as-Action for IDE Agents

Dead context kills long-running agents. Vlk gives them a scalpel, not a sledgehammer — and doubles as persistent memory for your IDE.

## What it does

Vlk is a native [MCP](https://modelcontextprotocol.io) server implementing **Memory-as-Action** ([MemAct, Zhang et al. 2025](https://arxiv.org/abs/2510.12635)). It acts as persistent working memory for coding agents in Zed, Cursor, or Claude Desktop.

The agent accumulates context: tool outputs, reasoning traces, errors. When it detects a failure loop or context bloat, it calls `vlk_time_travel` — Vlk atomically prunes dead memory slots from SQLite and injects the lesson learned. No external controllers. No fixed heuristics.

Unlike ephemeral chat context, Vlk's SQLite-backed `agent_history` table **survives restarts**. Close your editor, reopen tomorrow — the agent picks up with lessons intact and dead ends gone.

```
[mem_id:5] London: API error 503
[mem_id:6] London: API error 503 (retry)
[mem_id:7] London: API error 503 (retry)

Agent calls → vlk_time_travel([5,6,7], "London API down, use cached 12°C")

Result: slots 5-7 deleted. Lesson persisted. Context clean. Agent unblocked.
```

## Tools

| Tool | What it does |
|---|---|
| `vlk_time_travel` | Prune dead slots by `[mem_id]` and inject a lesson. Atomic DELETE + INSERT. |
| `vlk_get_history` | Return memory records for a session (with optional `limit`). |
| `vlk_search_memory` | Full-text search across memory by keyword (`LIKE %query%`). |
| `vlk_summarize_session` | Condensed summary: total records, lesson count, latest mem_id, token usage. |

All tools accept an optional `session_id` (defaults to `"default"`), enabling multi-agent or multi-project isolation.

## Architecture

```
Zed / Cursor / Claude Desktop
  │  agent calls tools via stdio JSON-RPC
  ▼
vlk-core (Rust binary, single-file)
  │  tools/list  → 4 MCP tools exposed
  │  tools/call  → atomic SQLite operations
  ▼
SQLite (WAL mode)
  │  agent_history table
  │  UNIQUE INDEX on (session_id, mem_id)
  │  created_at, parent_mem_id, importance
```

## Schema

```sql
CREATE TABLE agent_history (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id       TEXT NOT NULL,
    mem_id           INTEGER NOT NULL,
    role             TEXT NOT NULL,
    content          TEXT NOT NULL,
    tokens_estimated INTEGER NOT NULL,
    tool_call_id     TEXT,
    tool_calls       TEXT,
    created_at       DATETIME DEFAULT CURRENT_TIMESTAMP,
    parent_mem_id    INTEGER,
    importance       REAL DEFAULT 1.0
);

CREATE UNIQUE INDEX idx_session_mem   ON agent_history(session_id, mem_id);
CREATE INDEX        idx_session_created ON agent_history(session_id, created_at DESC);
```

## Use cases

| Scenario | Agent behavior |
|---|---|
| **API retry storm** | Agent hits same endpoint 6× with 503. Detects loop, prunes failed attempts, injects "use fallback". Continues without wasting context. |
| **Contradictory tool output** | Two search calls return conflicting facts. Agent prunes the stale one, keeps the verified source, notes the resolution. |
| **Context window pressure** | Long-running task accumulating dead branches. Agent periodically prunes abandoned reasoning paths, reclaims tokens. |
| **Multi-turn task decomposition** | Agent explores 5 approaches, 4 dead-end. Prunes dead ends, keeps winning strategy + rationale for downstream steps. |
| **Self-correction** | Agent realizes early assumption was wrong. Prunes reasoning built on it, injects corrected premise, re-derives from clean state. |
| **IDE session persistence** | Close Zed, reopen tomorrow. Agent reads `agent_history`, sees pruned failures + injected lessons from yesterday. Doesn't repeat mistakes. |

## Run

```bash
git clone https://github.com/aranajhonny/vlk.git
cd vlk/vlk-core
cargo build --release
```

Your IDE's agent will use `vlk.db` (created automatically on first run) as persistent memory across sessions.

### Zed

```json
// .zed/settings.json
{
  "context_servers": {
    "vlk": {
      "command": "/absolute/path/to/vlk-core/target/release/vlk-core",
      "env": {
        "DATABASE_URL": "sqlite:/absolute/path/to/vlk-core/vlk.db?mode=rwc"
      }
    }
  }
}
```

### Cursor / Claude Desktop

Same pattern — point `command` at the binary. See [MCP docs](https://modelcontextprotocol.io/docs).

## Smoke test

```bash
echo '{"jsonrpc":"2.0","method":"tools/list","id":1}' | ./target/release/vlk-core
# → 4 tools listed: time_travel, get_history, search_memory, summarize_session

echo '{"jsonrpc":"2.0","method":"tools/call","params":{"name":"vlk_time_travel","arguments":{"target_mem_ids":[1],"learning":"test"}},"id":2}' | ./target/release/vlk-core
# → "1 slots pruned, N tokens saved. Lesson injected"
```

## Stack

Rust · Tokio · SQLx · SQLite WAL · JSON-RPC 2.0 · stdio transport · chrono · tracing

## Citation

```bibtex
@article{zhang2025memact,
  title  = {Memory as Action: Autonomous Context Curation for Long-Horizon Agentic Tasks},
  author = {Zhang, Yuxiang and Shu, Jiangming and Ma, Ye and Lin, Xueyuan and Wu, Shangxi and Sang, Jitao},
  year   = {2025},
  url    = {https://arxiv.org/abs/2510.12635}
}
```

## License

MIT
