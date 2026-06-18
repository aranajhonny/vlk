use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{info, warn};

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

// ── Tool Input Types ─────────────────────────────────────────────────────────
#[derive(Debug, Deserialize)]
struct TimeTravelArgs {
    session_id: Option<String>,
    target_mem_ids: Vec<i64>,
    learning: String,
}

#[derive(Debug, Deserialize, Default)]
struct GetHistoryArgs {
    session_id: Option<String>,
    limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct SearchMemoryArgs {
    session_id: Option<String>,
    query: String,
    limit: Option<i64>,
}

// ── Data Models ──────────────────────────────────────────────────────────────
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Record {
    pub id: i64,
    pub session_id: String,
    pub mem_id: i64,
    pub role: String,
    pub content: String,
    pub tokens_estimated: i64,
    pub tool_call_id: Option<String>,
    pub tool_calls: Option<String>,
    pub created_at: Option<String>,
    pub parent_mem_id: Option<i64>,
    pub importance: Option<f64>,
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
        CREATE INDEX IF NOT EXISTS idx_session_created
            ON agent_history(session_id, created_at DESC);
        "#,
    )
    .execute(pool)
    .await
    .context("Failed to create agent_history table")?;

    info!("Database initialized with improved schema");
    Ok(())
}

// ── Core Operations ──────────────────────────────────────────────────────────
async fn prune_and_write(
    pool: &SqlitePool,
    session_id: &str,
    target_mem_ids: &[i64],
    memory_note: &str,
) -> Result<(Vec<Record>, i64)> {
    let mut tx = pool.begin().await?;

    let tokens_saved: i64 = if target_mem_ids.is_empty() {
        0
    } else {
        let json_ids = serde_json::to_value(target_mem_ids)?;

        let tokens: i64 = sqlx::query_scalar(
            r#"SELECT COALESCE(SUM(tokens_estimated), 0)
               FROM agent_history
               WHERE session_id = ?1 AND mem_id IN (SELECT value FROM json_each(?2))"#,
        )
        .bind(session_id)
        .bind(&json_ids)
        .fetch_one(&mut *tx)
        .await?;

        let rows = sqlx::query(
            r#"DELETE FROM agent_history
               WHERE session_id = ?1 AND mem_id IN (SELECT value FROM json_each(?2))"#,
        )
        .bind(session_id)
        .bind(&json_ids)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        info!("Pruned {rows} rows in session '{session_id}', saved {tokens} tokens");
        tokens
    };

    // Nuevo mem_id
    let last_mem: Option<i64> =
        sqlx::query_scalar("SELECT MAX(mem_id) FROM agent_history WHERE session_id = ?1")
            .bind(session_id)
            .fetch_one(&mut *tx)
            .await?;

    let new_mem_id = last_mem.unwrap_or(0) + 1;
    let tokens_estimated = ((memory_note.len() as f64) / 3.8).ceil() as i64;

    sqlx::query(
        r#"
        INSERT INTO agent_history
        (session_id, mem_id, role, content, tokens_estimated, created_at)
        VALUES (?1, ?2, 'system', ?3, ?4, ?5)
        "#,
    )
    .bind(session_id)
    .bind(new_mem_id)
    .bind(memory_note)
    .bind(tokens_estimated)
    .bind(Utc::now().to_rfc3339())
    .execute(&mut *tx)
    .await?;

    // Limpieza selectiva de tool_calls huérfanos
    if !target_mem_ids.is_empty() {
        sqlx::query("UPDATE agent_history SET tool_calls = NULL WHERE session_id = ?1")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
    }

    let remaining: Vec<Record> =
        sqlx::query_as("SELECT * FROM agent_history WHERE session_id = ?1 ORDER BY mem_id ASC")
            .bind(session_id)
            .fetch_all(&mut *tx)
            .await?;

    tx.commit().await?;
    Ok((remaining, tokens_saved))
}

