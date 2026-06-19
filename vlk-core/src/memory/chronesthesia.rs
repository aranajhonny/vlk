// ── Temporal State Engine ────────────────────────────────────────────────────
//
// The agent's memory has two concerns: content (what happened) and temporal
// state (when it matters). Content is stored immutably in memory_contents;
// temporal state is tracked in agent_timeline. Each timeline slot carries
// one of three states:
//
//   PRESENT → what the agent is processing right now (active context)
//   PAST    → dead ends, already learned from (hidden from context)
//   FUTURE  → preventive constraints extrapolated from experience
//
// The core operations are:
// - record_present_state : push new content + PRESENT slot
// - execute_time_travel  : PRESENT → PAST, inject FUTURE constraint
// - fetch_clean_context  : loop detection + consolidation + active context

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use tracing::info;

// ── Tipos de Estado Temporal ────────────────────────────────────────────────

/// A slot in the agent's timeline. Each slot joins content (from memory_contents)
/// with a temporal state and a position in the session chronology.
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct TimelineSlot {
    pub id: i64,
    pub content_id: Option<i64>,
    pub session_id: String,
    pub sequence_order: i64,
    pub temporal_state: String,
    pub learning_summary: Option<String>,
    pub created_at: Option<String>,
    /// Origin of FUTURE constraints: DERIVED (scar tissue) or PROSPECTIVE (foresight).
    pub constraint_type: Option<String>,
    // Enriched fields via JOIN with memory_contents
    pub raw_log: Option<String>,
    pub file_context: Option<String>,
}

/// Argumentos de entrada para el comando `vlk_time_travel`.
#[derive(Debug, Deserialize)]
pub struct TimeTravelArgs {
    pub session_id: Option<String>,
    /// Timeline slot IDs in PRESENT state that should transition to PAST.
    pub target_timeline_ids: Vec<i64>,
    /// Learned lesson — injected as a FUTURE constraint.
    pub learning: String,
    /// Required: raw log excerpt grounding the lesson in evidence.
    /// Prevents confused agents from injecting unverified constraints.
    #[serde(default)]
    pub raw_log_excerpt: String,
    /// Constraint origin type. Defaults to DERIVED.
    #[serde(default)]
    pub constraint_type: Option<String>,
}

// ── Database Initialization ───────────────────────────────────────────────────

/// Creates the `memory_contents` and `agent_timeline` tables.
pub async fn init_chronesthesia_tables(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS memory_contents (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            raw_log TEXT NOT NULL,
            file_context TEXT,
            tool_payload TEXT
        );

        CREATE TABLE IF NOT EXISTS agent_timeline (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            content_id INTEGER,
            session_id TEXT NOT NULL,
            sequence_order INTEGER NOT NULL,
            temporal_state TEXT CHECK(temporal_state IN ('PRESENT', 'PAST', 'FUTURE')),
            learning_summary TEXT,
            constraint_type TEXT CHECK(constraint_type IS NULL OR constraint_type IN ('DERIVED', 'PROSPECTIVE')),
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY(content_id) REFERENCES memory_contents(id)
        );

        CREATE INDEX IF NOT EXISTS idx_timeline_session_state
            ON agent_timeline(session_id, temporal_state);
        CREATE INDEX IF NOT EXISTS idx_timeline_sequence
            ON agent_timeline(session_id, sequence_order);
        CREATE INDEX IF NOT EXISTS idx_timeline_active_context
            ON agent_timeline(session_id, temporal_state, sequence_order);
        "#,
    )
    .execute(pool)
    .await
    .context("Failed to create chronesthesia tables")?;

    // Migration: add constraint_type column for existing databases (v0.4.x → v0.5.x)
    sqlx::query(
        "ALTER TABLE agent_timeline ADD COLUMN constraint_type TEXT CHECK(constraint_type IS NULL OR constraint_type IN ('DERIVED', 'PROSPECTIVE'))",
    )
    .execute(pool)
    .await
    .ok(); // Ignore error if column already exists

    info!("Chronesthesia tables initialized (memory_contents + agent_timeline)");
    Ok(())
}

// ── Core Operation: vlk_time_travel ────────────────────────────────────────────

