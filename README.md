# Vlk — Memory-as-Action for LLM Agents

Dead context kills long-running agents. Vlk gives them a scalpel, not a sledgehammer.

## What it does

Vlk is a native [MCP](https://modelcontextprotocol.io) server exposing a single tool: `vlk_time_travel`. Your agent calls it when it detects a failure loop or context bloat — Vlk atomically prunes dead memory slots from SQLite and injects the lesson learned.

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
  │  stdio + JSON-RPC
  ▼
vlk-core (Rust)
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

## Run

```bash
cd vlk-core && cargo build --release
```

### Zed

```json
// .zed/settings.json
{
  "context_servers": {
    "VlkMemAct": {
      "command": "/Users/jhonny/lab/agora/vlk-core/target/release/vlk-core"
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