async fn get_history(pool: &SqlitePool, session_id: &str, limit: i64) -> Result<Vec<Record>> {
    let records = sqlx::query_as(
        "SELECT * FROM agent_history WHERE session_id = ?1 ORDER BY mem_id DESC LIMIT ?2",
    )
    .bind(session_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(records)
}

async fn search_memory(
    pool: &SqlitePool,
    session_id: &str,
    query: &str,
    limit: i64,
) -> Result<Vec<Record>> {
    let pattern = format!("%{}%", query);
    let records = sqlx::query_as(
        r#"SELECT * FROM agent_history
           WHERE session_id = ?1
             AND content LIKE ?2
           ORDER BY created_at DESC LIMIT ?3"#,
    )
    .bind(session_id)
    .bind(pattern)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(records)
}

// ── MCP Tool Definitions ─────────────────────────────────────────────────────
fn tool_definitions() -> serde_json::Value {
    serde_json::json!({
        "tools": [
            {
                "name": "vlk_time_travel",
                "description": "Emergency memory management. Prunes dead-end memory slots and injects a lesson. Use when stuck in a failure loop or context is cluttered. Every message shows [mem_id:N] — use those exact numbers.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": { "type": "string", "description": "Optional. Defaults to 'default'." },
                        "target_mem_ids": { "type": "array", "items": { "type": "integer" }, "description": "[mem_id:N] numbers to permanently DELETE." },
                        "learning": { "type": "string", "description": "Concise lesson or condensed facts injected in place of deleted slots." }
                    },
                    "required": ["target_mem_ids", "learning"]
                }
            },
            {
                "name": "vlk_get_history",
                "description": "Returns memory history for a session. Use to recall past context or verify state.",
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
                "description": "Search memory by keyword. Useful for retrieving specific facts or decisions.",
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
                "description": "Returns a condensed textual summary of the current session memory.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": { "type": "string", "description": "Optional. Defaults to 'default'." }
                    }
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
                "serverInfo": { "name": "Vlk MemAct", "version": "0.3.0" }
            })),

            "tools/list" => ok(tool_definitions()),

            "tools/call" => {
                let params = req.params.unwrap_or_default();
                let name = params["name"].as_str().unwrap_or_default();
                let args = &params["arguments"];

                match name {
                    "vlk_time_travel" => {
                        let tool_args: TimeTravelArgs = serde_json::from_value(args.clone())
                            .unwrap_or_else(|_| TimeTravelArgs {
                                session_id: None,
                                target_mem_ids: vec![],
                                learning: String::new(),
                            });

                        let session_id = tool_args
                            .session_id
                            .unwrap_or_else(|| "default".to_string());
                        let learning = tool_args.learning.trim().to_string();

                        if learning.is_empty() {
                            return err("Field 'learning' is required and cannot be empty.".into());
                        }

                        match prune_and_write(
                            &self.db,
                            &session_id,
                            &tool_args.target_mem_ids,
                            &learning,
                        )
                        .await
                        {
                            Ok((_, tokens_saved)) => {
                                let text = format!(
                                    "⚠️ [VLK SYSTEM] {} slots pruned, {} tokens saved. Lesson injected:\n---\n{}\n---\nIgnore deleted content. Continue with this clean context.",
                                    tool_args.target_mem_ids.len(),
                                    tokens_saved,
                                    learning
                                );
                                ok(serde_json::json!({
                                    "content": [{ "type": "text", "text": text }]
                                }))
                            }
                            Err(e) => err(format!("Prune operation failed: {e}")),
                        }
                    }

                    "vlk_get_history" => {
                        let tool_args: GetHistoryArgs =
                            serde_json::from_value(args.clone()).unwrap_or_default();
                        let session_id = get_session_id(args);
                        let limit = tool_args.limit.unwrap_or(50);

                        match get_history(&self.db, &session_id, limit).await {
                            Ok(records) => ok(serde_json::json!({
                                "content": [{ "type": "text", "text": serde_json::to_string_pretty(&records).unwrap_or_default() }]
                            })),
                            Err(e) => err(format!("Failed to get history: {e}")),
                        }
                    }

                    "vlk_search_memory" => {
                        let tool_args: SearchMemoryArgs = serde_json::from_value(args.clone())
                            .unwrap_or_else(|_| SearchMemoryArgs {
                                session_id: None,
                                query: String::new(),
                                limit: None,
                            });

                        if tool_args.query.trim().is_empty() {
                            return err("Field 'query' is required.".into());
                        }

                        let session_id = get_session_id(args);
                        let limit = tool_args.limit.unwrap_or(20);

                        match search_memory(&self.db, &session_id, &tool_args.query, limit).await {
                            Ok(records) => ok(serde_json::json!({
                                "content": [{ "type": "text", "text": serde_json::to_string_pretty(&records).unwrap_or_default() }]
                            })),
                            Err(e) => err(format!("Search failed: {e}")),
                        }
                    }

                    "vlk_summarize_session" => {
                        let session_id = get_session_id(args);

                        match get_history(&self.db, &session_id, 100).await {
                            Ok(records) => {
                                let total = records.len();
                                let latest_mem = records.first().map(|r| r.mem_id).unwrap_or(0);
                                let system_msgs =
                                    records.iter().filter(|r| r.role == "system").count();
                                let total_tokens: i64 =
                                    records.iter().map(|r| r.tokens_estimated).sum();

                                let summary = format!(
                                    "Session '{}': {} total memories, {} system-injected lessons. Latest mem_id: {}. Estimated tokens: {}. Created at: {}.",
                                    session_id,
                                    total,
                                    system_msgs,
                                    latest_mem,
                                    total_tokens,
                                    records.first().and_then(|r| r.created_at.clone()).unwrap_or_else(|| "unknown".into())
                                );
                                ok(serde_json::json!({
                                    "content": [{ "type": "text", "text": summary }]
                                }))
                            }
                            Err(e) => err(format!("Summarize failed: {e}")),
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

    info!("🚀 Vlk MCP Server v0.3.0 ready (stdio)");

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