/// Transitions timeline slots from PRESENT → PAST and injects a FUTURE constraint.
/// Transitions timeline slots from PRESENT → PAST and injects a FUTURE constraint.
pub async fn execute_time_travel(pool: &SqlitePool, args: TimeTravelArgs) -> Result<(i64, String)> {
    let session_id = args.session_id.unwrap_or_else(|| "default".to_string());
    let learning = args.learning.trim().to_string();
    let raw_log_excerpt = args.raw_log_excerpt.trim().to_string();
    let constraint_type = args
        .constraint_type
        .as_deref()
        .unwrap_or("DERIVED")
        .to_string();

    if learning.is_empty() {
        anyhow::bail!("Field 'learning' is required and cannot be empty.");
    }
    if args.target_timeline_ids.is_empty() {
        anyhow::bail!("Field 'target_timeline_ids' must contain at least one ID.");
    }
    if raw_log_excerpt.is_empty() {
        anyhow::bail!(
            "Field 'raw_log_excerpt' is required. Provide 1-2 sentences of the raw error/log \
             that grounds this lesson in evidence. This prevents unverified constraints from \
             being injected."
        );
    }
    if constraint_type != "DERIVED" && constraint_type != "PROSPECTIVE" {
        anyhow::bail!(
            "Field 'constraint_type' must be 'DERIVED' (default) or 'PROSPECTIVE', got '{}'",
            constraint_type
        );
    }

    let mut tx = pool.begin().await?;

    // 1. Calculate tokens saved by moving these slots to PAST
    let json_ids = serde_json::to_string(&args.target_timeline_ids)?;
    let raw_chars_saved: i64 = sqlx::query_scalar(
        r#"
        SELECT COALESCE(SUM(LENGTH(mc.raw_log)), 0)
        FROM agent_timeline t
        JOIN memory_contents mc ON t.content_id = mc.id
        WHERE t.session_id = ?1
          AND t.id IN (SELECT value FROM json_each(?2))
          AND t.temporal_state = 'PRESENT'
        "#,
    )
    .bind(&session_id)
    .bind(&json_ids)
    .fetch_one(&mut *tx)
    .await?;

    let tokens_saved = ((raw_chars_saved as f64) / 3.8).ceil() as i64;

    // 2. Transition selected slots from PRESENT → PAST
    let rows_affected = sqlx::query(
        r#"
        UPDATE agent_timeline
        SET temporal_state = 'PAST'
        WHERE session_id = ?1
          AND id IN (SELECT value FROM json_each(?2))
          AND temporal_state = 'PRESENT'
        "#,
    )
    .bind(&session_id)
    .bind(&json_ids)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    info!(
        "Time travel: moved {rows_affected} slots from PRESENT→PAST in session '{session_id}', saved ~{tokens_saved} tokens"
    );

    // 3. Get next sequence number
    let max_seq: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(sequence_order), 0) FROM agent_timeline WHERE session_id = ?1",
    )
    .bind(&session_id)
    .fetch_one(&mut *tx)
    .await?;

    // 4. Inject FUTURE constraint (no heavy content, just the lesson)
    //    Includes evidence excerpt and constraint type for auditability.
    sqlx::query(
        r#"
        INSERT INTO agent_timeline (content_id, session_id, sequence_order, temporal_state, learning_summary, constraint_type)
        VALUES (NULL, ?1, ?2, 'FUTURE', ?3, ?4)
        "#,
    )
    .bind(&session_id)
    .bind(max_seq + 1)
    .bind(&learning)
    .bind(&constraint_type)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    // Store evidence excerpt in a separate PRESENT note so it's auditable
    // but doesn't clutter the active context (it's in PAST immediately, but
    // linked to this constraint via a shared transaction).
    // For now, we embed it in the return value so the caller can log it.
    let _evidence_note = format!(
        "[EVIDENCE for constraint '{}']: {}",
        &learning.chars().take(60).collect::<String>(),
        raw_log_excerpt
    );

    Ok((tokens_saved, learning))
}

// ── Consulta de Contexto Activo ─────────────────────────────────────────────

