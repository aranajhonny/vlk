use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{info, warn};

mod memory;

use memory::chronesthesia::{
    self, execute_time_travel, fetch_clean_context, get_session_summary, get_timeline,
    revoke_future_constraint, search_timeline, TimeTravelArgs,
};

// ── JSON-RPC Types ───────────────────────────────────────────────────────────
#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Option<serde_json::Value>,
    method: String,
    #[serde(default)]
    params: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

// ── Helpers ──────────────────────────────────────────────────────────────────
fn get_session_id(args: &serde_json::Value) -> String {
    args["session_id"]
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "default".to_string())
}

// ── Database Setup ───────────────────────────────────────────────────────────
async fn init_db(pool: &SqlitePool) -> Result<()> {
    // Legacy table — kept for backward compatibility, no longer used by tools
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS agent_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            mem_id INTEGER NOT NULL,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            tokens_estimated INTEGER NOT NULL,
            tool_call_id TEXT,
            tool_calls TEXT,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            parent_mem_id INTEGER,
            importance REAL DEFAULT 1.0
        );

        CREATE UNIQUE INDEX IF NOT EXISTS idx_session_mem
            ON agent_history(session_id, mem_id);
        "#,
    )
    .execute(pool)
    .await?;

    // New neuro-inspired tables
    chronesthesia::init_chronesthesia_tables(pool).await?;

    info!("Database initialized: agent_history (legacy) + memory_contents + agent_timeline");
    Ok(())
}

// ── MCP Tool Definitions ─────────────────────────────────────────────────────
fn tool_definitions() -> serde_json::Value {
    serde_json::json!({
        "tools": [
            {
                "name": "vlk_time_travel",
                "description": "Chronesthetic memory management. Transitions PRESENT timeline slots to PAST and injects a FUTURE constraint. Use when stuck in a failure loop or context is cluttered. Check vlk_get_history for [timeline:N] IDs. Requires raw_log_excerpt as evidence — the original error/log that grounds this lesson.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": { "type": "string", "description": "Optional. Defaults to 'default'." },
                        "target_timeline_ids": {
                            "type": "array",
                            "items": { "type": "integer" },
                            "description": "Timeline slot IDs (PRESENT) to transition to PAST. Listed in vlk_get_history output."
                        },
                        "learning": {
                            "type": "string",
                            "description": "Lesson learned — injected as a FUTURE constraint in place of the archived slots."
                        },
                        "raw_log_excerpt": {
                            "type": "string",
                            "description": "REQUIRED. 1-2 sentences of the original error/log that grounds this lesson in evidence. Prevents unverified constraints."
                        },
                        "constraint_type": {
                            "type": "string",
                            "enum": ["DERIVED", "PROSPECTIVE"],
                            "description": "Origin of the constraint. DERIVED (default): scar tissue from failure. PROSPECTIVE: genuine foresight (deadlines, maintenance windows)."
                        }
                    },
                    "required": ["target_timeline_ids", "learning", "raw_log_excerpt"]
                }
            },
            {
                "name": "vlk_get_history",
                "description": "Returns the full timeline for a session including temporal state (PRESENT/PAST/FUTURE), content excerpts, and timeline IDs. Use to audit context or find IDs for vlk_time_travel.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": { "type": "string", "description": "Optional. Defaults to 'default'." },
                        "limit": { "type": "integer", "default": 50, "description": "Max records to return." }
                    }
                }
            },
            {
                "name": "vlk_search_memory",
                "description": "Search timeline by keyword across raw logs and learning summaries. Useful for retrieving specific facts, errors, or past decisions.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": { "type": "string", "description": "Optional. Defaults to 'default'." },
                        "query": { "type": "string", "description": "Keyword or phrase to search for." },
                        "limit": { "type": "integer", "default": 20, "description": "Max results." }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "vlk_summarize_session",
                "description": "Returns a condensed summary of session memory with counts of PRESENT (active), PAST (archived), and FUTURE (constraint) timeline slots.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": { "type": "string", "description": "Optional. Defaults to 'default'." }
                    }
                }
            },
            {
                "name": "vlk_fetch_context",
                "description": "Returns the clean active context (PRESENT + FUTURE only). Automatically detects and mitigates error loops: if the same error repeats 3+ times, it's archived as PAST and a SYSTEM ANCHOR constraint is injected. Also consolidates FUTURE constraints when they exceed 5+ entries and detects conflicting directives. Call this before each agent iteration.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": { "type": "string", "description": "Optional. Defaults to 'default'." }
                    }
                }
            },
            {
                "name": "vlk_revoke_future",
                "description": "Revoke a FUTURE constraint by moving it to PAST. Use this when a constraint was incorrectly learned or is no longer applicable. Find the timeline ID via vlk_get_history.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": { "type": "string", "description": "Optional. Defaults to 'default'." },
                        "timeline_id": { "type": "integer", "description": "The timeline slot ID (FUTURE state) to revoke." }
                    },
                    "required": ["timeline_id"]
                }
            }
        ]
    })
}

// ── MCP Request Handler ─────────────────────────────────────────────────────
struct AppState {
    db: SqlitePool,
}

impl AppState {
    async fn handle(&self, req: JsonRpcRequest) -> JsonRpcResponse {
        let id = req.id.clone();

        let ok = |result: serde_json::Value| JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: id.clone(),
            result: Some(result),
            error: None,
        };

