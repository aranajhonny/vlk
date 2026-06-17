use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::info;

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
}

// ── Database Setup ───────────────────────────────────────────────────────────

async fn init_db(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS agent_history (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id      TEXT NOT NULL,
            mem_id          INTEGER NOT NULL,
            role            TEXT NOT NULL,
            content         TEXT NOT NULL,
            tokens_estimated INTEGER NOT NULL,
            tool_call_id    TEXT,
            tool_calls      TEXT
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_session_mem
        ON agent_history(session_id, mem_id);
        "#,
    )
    .execute(pool)
    .await
    .context("Failed to create agent_history table")?;

    info!("Database initialized");
    Ok(())
}

// ── Atomic Prune & Write ─────────────────────────────────────────────────────

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
        let placeholders: String = target_mem_ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(",");

        let query_str = format!(
            "SELECT COALESCE(SUM(tokens_estimated), 0) AS total FROM agent_history WHERE session_id = ?1 AND mem_id IN ({})",
            placeholders
        );

        let mut q = sqlx::query_scalar::<_, i64>(&query_str).bind(session_id);
        for id in target_mem_ids {
            q = q.bind(id);
        }
        let tokens: i64 = q.fetch_one(&mut *tx).await?;

        let delete_str = format!(
            "DELETE FROM agent_history WHERE session_id = ?1 AND mem_id IN ({})",
            placeholders
        );
        let mut dq = sqlx::query(&delete_str).bind(session_id);
        for id in target_mem_ids {
            dq = dq.bind(id);
        }
        let rows = dq.execute(&mut *tx).await?.rows_affected();
        info!("Pruned {rows} rows, saved {tokens} tokens");

        tokens
    };

    let last_mem: Option<i64> =
        sqlx::query_scalar("SELECT MAX(mem_id) FROM agent_history WHERE session_id = ?1")
            .bind(session_id)
            .fetch_one(&mut *tx)
            .await?;

    let new_mem_id = last_mem.unwrap_or(0) + 1;
    let tokens_estimated = (memory_note.len() as f64 / 4.0).ceil() as i64;

    sqlx::query(
        r#"
        INSERT INTO agent_history (session_id, mem_id, role, content, tokens_estimated, tool_call_id, tool_calls)
        VALUES (?1, ?2, 'system', ?3, ?4, NULL, NULL)
        "#,
    )
    .bind(session_id)
    .bind(new_mem_id)
    .bind(memory_note)
    .bind(tokens_estimated)
    .execute(&mut *tx)
    .await?;

    // Clear orphaned tool metadata
    sqlx::query("UPDATE agent_history SET tool_calls = NULL WHERE session_id = ?1")
        .bind(session_id)
        .execute(&mut *tx)
        .await?;

    let remaining: Vec<Record> = sqlx::query_as(
        "SELECT id, session_id, mem_id, role, content, tokens_estimated, tool_call_id, tool_calls FROM agent_history WHERE session_id = ?1 ORDER BY mem_id ASC",
    )
    .bind(session_id)
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok((remaining, tokens_saved))
}

// ── MCP Tool Definition ─────────────────────────────────────────────────────

fn time_travel_tool_def() -> serde_json::Value {
    serde_json::json!({
        "name": "vlk_time_travel",
        "description": "Emergency memory management. Prunes dead-end memory slots and injects a lesson. Use when stuck in a failure loop or context is cluttered. Every message shows [mem_id:N] — use those exact numbers.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "target_mem_ids": {
                    "type": "array", "items": { "type": "integer" },
                    "description": "[mem_id:N] numbers to permanently DELETE."
                },
                "learning": {
                    "type": "string",
                    "description": "Concise lesson or condensed facts injected in place of deleted slots."
                }
            },
            "required": ["target_mem_ids", "learning"]
        }
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
        let err = |msg: &str| JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: id.clone(),
            error: Some(JsonRpcError {
                code: -32603,
                message: msg.into(),
            }),
            result: None,
        };

        match req.method.as_str() {
            "initialize" => ok(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "Vlk MemAct", "version": "0.2.0" }
            })),

            "tools/list" => ok(serde_json::json!({
                "tools": [time_travel_tool_def()]
            })),

            "tools/call" => {
                let params = req.params.unwrap_or_default();
                let name = params["name"].as_str().unwrap_or("");
                let args = &params["arguments"];

                if name != "vlk_time_travel" {
                    return ok(serde_json::json!({
                        "content": [{ "type": "text", "text": format!("Unknown tool: {name}") }],
                        "isError": true
                    }));
                }

                let target_ids: Vec<i64> = args["target_mem_ids"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
                    .unwrap_or_default();
                let learning = args["learning"].as_str().unwrap_or("").to_string();

                if target_ids.is_empty() && learning.is_empty() {
                    return ok(serde_json::json!({
                        "content": [{ "type": "text", "text": "Error: target_mem_ids and learning are required." }],
                        "isError": true
                    }));
                }

                match prune_and_write(&self.db, "default", &target_ids, &learning).await {
                    Ok((_, tokens_saved)) => {
                        let text = format!(
                            "⚠️ [VLK SYSTEM] {} slots pruned, {} tokens saved. Lesson injected:\n---\n{}\n---\nIgnore deleted content. Continue with this clean context.",
                            target_ids.len(), tokens_saved, learning
                        );
                        ok(serde_json::json!({ "content": [{ "type": "text", "text": text }] }))
                    }
                    Err(e) => err(&format!("Prune failed: {e}")),
                }
            }

            "prompts/list" => ok(serde_json::json!({ "prompts": [] })),
            "notifications/initialized" => JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: None,
                result: Some(serde_json::json!({})),
                error: None,
            },

            _ => JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: id.clone(),
                error: Some(JsonRpcError {
                    code: -32601,
                    message: format!("Method not found: {}", req.method),
                }),
                result: None,
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
        .max_connections(5)
        .connect(&db_url)
        .await?;
    sqlx::query("PRAGMA journal_mode=WAL")
        .execute(&pool)
        .await?;
    init_db(&pool).await?;

    let state = AppState { db: pool };
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    info!("Vlk MCP Server ready (stdio)");

    while let Some(line) = stdin.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let req: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let err = JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: None,
                    error: Some(JsonRpcError {
                        code: -32700,
                        message: format!("Parse error: {e}"),
                    }),
                    result: None,
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