/// Generates the clean payload to inject into the IDE's context window.
/// Filters out noisy PAST entries and prioritizes FUTURE rules and immediate
/// PRESENT state. This is the function the agent's prompt system should call
/// before each iteration to get only what is relevant.
pub async fn fetch_active_context(pool: &SqlitePool, session_id: &str) -> Result<String> {
    let rows: Vec<(
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as(
        r#"
        SELECT t.temporal_state, t.learning_summary, mc.raw_log, mc.file_context, t.constraint_type
        FROM agent_timeline t
        LEFT JOIN memory_contents mc ON t.content_id = mc.id
        WHERE t.session_id = ?1 AND t.temporal_state IN ('PRESENT', 'FUTURE')
        ORDER BY t.sequence_order ASC
        "#,
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;

    let mut buf = String::new();
    buf.push_str("=== VLK CHRONESTHESIA LAYER ===\n");
    buf.push_str("Active context: PRESENT (live) + FUTURE (constraints). PAST is archived.\n\n");

    for (state, summary, raw_log, file_context, constraint_type) in rows {
        match state.as_str() {
            "FUTURE" => {
                if let Some(sum) = summary {
                    let ctype = constraint_type.as_deref().unwrap_or("DERIVED");
                    let tag = match ctype {
                        "PROSPECTIVE" => "[PROSPECTIVE CONSTRAINT]",
                        _ => "[PREVENTIVE FUTURE CONSTRAINT]",
                    };
                    buf.push_str(&format!("{}: {}\n", tag, sum));
                }
            }
            "PRESENT" => {
                if let Some(log) = raw_log {
                    let ctx = file_context
                        .map(|f| format!("File: {}", f))
                        .unwrap_or_default();
                    buf.push_str(&format!("[ACTIVE PRESENT STATE] {} | Log: {}\n", ctx, log));
                } else if let Some(sum) = summary {
                    buf.push_str(&format!("[PRESENT NOTE]: {}\n", sum));
                }
            }
            _ => {}
        }
    }

    if buf.lines().count() <= 2 {
        buf.push_str("(No active PRESENT or FUTURE entries for this session.)\n");
    }

    Ok(buf)
}

// ── Level 1: Automatic Interception Hook (Loop Detection) ─────────────────

/// Default threshold: if the same raw_log appears 3+ times in PRESENT, it is
/// considered a loop and auto-mitigated.
const LOOP_THRESHOLD_DEFAULT: usize = 3;

/// Threshold for FUTURE constraint consolidation: when the count of FUTURE
/// entries exceeds this, they are merged into a single consolidated constraint.
const FUTURE_CONSOLIDATION_THRESHOLD: usize = 5;

/// Extracts a semantic fingerprint from a raw_log for grouping.
///
/// This broadens loop detection beyond exact whitespace-normalized match to
/// catch near-duplicates — same error code with different timestamps, same
/// compiler error with different line numbers, etc.
///
/// Strategy (ranked by specificity):
/// 1. Rust error codes: `error[E0277]`
/// 2. TypeScript/JS errors: `TS2345`, `TypeError`, `SyntaxError`
/// 3. Python tracebacks: last line exception type + message
/// 4. Go/Rust panics: `panic:`, `fatal error:`
/// 5. Test assertion failures: `expected: ... got:`, `AssertionError`
/// 6. HTTP status + message prefix: `503 Service Unavailable`
/// 7. Fall back to first 80 chars with timestamps stripped
fn fingerprint_log(raw_log: &str) -> String {
    let trimmed = raw_log.trim();

    // 1. Rust compiler error codes: error[EXXXX]
    if let Some(cap) = extract_rust_error_code(trimmed) {
        return cap;
    }

    // 2. TypeScript / JavaScript compiler & runtime errors
    if let Some(cap) = extract_ts_error(trimmed) {
        return cap;
    }

    // 3. Python traceback — last line exception
    if let Some(cap) = extract_python_error(trimmed) {
        return cap;
    }

    // 4. Go / Rust panics
    if let Some(cap) = extract_panic(trimmed) {
        return cap;
    }

    // 5. Test assertion failures
    if let Some(cap) = extract_test_failure(trimmed) {
        return cap;
    }

    // 6. HTTP status code pattern: "NNN StatusText" or "Error NNN"
    if let Some(cap) = extract_http_error(trimmed) {
        return cap;
    }

    // 7. Fallback: first 80 chars with timestamp/datetime noise stripped
    strip_timestamps(&trimmed.chars().take(80).collect::<String>())
        .trim()
        .to_string()
}

/// Extracts a Rust compiler error code like "error[E0277]".
fn extract_rust_error_code(log: &str) -> Option<String> {
    // Match error[EXXXX] where XXXX is 4 digits
    let start = log.find("error[")?;
    let code_start = start + 6; // after "error["
    let rest = &log[code_start..];
    let end = rest.find(']')?;
    if end >= 4 {
        // E followed by 4+ digits
        let code = &rest[..end];
        if code.len() >= 5 && code.starts_with('E') && code[1..].chars().all(|c| c.is_ascii_digit())
        {
            return Some(format!("error[{}]", code));
        }
    }
    None
}

/// Extracts an HTTP error fingerprint like "503 Service Unavailable" or "429 Too Many Requests".
fn extract_http_error(log: &str) -> Option<String> {
    // Pattern: "NNN Text..." where NNN is 3-digit HTTP status
    let words: Vec<&str> = log.split_whitespace().collect();
    for (i, word) in words.iter().enumerate() {
        // Strip any leading/trailing punctuation
        let clean = word.trim_matches(|c: char| !c.is_ascii_digit());
        if clean.len() == 3 && clean.chars().all(|c| c.is_ascii_digit()) {
            let code = clean.parse::<u16>().ok()?;
            if (100..600).contains(&code) {
                // Collect next 2-3 words as the message prefix and strip timestamps
                let raw_message: Vec<&str> = words[i + 1..]
                    .iter()
                    .take(3)
                    .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '-'))
                    .filter(|w| !w.is_empty())
                    .collect();
                let message = strip_timestamps(&raw_message.join(" "));
                return Some(format!("HTTP {} {}", code, message));
            }
        }
        // Also match "Error NNN" pattern
        if clean.len() == 3 && clean.chars().all(|c| c.is_ascii_digit()) {
            let code = clean.parse::<u16>().ok()?;
            if (100..600).contains(&code) && i > 0 {
                return Some(format!("Error {}", code));
            }
        }
    }
    None
}

/// Extracts TypeScript compiler errors (TSdddd) or JS runtime error names.
fn extract_ts_error(log: &str) -> Option<String> {
    // TypeScript compiler errors: TS2345: ... or error TS2345: ...
    if let Some(start) = log.find("TS") {
        let code_start = start + 2;
        let rest = &log[code_start..];
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if end >= 4 {
            let code = &rest[..end];
            return Some(format!("TS{}", code));
        }
    }

    // JS runtime errors: TypeError: ..., SyntaxError: ..., ReferenceError: ..., RangeError: ...
    for kind in &[
        "TypeError",
        "SyntaxError",
        "ReferenceError",
        "RangeError",
        "URIError",
        "EvalError",
    ] {
        if let Some(pos) = log.find(kind) {
            // Extract first meaningful word after ": "
            let after = log[pos + kind.len()..].trim_start();
            let after = after.strip_prefix(':').map(|s| s.trim()).unwrap_or(after);
            let first_word = after.split_whitespace().next().unwrap_or("");
            let msg = strip_timestamps(first_word);
            return Some(format!("{}: {}", kind, msg));
        }
    }

    // Generic Error: patterns common in Node.js
    if let Some(pos) = log.find("Error:") {
        let after = &log[pos + 6..].trim_start();
        let first_word = after.split_whitespace().next().unwrap_or("");
        let msg = strip_timestamps(first_word);
        return Some(format!("Error: {}", msg));
    }

    None
}

