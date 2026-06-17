# Vlk — Memory-as-Action for LLM Agents

Dead context kills long-running agents. Vlk gives them a scalpel, not a sledgehammer — and doubles as persistent memory for your IDE's coding agent.

## What it does

Vlk is a native [MCP](https://modelcontextprotocol.io) server that acts as **persistent working memory for IDE coding agents**. It exposes a single tool — `vlk_time_travel` — that your agent calls when it detects a failure loop or context bloat. Vlk atomically prunes dead memory slots from SQLite and injects the lesson learned.

Unlike ephemeral chat context that vanishes between sessions, Vlk's SQLite-backed `agent_history` table survives restarts. Your Zed or Cursor agent picks up right where it left off — lessons intact, dead ends gone.

```
[mem_id:5] London: API error 503
[mem_id:6] London: API error 503 (retry)
[mem_id:7] London: API error 503 (retry)

Agent calls → vlk_time_travel([5,6,7], "London API down, use cached 12°C")

Result: slots 5-7 deleted. Lesson persisted. Context clean. Agent unblocked.
```

No external controllers, no fixed heuristics. The agent curates its own working memory at runtime. This is [MemAct (Zhang et al. 2025)](https://arxiv.org/abs/2510.12635).

## Architecture

```
Zed / Cursor / Claude Desktop
  │  agent calls vlk_time_travel via stdio JSON-RPC
  ▼
vlk-core (Rust) ← persistent memory layer for the IDE
  │  tools/list  → vlk_time_travel
  │  tools/call  → atomic DELETE + INSERT
  ▼
SQLite (WAL) — agent_history
```

## Use cases

| Scenario | Agent behavior |
|---|---|
| **API retry storm** | Agent hits same endpoint 6x with 503. Detects loop, prunes failed attempts, injects "use fallback". Continues without wasting context. |
| **Contradictory tool output** | Two search calls return conflicting facts. Agent prunes the stale one, keeps the verified source, notes the resolution. |
| **Context window pressure** | Long-running task accumulating dead branches. Agent periodically prunes abandoned reasoning paths, reclaims tokens. |
| **Multi-turn task decomposition** | Agent explores 5 approaches, 4 dead-end. Prunes dead ends, keeps winning strategy + rationale for downstream steps. |
| **Self-correction** | Agent realizes early assumption was wrong. Prunes reasoning built on it, injects corrected premise, re-derives from clean state. |
| **IDE session persistence** | You close Zed, reopen tomorrow. Agent reads `agent_history`, sees pruned failures + injected lessons from yesterday. Continues without repeating the same mistakes. |

## Run

```bash
cd vlk-core && cargo build --release
```

Your IDE's agent will use the SQLite database (`vlk.db`, created automatically on first run) as persistent memory across sessions.

### Zed

```json
// .zed/settings.json
{
  "context_servers": {
    "VlkMemAct": {
      "command": "/absolute/path/to/vlk-core/target/release/vlk-core"
    }
  }
}
```

### Cursor / Claude Desktop

Same pattern — point `command` at the binary. See [MCP docs](https://modelcontextprotocol.io/docs).

## Smoke test

```bash
echo '{"jsonrpc":"2.0","method":"tools/list","id":1}' | ./target/release/vlk-core
# → vlk_time_travel listed

echo '{"jsonrpc":"2.0","method":"tools/call","params":{"name":"vlk_time_travel","arguments":{"target_mem_ids":[1],"learning":"test"}},"id":2}' | ./target/release/vlk-core
# → "1 slots pruned, N tokens saved"
```

## Tool schema

```json
{
  "name": "vlk_time_travel",
  "inputSchema": {
    "properties": {
      "target_mem_ids": { "type": "array", "items": { "type": "integer" } },
      "learning": { "type": "string" }
    },
    "required": ["target_mem_ids", "learning"]
  }
}
```

## Citation

```bibtex
@article{zhang2025memact,
  title  = {Memory as Action: Autonomous Context Curation for Long-Horizon Agentic Tasks},
  author = {Zhang, Yuxiang and Shu, Jiangming and Ma, Ye and Lin, Xueyuan and Wu, Shangxi and Sang, Jitao},
  year   = {2025},
  url    = {https://arxiv.org/abs/2510.12635}
}
```