        let err = |msg: String| JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: id.clone(),
            result: None,
            error: Some(JsonRpcError {
                code: -32603,
                message: msg,
            }),
        };

        match req.method.as_str() {
            "initialize" => ok(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "Vlk MemAct", "version": "0.4.0-chronesthesia" }
            })),

            "tools/list" => ok(tool_definitions()),

            "tools/call" => {
                let params = req.params.unwrap_or_default();
                let name = params["name"].as_str().unwrap_or_default();
                let args = &params["arguments"];

                match name {
                    // ── vlk_time_travel: PRESENT → PAST + FUTURE injection ──
                    "vlk_time_travel" => {
                        let tool_args: TimeTravelArgs = serde_json::from_value(args.clone())
                            .unwrap_or_else(|_| TimeTravelArgs {
                                session_id: None,
                                target_timeline_ids: vec![],
                                learning: String::new(),
                                raw_log_excerpt: String::new(),
                                constraint_type: None,
                            });

                        match execute_time_travel(&self.db, tool_args).await {
                            Ok((tokens_saved, learning)) => {
                                let text = format!(
                                    "🧠 [VLK CHRONESTHESIA] Transitioned timeline slots to PAST (~{} tokens saved). FUTURE constraint injected:\n---\n{}\n---\nThese lessons will be prefixed as [PREVENTIVE FUTURE CONSTRAINT] in subsequent context fetches. Old raw logs are archived and will no longer consume context.",
                                    tokens_saved, learning
                                );
                                ok(serde_json::json!({
                                    "content": [{ "type": "text", "text": text }]
                                }))
                            }
                            Err(e) => err(format!("Time travel failed: {e}")),
                        }
                    }

                    // ── vlk_get_history: full timeline ──
                    "vlk_get_history" => {
                        let session_id = get_session_id(args);
                        let limit = args["limit"].as_i64().unwrap_or(50);

                        match get_timeline(&self.db, &session_id, limit).await {
                            Ok(slots) => {
                                let text = serde_json::to_string_pretty(&slots).unwrap_or_default();
                                ok(serde_json::json!({
                                    "content": [{ "type": "text", "text": text }]
                                }))
                            }
                            Err(e) => err(format!("Failed to get timeline: {e}")),
                        }
                    }

                    // ── vlk_search_memory: keyword search ──
                    "vlk_search_memory" => {
                        let session_id = get_session_id(args);
                        let query_str = args["query"]
                            .as_str()
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        let limit = args["limit"].as_i64().unwrap_or(20);

                        if query_str.is_empty() {
                            return err("Field 'query' is required.".into());
                        }

                        match search_timeline(&self.db, &session_id, &query_str, limit).await {
                            Ok(slots) => {
                                let text = serde_json::to_string_pretty(&slots).unwrap_or_default();
                                ok(serde_json::json!({
                                    "content": [{ "type": "text", "text": text }]
                                }))
                            }
                            Err(e) => err(format!("Search failed: {e}")),
                        }
                    }

                    // ── vlk_summarize_session: state counts ──
                    "vlk_summarize_session" => {
                        let session_id = get_session_id(args);

                        match get_session_summary(&self.db, &session_id).await {
                            Ok(summary) => ok(serde_json::json!({
                                "content": [{ "type": "text", "text": summary }]
                            })),
                            Err(e) => err(format!("Summarize failed: {e}")),
                        }
                    }

                    // ── vlk_fetch_context: clean active context (interceptor + consolidation + conflict detection) ──
                    "vlk_fetch_context" => {
                        let session_id = get_session_id(args);

                        match fetch_clean_context(&self.db, &session_id).await {
                            Ok(ctx) => ok(serde_json::json!({
                                "content": [{ "type": "text", "text": ctx }]
                            })),
                            Err(e) => err(format!("Fetch context failed: {e}")),
                        }
                    }

                    // ── vlk_revoke_future: remove an incorrectly learned constraint ──
                    "vlk_revoke_future" => {
                        let session_id = get_session_id(args);
                        let timeline_id = args["timeline_id"].as_i64().unwrap_or(0);

                        if timeline_id == 0 {
                            return err("Field 'timeline_id' is required.".into());
                        }

                        match revoke_future_constraint(&self.db, &session_id, timeline_id).await {
                            Ok(true) => ok(serde_json::json!({
                                "content": [{ "type": "text", "text": format!("🗑️ [VLK REVOKE] FUTURE constraint #{timeline_id} revoked (moved to PAST). It will no longer appear in active context.") }]
                            })),
                            Ok(false) => err(format!("No FUTURE constraint found with id #{timeline_id} in session '{session_id}'.")),
                            Err(e) => err(format!("Revoke failed: {e}")),
                        }
                    }

                    _ => ok(serde_json::json!({
                        "content": [{ "type": "text", "text": format!("Unknown tool: {name}") }],
                        "isError": true
                    })),
                }
            }

            "prompts/list" => ok(serde_json::json!({ "prompts": [] })),
            "notifications/initialized" => ok(serde_json::json!({})),

            _ => JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32601,
                    message: format!("Method not found: {}", req.method),
                }),
            },
        }
    }
}

// ── Main: stdio JSON-RPC loop ────────────────────────────────────────────────
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let db_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:vlk.db?mode=rwc".into());

    let pool = SqlitePoolOptions::new()
        .max_connections(10)
        .connect(&db_url)
        .await?;

    sqlx::query("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
        .execute(&pool)
        .await?;

    init_db(&pool).await?;

    let state = AppState { db: pool };

    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    info!("🚀 Vlk MCP Server v0.4.0-chronesthesia ready (stdio)");

    while let Some(line) = stdin.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let req: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                warn!("Parse error: {e}");
                let err = JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: None,
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32700,
                        message: e.to_string(),
                    }),
                };
                let payload = serde_json::to_string(&err)?;
                stdout.write_all(payload.as_bytes()).await?;
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
                continue;
            }
        };

        let resp = state.handle(req).await;
        let payload = serde_json::to_string(&resp)?;
        stdout.write_all(payload.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }

    Ok(())
}