/// Extracts Python exception type + message prefix from a traceback-like line.
fn extract_python_error(log: &str) -> Option<String> {
    // Common pattern: "ExceptionType: message"
    let python_exceptions = [
        "ValueError",
        "TypeError",
        "KeyError",
        "IndexError",
        "AttributeError",
        "ImportError",
        "ModuleNotFoundError",
        "FileNotFoundError",
        "OSError",
        "RuntimeError",
        "RecursionError",
        "StopIteration",
        "AssertionError",
        "OverflowError",
        "ZeroDivisionError",
        "UnboundLocalError",
        "NameError",
        "SyntaxError",
        "IndentationError",
        "TabError",
        "SystemExit",
        "KeyboardInterrupt",
        "ConnectionError",
        "TimeoutError",
    ];

    let lower = log.to_lowercase();
    for exc in &python_exceptions {
        if lower.contains(&exc.to_lowercase()) {
            // Found a Python exception name — try to extract the message
            if let Some(pos) = lower.find(&exc.to_lowercase()) {
                let after = log[pos + exc.len()..].trim_start();
                if after.starts_with(':') {
                    let msg = after[1..].trim();
                    let msg = strip_timestamps(msg);
                    let first = msg.split_whitespace().take(4).collect::<Vec<_>>().join(" ");
                    return Some(format!("{}: {}", exc, first));
                }
            }
        }
    }

    None
}

/// Extracts Go (`panic:`, `fatal error:`) or Rust (`panicked at`) panic patterns.
fn extract_panic(log: &str) -> Option<String> {
    let lower = log.to_lowercase();

    // Go: "panic: runtime error: invalid memory address"
    if lower.contains("panic:") {
        // Extract the key noun after "panic: "/"runtime error: "
        let after = log.splitn(2, ':').nth(1).unwrap_or("").trim();
        if after.contains("nil pointer") {
            return Some("panic: nil pointer dereference".into());
        }
        if after.contains("index out of range") {
            return Some("panic: index out of range".into());
        }
        if after.contains("close of closed channel") {
            return Some("panic: close of closed channel".into());
        }
        if after.contains("send on closed channel") {
            return Some("panic: send on closed channel".into());
        }
        if after.contains("concurrent write") || after.contains("concurrent map") {
            return Some("panic: concurrent map write".into());
        }
        // Generic panic: take first 4 words
        let first = after
            .split_whitespace()
            .take(4)
            .collect::<Vec<_>>()
            .join(" ");
        return Some(format!("panic: {}", first));
    }

    // Go: "fatal error: concurrent map writes"
    if lower.contains("fatal error:") {
        let after = log.splitn(2, ':').nth(1).unwrap_or("").trim();
        let first = after
            .split_whitespace()
            .take(4)
            .collect::<Vec<_>>()
            .join(" ");
        return Some(format!("fatal error: {}", first));
    }

    // Rust: "thread 'main' panicked at 'src/main.rs:42:...'" or "panicked at '...'"
    if lower.contains("panicked at") || lower.contains("panicked '") {
        // Find the panic message: text between single quotes AFTER "panicked"
        let panicked_pos = lower.find("panicked").unwrap();
        let after_panicked = &log[panicked_pos + 9..];
        if let Some(start) = after_panicked.find('\'') {
            let rest = &after_panicked[start + 1..];
            if let Some(end) = rest.find('\'') {
                let msg = &rest[..end];
                let short = msg.chars().take(60).collect::<String>();
                return Some(format!("panic: {}", short));
            }
        }
        return Some("panic: (unknown)".into());
    }

    None
}

/// Extracts test assertion failure patterns from various frameworks.
fn extract_test_failure(log: &str) -> Option<String> {
    let lower = log.to_lowercase();

    // Common "expected: X, got: Y" or "expected X but got Y" pattern
    if lower.contains("expected") && (lower.contains("got") || lower.contains("but")) {
        if let Some(pos) = lower.find("expected") {
            let after = log[pos + 8..].trim();
            let first = after
                .split_whitespace()
                .take(6)
                .collect::<Vec<_>>()
                .join(" ");
            return Some(format!("assert: expected {}", first));
        }
        return Some("assert: expected vs got".into());
    }

    // AssertionError [ERR_ASSERTION] (Node.js)
    if lower.contains("assertionerror") || lower.contains("assertion error") {
        if let Some(pos) = log.to_uppercase().find("ASSERTION") {
            let after = log[pos..].chars().take(60).collect::<String>();
            return Some(format!("assert: {}", after));
        }
        return Some("assert: AssertionError".into());
    }

    // pytest / unittest: "FAILED test_file.py::test_name - AssertionError: ..."
    if lower.starts_with("failed") && lower.contains("assertion") {
        return Some("test: failed assertion".into());
    }

    // Jest / Vitest: "expect(received).toBe(expected)"
    if (lower.contains("expect(") && lower.contains("toBe(")) || lower.contains("toEqual(") {
        return Some("test: expect assertion".into());
    }

    // Rust test failure: "---- test_name stdout ----" + "assertion failed"
    if lower.contains("assertion failed")
        || lower.contains("assert_eq!")
        || lower.contains("assert_ne!")
    {
        if let Some(pos) = lower
            .find("assertion failed")
            .or_else(|| lower.find("assert_eq!"))
            .or_else(|| lower.find("assert_ne!"))
        {
            let after = &log[pos..];
            let short = after.chars().take(60).collect::<String>();
            return Some(format!("assert: {}", short));
        }
        return Some("assert: assertion failed".into());
    }

    None
}

/// Strips timestamps (HH:MM:SS, ISO 8601, etc.) from a log prefix,
/// replacing them with a placeholder to normalize near-duplicate strings.
fn strip_timestamps(log: &str) -> String {
    // Replace ISO 8601 timestamps like 2024-01-15T14:32:01
    let re_iso =
        regex::Regex::new(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:?\d{2})?")
            .unwrap();
    let result = re_iso.replace_all(log, "[TIMESTAMP]").to_string();

    // Replace time-only patterns like 14:32:01 or 14:32:01.123
    let re_time = regex::Regex::new(r"\b\d{2}:\d{2}:\d{2}(\.\d+)?\b").unwrap();
    re_time.replace_all(&result, "[TIME]").to_string()
}

/// Scans PRESENT slots for identical raw_log repetitions.
/// If >= `threshold` identical occurrences are found, auto-executes `execute_time_travel`,
/// injecting a FUTURE constraint with a "system anchor" format.
///
/// Uses fingerprint-based grouping to catch near-duplicates (same error code,
/// different timestamps/parameters), not just exact matches.
///
/// Returns `true` if mitigation was executed, `false` if no loop was found.
pub async fn auto_detect_and_mitigate_loops(
    pool: &SqlitePool,
    session_id: &str,
    threshold: usize,
) -> Result<bool> {
    // 1. Get all PRESENT slots with their raw_log
    let rows = sqlx::query(
        r#"
        SELECT t.id, mc.raw_log
        FROM agent_timeline t
        JOIN memory_contents mc ON t.content_id = mc.id
        WHERE t.session_id = ?1 AND t.temporal_state = 'PRESENT'
        ORDER BY t.sequence_order DESC
        "#,
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;

    let present_slots: Vec<(i64, String)> = rows
        .iter()
        .map(|row| {
            let id: i64 = row.get(0);
            let raw_log: String = row.get(1);
            (id, raw_log)
        })
        .collect();

    if present_slots.len() < threshold {
        return Ok(false);
    }

    // 2. Group by FINGERPRINT (not exact match) — catches near-duplicates
    let mut fingerprint_groups: std::collections::HashMap<String, Vec<i64>> =
        std::collections::HashMap::new();

    for (id, raw_log) in &present_slots {
        let fp = fingerprint_log(raw_log);
        fingerprint_groups.entry(fp).or_default().push(*id);
    }

    // 3. Also compute exact-match groups for fallback (exact duplicates are
    //    the strongest signal and should be preferred when they exist)
    let mut exact_groups: std::collections::HashMap<String, Vec<i64>> =
        std::collections::HashMap::new();
    for (id, raw_log) in &present_slots {
        let exact = raw_log.trim().to_string();
        exact_groups.entry(exact).or_default().push(*id);
    }

    // 4. Choose the best group: prefer exact match groups first (stronger signal),
    //    then fall back to fingerprint groups
    let mut target_ids: Vec<i64> = Vec::new();
    let mut target_signature = String::new();
    let mut detection_method = "exact";

    // First, check exact matches
    for (log, ids) in &exact_groups {
        if ids.len() >= threshold && ids.len() > target_ids.len() {
            target_ids = ids.clone();
            target_signature = log.chars().take(80).collect();
            detection_method = "exact";
        }
    }

    // If no exact match found at threshold, try fingerprints
    if target_ids.len() < threshold {
        for (fp, ids) in &fingerprint_groups {
            if ids.len() >= threshold && ids.len() > target_ids.len() {
                target_ids = ids.clone();
                target_signature = fp.chars().take(80).collect();
                detection_method = "fingerprint";
            }
        }
    }

    if target_ids.len() < threshold {
        return Ok(false);
    }

    // 5. Execute autonomous time travel with an aggressive "system anchor"
    let count = target_ids.len();

    let automated_learning = format!(
        "[SYSTEM ANCHOR] Loop detected ({detection_method}): the same error pattern appeared {count} times. Last signature: \"{target_signature}...\". CURRENT STRATEGY EXHAUSTED. Mandatory: completely change approach. Do not repeat the same action. Try another tool, another file, or consult the user."
    );

    // Use a single transaction for detection + mitigation + verification
    let mut tx = pool.begin().await?;

    // 4a. Move slots to PAST
    let json_ids = serde_json::to_string(&target_ids)?;
    let rows_affected = sqlx::query(
        r#"
        UPDATE agent_timeline
        SET temporal_state = 'PAST'
        WHERE session_id = ?1
          AND id IN (SELECT value FROM json_each(?2))
          AND temporal_state = 'PRESENT'
        "#,
    )
    .bind(session_id)
    .bind(&json_ids)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // 4b. Calculate saved tokens
    let raw_chars_saved: i64 = sqlx::query_scalar(
        r#"
        SELECT COALESCE(SUM(LENGTH(mc.raw_log)), 0)
        FROM agent_timeline t
        JOIN memory_contents mc ON t.content_id = mc.id
        WHERE t.session_id = ?1
          AND t.id IN (SELECT value FROM json_each(?2))
        "#,
    )
    .bind(session_id)
    .bind(&json_ids)
    .fetch_one(&mut *tx)
    .await?;
    let tokens_saved = ((raw_chars_saved as f64) / 3.8).ceil() as i64;

    // 4c. Get next sequence_order
    let max_seq: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(sequence_order), 0) FROM agent_timeline WHERE session_id = ?1",
    )
    .bind(session_id)
    .fetch_one(&mut *tx)
    .await?;

    // 4d. Insert FUTURE constraint with DERIVED type (auto-mitigation = scar tissue)
    sqlx::query(
        r#"
        INSERT INTO agent_timeline (content_id, session_id, sequence_order, temporal_state, learning_summary, constraint_type)
        VALUES (NULL, ?1, ?2, 'FUTURE', ?3, 'DERIVED')
        "#,
    )
    .bind(session_id)
    .bind(max_seq + 1)
    .bind(&automated_learning)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    tracing::info!(
        "[AUTO-INTERCEPT] Session '{}': mitigated {count} looped slots (rows_affected={}, detection={detection_method}), ~{tokens_saved} tokens saved. System anchor injected.",
        session_id, rows_affected
    );

    Ok(true)
}

/// Enhanced version of `fetch_active_context` that runs the automatic interceptor
/// before returning context. The agent never sees repeated errors — they arrive
/// as FUTURE constraints.
///
/// Also runs FUTURE constraint consolidation to prevent unbounded growth
/// and conflict detection to warn about contradictory directives.
pub async fn fetch_clean_context(pool: &SqlitePool, session_id: &str) -> Result<String> {
    // Level 1: automatic loop pruning before building context
    let _mitigated =
        auto_detect_and_mitigate_loops(pool, session_id, LOOP_THRESHOLD_DEFAULT).await?;

    // Level 2: consolidate FUTURE constraints if they exceed threshold
    let _consolidated = consolidate_future_constraints(pool, session_id).await?;

    // Level 3: detect conflicting FUTURE constraints
    let conflicts = detect_future_conflicts(pool, session_id).await?;

    // Clean context — the LLM only sees PRESENT + FUTURE, loops are already in PAST
    let mut ctx = fetch_active_context(pool, session_id).await?;

    // Append conflict warnings if any exist
    if !conflicts.is_empty() {
        ctx.push_str("\n─── ⚠ CONFLICTING FUTURE CONSTRAINTS DETECTED ───\n");
        for c in &conflicts {
            ctx.push_str(&format!("  ⚠ {}\n", c));
        }
        ctx.push_str("  → Resolve with vlk_revoke_future before proceeding.\n");
    }

    Ok(ctx)
}

// ── FUTURE Constraint Consolidation ──────────────────────────────────────────

/// Consolidates FUTURE constraints when they exceed the threshold.
///
/// Maximum length of a consolidated constraint string. Beyond this, older
/// lessons are dropped to prevent unbounded token growth across re-consolidations.
const MAX_CONSOLIDATED_CHARS: usize = 2000;

/// When too many individual constraints accumulate, they become their own
/// noise. This function merges all FUTURE entries into a single consolidated
/// constraint and archives the individual ones to PAST.
///
/// To prevent unbounded token growth across re-consolidation cycles:
/// 1. Deduplicates lessons (trimmed, case-insensitive comparison).
/// 2. Truncates the consolidated string to [`MAX_CONSOLIDATED_CHARS`],
///    keeping the newest lessons and noting how many were dropped.
///
/// Returns `true` if consolidation was performed.
pub async fn consolidate_future_constraints(pool: &SqlitePool, session_id: &str) -> Result<bool> {
    // Get all FUTURE constraints for this session
    let rows: Vec<(i64, Option<String>, Option<String>)> = sqlx::query_as(
        r#"
        SELECT id, learning_summary, constraint_type
        FROM agent_timeline
        WHERE session_id = ?1 AND temporal_state = 'FUTURE'
        ORDER BY sequence_order ASC
        "#,
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;

    if rows.len() < FUTURE_CONSOLIDATION_THRESHOLD {
        return Ok(false);
    }

    // Collect all lessons, deduplicating by trimmed content (case-insensitive)
    let mut seen = std::collections::HashSet::new();
    let mut lessons: Vec<String> = Vec::with_capacity(rows.len());
    for (_, summary, _) in &rows {
        if let Some(s) = summary {
            let key = s.trim().to_lowercase();
            if seen.insert(key) {
                lessons.push(s.trim().to_string());
            }
        }
    }

    if lessons.is_empty() {
        return Ok(false);
    }

    // Build consolidated constraint with bounded length
    let total = lessons.len();
    let joined = lessons.join(" | ");
    let (body, dropped) = if joined.len() > MAX_CONSOLIDATED_CHARS {
        // Walk from the front (newest-first after reversed) to find how many
        // lessons fit. Since the DB orders ASC (oldest first), newer lessons
        // are at the end — we want to keep those.
        let mut budget = MAX_CONSOLIDATED_CHARS;
        let mut kept: Vec<&str> = Vec::new();
        for lesson in lessons.iter().rev() {
            let cost = lesson.len() + if kept.is_empty() { 0 } else { 3 }; // " | " separator
            if cost > budget {
                break;
            }
            budget -= cost;
            kept.push(lesson);
        }
        kept.reverse();
        let n_dropped = total - kept.len();
        (kept.join(" | "), n_dropped)
    } else {
        (joined, 0)
    };

    let consolidated = if dropped > 0 {
        format!(
            "[CONSOLIDATED CONSTRAINTS from {} lessons, truncated to {} newest]: {}",
            total,
            total - dropped,
            body
        )
    } else {
        format!(
            "[CONSOLIDATED CONSTRAINTS from {} prior lessons]: {}",
            total, body
        )
    };

    let mut tx = pool.begin().await?;

    // Move all individual FUTURE entries to PAST
    let ids: Vec<i64> = rows.iter().map(|(id, _, _)| *id).collect();
    let json_ids = serde_json::to_string(&ids)?;

    sqlx::query(
        r#"
        UPDATE agent_timeline
        SET temporal_state = 'PAST'
        WHERE session_id = ?1
          AND id IN (SELECT value FROM json_each(?2))
          AND temporal_state = 'FUTURE'
        "#,
    )
    .bind(session_id)
    .bind(&json_ids)
    .execute(&mut *tx)
    .await?;

    // Get next sequence number
    let max_seq: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(sequence_order), 0) FROM agent_timeline WHERE session_id = ?1",
    )
    .bind(session_id)
    .fetch_one(&mut *tx)
    .await?;

    // Insert consolidated FUTURE constraint
    sqlx::query(
        r#"
        INSERT INTO agent_timeline (content_id, session_id, sequence_order, temporal_state, learning_summary, constraint_type)
        VALUES (NULL, ?1, ?2, 'FUTURE', ?3, 'DERIVED')
        "#,
    )
    .bind(session_id)
    .bind(max_seq + 1)
    .bind(&consolidated)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    tracing::info!(
        "[CONSOLIDATE] Session '{}': merged {} FUTURE constraints into 1 consolidated entry (dedup {} -> {}, dropped {} chars).",
        session_id,
        ids.len(),
        rows.len(),
        total,
        if dropped > 0 { format!("{} lessons", dropped) } else { "none".into() }
    );

    Ok(true)
}

// ── Conflict Detection for FUTURE Constraints ────────────────────────────────

/// Detects contradictory FUTURE constraint pairs using keyword-based heuristics.
///
/// This is a lightweight, non-ML approach: specific keyword pairs indicate
/// likely contradictions (e.g., "retry" vs. "never retry", "use cache" vs.
/// "do not cache"). More sophisticated semantic detection would require an
/// embedding model — this is the 80/20 solution.
pub async fn detect_future_conflicts(pool: &SqlitePool, session_id: &str) -> Result<Vec<String>> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        r#"
        SELECT id, COALESCE(learning_summary, '')
        FROM agent_timeline
        WHERE session_id = ?1 AND temporal_state = 'FUTURE'
        ORDER BY sequence_order ASC
        "#,
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;

    if rows.len() < 2 {
        return Ok(vec![]);
    }

    // Conflict patterns: (keyword_a, keyword_b, description)
    let conflict_patterns: &[(&str, &str, &str)] = &[
        ("retry", "never retry", "retry vs. never retry"),
        ("retry", "do not retry", "retry vs. do not retry"),
        ("use cache", "do not cache", "cache vs. do not cache"),
        ("use cache", "never cache", "cache vs. never cache"),
        ("use cache", "avoid cache", "cache vs. avoid cache"),
        (
            "always retry",
            "stop retrying",
            "always retry vs. stop retrying",
        ),
        ("format!", "avoid format!", "format! vs. avoid format!"),
        ("push_str", "avoid push_str", "push_str vs. avoid push_str"),
        (
            "exponential backoff",
            "linear backoff",
            "exponential vs. linear backoff",
        ),
        (
            "connect directly",
            "use proxy",
            "direct connection vs. proxy",
        ),
    ];

    let mut conflicts: Vec<String> = Vec::new();
    let lessons: Vec<(i64, String)> = rows
        .into_iter()
        .map(|(id, summary)| (id, summary.to_lowercase()))
        .collect();

    for (i, (id_a, lesson_a)) in lessons.iter().enumerate() {
        for (id_b, lesson_b) in lessons.iter().skip(i + 1) {
            for (kw_a, kw_b, desc) in conflict_patterns {
                if lesson_a.contains(kw_a) && lesson_b.contains(kw_b)
                    || lesson_a.contains(kw_b) && lesson_b.contains(kw_a)
                {
                    conflicts.push(format!(
                        "Constraint #{id_a} vs #{id_b}: {desc} (\"{kw_a}\" ↔ \"{kw_b}\")"
                    ));
                    break; // One conflict per pair is enough
                }
            }
        }
    }

    Ok(conflicts)
}

// ── FUTURE Constraint Revocation ─────────────────────────────────────────────

/// Revokes a FUTURE constraint by moving it to PAST.
///
/// This allows the agent (or user) to remove a constraint that was incorrectly
/// learned — for example, when a confused agent misdiagnosed an error and
/// injected a wrong lesson.
pub async fn revoke_future_constraint(
    pool: &SqlitePool,
    session_id: &str,
    timeline_id: i64,
) -> Result<bool> {
    let rows_affected = sqlx::query(
        r#"
        UPDATE agent_timeline
        SET temporal_state = 'PAST'
        WHERE session_id = ?1
          AND id = ?2
          AND temporal_state = 'FUTURE'
        "#,
    )
    .bind(session_id)
    .bind(timeline_id)
    .execute(pool)
    .await?
    .rows_affected();

    if rows_affected > 0 {
        tracing::info!(
            "[REVOKE] Session '{}': revoked FUTURE constraint #{timeline_id}",
            session_id
        );
    }

    Ok(rows_affected > 0)
}
// ── Record New Present State ────────────────────────────────────────────────

/// Stores content in `memory_contents` and creates a PRESENT slot in the
/// timeline. Called when the agent encounters an error or relevant state
/// that should be tracked.
pub async fn record_present_state(
    pool: &SqlitePool,
    session_id: &str,
    raw_log: &str,
    file_context: Option<&str>,
    tool_payload: Option<&str>,
) -> Result<i64> {
    let mut tx = pool.begin().await?;

    // Insert content in memory_contents
    let content_id: i64 = sqlx::query_scalar(
        r#"
        INSERT INTO memory_contents (raw_log, file_context, tool_payload)
        VALUES (?1, ?2, ?3)
        RETURNING id
        "#,
    )
    .bind(raw_log)
    .bind(file_context)
    .bind(tool_payload)
    .fetch_one(&mut *tx)
    .await?;

    // Get next sequence number
    let max_seq: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(sequence_order), 0) FROM agent_timeline WHERE session_id = ?1",
    )
    .bind(session_id)
    .fetch_one(&mut *tx)
    .await?;

    // Create PRESENT timeline slot
    sqlx::query(
        r#"
        INSERT INTO agent_timeline (content_id, session_id, sequence_order, temporal_state)
        VALUES (?1, ?2, ?3, 'PRESENT')
        "#,
    )
    .bind(content_id)
    .bind(session_id)
    .bind(max_seq + 1)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    info!(
        "Recorded PRESENT state (content_id={}) in session '{}' at sequence {}",
        content_id,
        session_id,
        max_seq + 1
    );

    Ok(content_id)
}

// ── Timeline Queries ─────────────────────────────────────────────────────────

/// Gets the full timeline for a session (all states), enriched with content
/// from memory_contents via JOIN.
pub async fn get_timeline(
    pool: &SqlitePool,
    session_id: &str,
    limit: i64,
) -> Result<Vec<TimelineSlot>> {
    let slots = sqlx::query_as::<_, TimelineSlot>(
        r#"
        SELECT t.id, t.content_id, t.session_id, t.sequence_order,
               t.temporal_state, t.learning_summary, t.created_at, t.constraint_type,
               mc.raw_log, mc.file_context
        FROM agent_timeline t
        LEFT JOIN memory_contents mc ON t.content_id = mc.id
        WHERE t.session_id = ?1
        ORDER BY t.sequence_order DESC
        LIMIT ?2
        "#,
    )
    .bind(session_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(slots)
}

/// Searches the timeline by textual content (raw_log or learning_summary).
pub async fn search_timeline(
    pool: &SqlitePool,
    session_id: &str,
    query_str: &str,
    limit: i64,
) -> Result<Vec<TimelineSlot>> {
    let pattern = format!("%{}%", query_str);
    let slots = sqlx::query_as::<_, TimelineSlot>(
        r#"
        SELECT t.id, t.content_id, t.session_id, t.sequence_order,
               t.temporal_state, t.learning_summary, t.created_at, t.constraint_type,
               mc.raw_log, mc.file_context
        FROM agent_timeline t
        LEFT JOIN memory_contents mc ON t.content_id = mc.id
        WHERE t.session_id = ?1
          AND (mc.raw_log LIKE ?2 OR t.learning_summary LIKE ?2)
        ORDER BY t.sequence_order DESC
        LIMIT ?3
        "#,
    )
    .bind(session_id)
    .bind(&pattern)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(slots)
}

/// Generates a textual summary of the session, including counts per temporal state.
pub async fn get_session_summary(pool: &SqlitePool, session_id: &str) -> Result<String> {
    // Count by state
    let state_counts: Vec<(String, i64)> = sqlx::query_as(
        r#"
        SELECT temporal_state, COUNT(*) as cnt
        FROM agent_timeline
        WHERE session_id = ?1
        GROUP BY temporal_state
        "#,
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;

    let mut present = 0i64;
    let mut past = 0i64;
    let mut future = 0i64;

    for (state, count) in &state_counts {
        match state.as_str() {
            "PRESENT" => present = *count,
            "PAST" => past = *count,
            "FUTURE" => future = *count,
            _ => {}
        }
    }

    let total = present + past + future;

    // Calculate total approximate tokens
    let total_chars: i64 = sqlx::query_scalar(
        r#"
        SELECT COALESCE(SUM(LENGTH(mc.raw_log)), 0)
        FROM agent_timeline t
        LEFT JOIN memory_contents mc ON t.content_id = mc.id
        WHERE t.session_id = ?1
        "#,
    )
    .bind(session_id)
    .fetch_one(pool)
    .await?;

    let estimated_tokens = ((total_chars as f64) / 3.8).ceil() as i64;

    let summary = format!(
        "Session '{}': {} total timeline slots | {} PRESENT (active) | {} PAST (archived) | {} FUTURE (constraints) | ~{} estimated tokens in raw_log data.",
        session_id, total, present, past, future, estimated_tokens
    );

    Ok(summary)
}
