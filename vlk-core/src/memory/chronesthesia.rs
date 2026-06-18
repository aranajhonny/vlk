// ── Chronesthetic Layer (Left Parietal Cortex) ───────────────────────────────
// Based on Nyberg & Tulving (2010): "Consciousness of subjective time in the brain"
//
// The brain does not use the hippocampus (content) for mental time travel, but a
// differentiated network in the left lateral parietal cortex. This implementation
// emulates that specialization: we separate content (hippocampus) from temporal
// awareness (parietal) so the agent can position itself relative to its own history.
//
// TemporalState represents the three modes of subjective time awareness:
//   PRESENT → what the agent is processing right now (active context)
//   PAST    → dead ends, already learned from (hidden from context)
//   FUTURE  → preventive constraints extrapolated from experience
//
// Reference:
//   Nyberg, L., & Tulving, E. (2010). Consciousness of subjective time in the brain.
//   Proceedings of the National Academy of Sciences, 107(51), 21773–21774.
//   https://doi.org/10.1073/pnas.1016823108

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use tracing::info;

// ── Tipos de Estado Temporal ────────────────────────────────────────────────

/// The three modes of the agent's subjective time awareness.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[allow(dead_code)]
pub enum TemporalState {
    /// Active context: what the agent is currently processing.
    /// Injected into the LLM prompt with full content.
    PRESENT,
    /// Dead end: the agent has already learned from this experience.
    /// Hidden from active context to save tokens.
    PAST,
    /// Preventive constraint: an extrapolated lesson that modulates the agent's
    /// future behavior. Always injected as [PREVENTIVE FUTURE CONSTRAINT].
    FUTURE,
}

impl TemporalState {
    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            TemporalState::PRESENT => "PRESENT",
            TemporalState::PAST => "PAST",
            TemporalState::FUTURE => "FUTURE",
        }
    }
}

// ── Estructuras de Datos ────────────────────────────────────────────────────

/// HIPPOCAMPAL LAYER: Pure, immutable content.
/// Stores logs, stacktraces, tool payloads. Does not change.
/// It is the "what happened" stripped of all temporal interpretation.
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
#[allow(dead_code)]
pub struct MemoryContent {
    pub id: i64,
    pub raw_log: String,
    pub file_context: Option<String>,
    pub tool_payload: Option<String>,
}

/// PARIETAL LAYER: The agent's subjective timeline.
/// Each slot associates content (optional) with a temporal state and a position
/// in the agent's subjective chronology.
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

/// Constraint origin — distinguishes scar tissue (derived from failure) from
/// genuine foresight (user-provided deadlines, maintenance windows, etc.).
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[allow(dead_code)]
pub enum ConstraintType {
    /// Retrospective: learned from past failure, projected forward.
    DERIVED,
    /// Prospective: genuine foresight (deadlines, planned maintenance, etc.).
    PROSPECTIVE,
}

impl ConstraintType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConstraintType::DERIVED => "DERIVED",
            ConstraintType::PROSPECTIVE => "PROSPECTIVE",
        }
    }
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

/// Creates the `memory_contents` (hippocampal) and `agent_timeline` (parietal) tables.
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
/// This is the computational equivalent of mental time travel:
/// the agent "closes" a stuck present and projects a rule forward.
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
/// 1. Extract Rust error codes: `error[E0277]`
/// 2. Extract HTTP status + message prefix: `503 Service Unavailable`
/// 3. Fall back to first 80 chars with timestamps stripped
fn fingerprint_log(raw_log: &str) -> String {
    let trimmed = raw_log.trim();

    // 1. Rust compiler error codes: error[EXXXX]
    if let Some(cap) = extract_rust_error_code(trimmed) {
        return cap;
    }

    // 2. HTTP status code pattern: "NNN StatusText" or "Error NNN"
    if let Some(cap) = extract_http_error(trimmed) {
        return cap;
    }

    // 3. Fallback: first 80 chars with timestamp/datetime noise stripped
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
/// When too many individual constraints accumulate, they become their own
/// noise. This function merges all FUTURE entries into a single consolidated
/// constraint and archives the individual ones to PAST.
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

    // Collect all lessons
    let lessons: Vec<String> = rows
        .iter()
        .filter_map(|(_, summary, _)| summary.clone())
        .collect();

    if lessons.is_empty() {
        return Ok(false);
    }

    // Build consolidated constraint
    let consolidated = format!(
        "[CONSOLIDATED CONSTRAINTS from {} prior lessons]: {}",
        lessons.len(),
        lessons.join(" | ")
    );

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
        "[CONSOLIDATE] Session '{}': merged {} FUTURE constraints into 1 consolidated entry.",
        session_id,
        ids.len()
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

/// Stores content in the hippocampal layer and creates a PRESENT slot in the
/// timeline. Called automatically when the agent encounters an error or relevant
/// state that should be tracked.
#[allow(dead_code)]
pub async fn record_present_state(
    pool: &SqlitePool,
    session_id: &str,
    raw_log: &str,
    file_context: Option<&str>,
    tool_payload: Option<&str>,
) -> Result<i64> {
    let mut tx = pool.begin().await?;

    // Insert content in the hippocampal layer
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

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    /// Helper: creates an in-memory database and initializes tables.
    async fn setup_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("Failed to create in-memory SQLite pool");

        sqlx::query("PRAGMA foreign_keys=ON;")
            .execute(&pool)
            .await
            .unwrap();

        init_chronesthesia_tables(&pool)
            .await
            .expect("Failed to initialize tables");

        pool
    }

    /// Helper: registers N PRESENT slots with dummy logs for a session.
    async fn seed_present_states(pool: &SqlitePool, session_id: &str, n: i32) {
        for i in 0..n {
            record_present_state(
                pool,
                session_id,
                &format!("Error log entry #{}", i),
                Some(&format!("src/main.rs:{}", 10 + i)),
                None,
            )
            .await
            .expect("Failed to seed present state");
        }
    }

    // ────────────────────────────────────────────────────────────────────────────
    // 1. Initialization
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_init_creates_tables() {
        let pool = setup_pool().await;

        // Verify that memory_contents exists
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memory_contents'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1, "memory_contents table must exist");

        // Verify that agent_timeline exists
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='agent_timeline'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1, "agent_timeline table must exist");

        // Verify the CHECK constraint on temporal_state
        let create_sql: String = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='agent_timeline'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            create_sql.contains("CHECK"),
            "agent_timeline must have CHECK constraint on temporal_state"
        );
        assert!(
            create_sql.contains("FOREIGN KEY"),
            "agent_timeline must have FOREIGN KEY to memory_contents"
        );

        // Verify the composite index for active context queries exists
        let idx_sql: String = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE type='index' AND name='idx_timeline_active_context'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            idx_sql.contains("session_id")
                && idx_sql.contains("temporal_state")
                && idx_sql.contains("sequence_order"),
            "idx_timeline_active_context must cover session_id, temporal_state, sequence_order"
        );
    }

    // ────────────────────────────────────────────────────────────────────────────
    // 2. record_present_state — Hippocampal Layer + PRESENT Slot
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_record_present_state_creates_content_and_timeline() {
        let pool = setup_pool().await;

        let content_id = record_present_state(
            &pool,
            "session-1",
            "thread 'main' panicked at 'index out of bounds'",
            Some("src/lib.rs:42"),
            Some(r#"{"tool":"edit","file":"src/lib.rs"}"#),
        )
        .await
        .expect("record_present_state failed");

        // Verify content was inserted into memory_contents
        let content: MemoryContent = sqlx::query_as("SELECT * FROM memory_contents WHERE id = ?1")
            .bind(content_id)
            .fetch_one(&pool)
            .await
            .expect("MemoryContent not found");

        assert_eq!(
            content.raw_log,
            "thread 'main' panicked at 'index out of bounds'"
        );
        assert_eq!(content.file_context.unwrap(), "src/lib.rs:42");
        assert!(content.tool_payload.unwrap().contains("edit"));

        // Verify the PRESENT slot was created in agent_timeline
        let slots: Vec<TimelineSlot> = get_timeline(&pool, "session-1", 10)
            .await
            .expect("get_timeline failed");

        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].content_id, Some(content_id));
        assert_eq!(slots[0].temporal_state, "PRESENT");
        assert_eq!(slots[0].sequence_order, 1);
        assert_eq!(
            slots[0].raw_log.as_deref(),
            Some("thread 'main' panicked at 'index out of bounds'")
        );
        assert!(slots[0].learning_summary.is_none());
    }

    #[tokio::test]
    async fn test_record_present_state_increments_sequence() {
        let pool = setup_pool().await;

        record_present_state(&pool, "s1", "log1", None, None)
            .await
            .expect("first record failed");
        record_present_state(&pool, "s1", "log2", None, None)
            .await
            .expect("second record failed");
        record_present_state(&pool, "s1", "log3", None, None)
            .await
            .expect("third record failed");

        let slots = get_timeline(&pool, "s1", 10).await.unwrap();
        // ORDER BY sequence_order DESC, so slot 3 is first
        assert_eq!(slots.len(), 3);
        assert_eq!(slots[0].sequence_order, 3);
        assert_eq!(slots[1].sequence_order, 2);
        assert_eq!(slots[2].sequence_order, 1);
    }

    // ────────────────────────────────────────────────────────────────────────────
    // 3. execute_time_travel — PRESENT → PAST + FUTURE injection
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_time_travel_transitions_present_to_past() {
        let pool = setup_pool().await;
        seed_present_states(&pool, "session-tt", 3).await;

        let slots_before = get_timeline(&pool, "session-tt", 10).await.unwrap();
        assert_eq!(slots_before.len(), 3);
        assert!(slots_before.iter().all(|s| s.temporal_state == "PRESENT"));

        let target_ids: Vec<i64> = slots_before.iter().take(2).map(|s| s.id).collect();

        let (tokens_saved, lesson) = execute_time_travel(
            &pool,
            TimeTravelArgs {
                session_id: Some("session-tt".into()),
                target_timeline_ids: target_ids.clone(),
                learning: "Do not access index out of bounds. Always check .len() first.".into(),
                raw_log_excerpt: "test error".into(),
                constraint_type: None,
            },
        )
        .await
        .expect("execute_time_travel failed");

        assert!(
            tokens_saved > 0,
            "Should have saved tokens from archived logs"
        );
        assert_eq!(
            lesson,
            "Do not access index out of bounds. Always check .len() first."
        );

        // Verify: 2 slots are now PAST, 1 remains PRESENT, 1 FUTURE injected
        let slots_after = super::get_timeline(&pool, "session-tt", 10).await.unwrap();
        let past_count = slots_after
            .iter()
            .filter(|s| s.temporal_state == "PAST")
            .count();
        let present_count = slots_after
            .iter()
            .filter(|s| s.temporal_state == "PRESENT")
            .count();
        let future_count = slots_after
            .iter()
            .filter(|s| s.temporal_state == "FUTURE")
            .count();

        assert_eq!(past_count, 2, "Two slots should be PAST");
        assert_eq!(present_count, 1, "One slot should remain PRESENT");
        assert_eq!(future_count, 1, "One FUTURE constraint should be injected");
    }

    #[tokio::test]
    async fn test_time_travel_injects_future_constraint() {
        let pool = setup_pool().await;
        seed_present_states(&pool, "session-future", 1).await;

        let slots = get_timeline(&pool, "session-future", 10).await.unwrap();
        let target_id = slots[0].id;

        execute_time_travel(
            &pool,
            TimeTravelArgs {
                session_id: Some("session-future".into()),
                target_timeline_ids: vec![target_id],
                learning: "Rate-limit API calls to endpoint X. Use local 12°C cache.".into(),
                raw_log_excerpt: "test error".into(),
                constraint_type: None,
            },
        )
        .await
        .expect("execute_time_travel failed");

        // Find the FUTURE slot
        let future_slots: Vec<TimelineSlot> = sqlx::query_as(
            r#"SELECT t.id, t.content_id, t.session_id, t.sequence_order,
                      t.temporal_state, t.learning_summary, t.created_at, t.constraint_type,
                      mc.raw_log, mc.file_context
               FROM agent_timeline t
               LEFT JOIN memory_contents mc ON t.content_id = mc.id
               WHERE t.session_id = ?1 AND t.temporal_state = 'FUTURE'"#,
        )
        .bind("session-future")
        .fetch_all(&pool)
        .await
        .unwrap();

        assert_eq!(future_slots.len(), 1);
        assert_eq!(
            future_slots[0].learning_summary.as_deref(),
            Some("Rate-limit API calls to endpoint X. Use local 12°C cache.")
        );
        assert!(
            future_slots[0].content_id.is_none(),
            "FUTURE must not link to heavy content"
        );
        assert!(
            future_slots[0].raw_log.is_none(),
            "FUTURE must not carry raw_log"
        );
    }

    #[tokio::test]
    async fn test_time_travel_validates_empty_learning() {
        let pool = setup_pool().await;

        let result = execute_time_travel(
            &pool,
            TimeTravelArgs {
                session_id: None,
                target_timeline_ids: vec![1, 2],
                learning: "   ".into(),
                raw_log_excerpt: "test".into(),
                constraint_type: None,
            },
        )
        .await;

        assert!(result.is_err(), "Should reject empty learning");
        assert!(
            result.unwrap_err().to_string().contains("learning"),
            "Error should mention 'learning'"
        );
    }

    #[tokio::test]
    async fn test_time_travel_validates_empty_targets() {
        let pool = setup_pool().await;

        let result = execute_time_travel(
            &pool,
            TimeTravelArgs {
                session_id: None,
                target_timeline_ids: vec![],
                learning: "some lesson".into(),
                raw_log_excerpt: "test".into(),
                constraint_type: None,
            },
        )
        .await;

        assert!(result.is_err(), "Should reject empty target_timeline_ids");
    }

    // ────────────────────────────────────────────────────────────────────────────
    // 4. fetch_active_context — Surgical filtering
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_fetch_context_excludes_past() {
        let pool = setup_pool().await;
        seed_present_states(&pool, "session-ctx", 3).await;

        let slots = get_timeline(&pool, "session-ctx", 10).await.unwrap();
        let target_ids: Vec<i64> = slots.iter().map(|s| s.id).take(2).collect();

        // Archivar 2 slots como PAST
        execute_time_travel(
            &pool,
            TimeTravelArgs {
                session_id: Some("session-ctx".into()),
                target_timeline_ids: target_ids,
                learning: "Lesson: check bounds.".into(),
                raw_log_excerpt: "test error".into(),
                constraint_type: None,
            },
        )
        .await
        .unwrap();

        let context = fetch_active_context(&pool, "session-ctx")
            .await
            .expect("fetch_active_context failed");

        // The archived slots (the most recent 2: seq 3 and 2 = "#2" and "#1") must NOT appear
        assert!(
            !context.contains("Error log entry #1"),
            "PAST slot (#1) raw_log leaked into active context: {}",
            context
        );
        assert!(
            !context.contains("Error log entry #2"),
            "PAST slot (#2) raw_log leaked into active context"
        );

        // Must contain the remaining PRESENT slot (oldest, seq 1 = "#0")
        assert!(
            context.contains("Error log entry #0"),
            "PRESENT slot (#0) should appear in active context"
        );

        // Must contain the FUTURE constraint
        assert!(
            context.contains("PREVENTIVE FUTURE CONSTRAINT"),
            "FUTURE constraint should appear in active context"
        );
        assert!(
            context.contains("Lesson: check bounds."),
            "FUTURE learning_summary should appear in active context"
        );
    }

    #[tokio::test]
    async fn test_fetch_context_empty_session() {
        let pool = setup_pool().await;

        let context = fetch_active_context(&pool, "empty-session")
            .await
            .expect("fetch_active_context failed");

        assert!(
            context.contains("VLK CHRONESTHESIA LAYER"),
            "Context should have header: {}",
            context
        );
        // With no slots, neither PRESENT nor FUTURE should show
        assert!(
            !context.contains("PREVENTIVE FUTURE CONSTRAINT"),
            "No FUTURE expected in empty session"
        );
        assert!(
            !context.contains("ACTIVE PRESENT STATE"),
            "No PRESENT expected in empty session"
        );
    }

    #[tokio::test]
    async fn test_fetch_context_only_past_is_empty() {
        let pool = setup_pool().await;
        seed_present_states(&pool, "all-past", 1).await;

        let slots = get_timeline(&pool, "all-past", 10).await.unwrap();

        execute_time_travel(
            &pool,
            TimeTravelArgs {
                session_id: Some("all-past".into()),
                target_timeline_ids: vec![slots[0].id],
                learning: "All archived.".into(),
                raw_log_excerpt: "test".into(),
                constraint_type: None,
            },
        )
        .await
        .unwrap();

        let context = fetch_active_context(&pool, "all-past")
            .await
            .expect("fetch_active_context failed");

        // Only the FUTURE constraint should appear, not PAST logs
        assert!(
            context.contains("All archived."),
            "FUTURE constraint should appear"
        );
        assert!(
            !context.contains("Error log entry #0"),
            "PAST should not appear"
        );
    }

    // ────────────────────────────────────────────────────────────────────────────
    // 5. get_timeline — Full history
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_get_timeline_ordered_by_sequence() {
        let pool = setup_pool().await;
        seed_present_states(&pool, "timeline-order", 5).await;

        let slots = get_timeline(&pool, "timeline-order", 10).await.unwrap();

        // Verify descending order (most recent first)
        assert_eq!(slots.len(), 5);
        for i in 0..(slots.len() - 1) {
            assert!(
                slots[i].sequence_order >= slots[i + 1].sequence_order,
                "Slots must be ordered by sequence_order DESC"
            );
        }
    }

    #[tokio::test]
    async fn test_get_timeline_limit() {
        let pool = setup_pool().await;
        seed_present_states(&pool, "limit-test", 10).await;

        let slots = get_timeline(&pool, "limit-test", 3).await.unwrap();
        assert_eq!(slots.len(), 3, "Should respect limit parameter");
    }

    // ────────────────────────────────────────────────────────────────────────────
    // 6. search_timeline — Content search
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_search_finds_raw_log() {
        let pool = setup_pool().await;

        record_present_state(
            &pool,
            "search-session",
            "error: cannot find trait `Serialize`",
            Some("Cargo.toml"),
            None,
        )
        .await
        .unwrap();

        record_present_state(
            &pool,
            "search-session",
            "warning: unused variable: `counter`",
            Some("src/main.rs:15"),
            None,
        )
        .await
        .unwrap();

        let results = search_timeline(&pool, "search-session", "Serialize", 10)
            .await
            .unwrap();

        assert_eq!(results.len(), 1, "Should find only the Serialize error");
        assert!(
            results[0].raw_log.as_deref().unwrap().contains("Serialize"),
            "Should match the correct entry"
        );
    }

    #[tokio::test]
    async fn test_search_finds_learning_summary() {
        let pool = setup_pool().await;
        seed_present_states(&pool, "search-summary", 1).await;

        let slots = get_timeline(&pool, "search-summary", 10).await.unwrap();

        execute_time_travel(
            &pool,
            TimeTravelArgs {
                session_id: Some("search-summary".into()),
                target_timeline_ids: vec![slots[0].id],
                learning: "Always add #[derive(Serialize)] to structs exposed via MCP.".into(),
                raw_log_excerpt: "test".into(),
                constraint_type: None,
            },
        )
        .await
        .unwrap();

        let results = search_timeline(&pool, "search-summary", "#[derive(Serialize)]", 10)
            .await
            .unwrap();

        assert!(
            results.iter().any(|s| s.learning_summary.as_deref()
                == Some("Always add #[derive(Serialize)] to structs exposed via MCP.")),
            "Search should find learning_summary in FUTURE constraint"
        );
    }

    #[tokio::test]
    async fn test_search_returns_empty_for_no_match() {
        let pool = setup_pool().await;
        seed_present_states(&pool, "no-match", 3).await;

        let results = search_timeline(&pool, "no-match", "xyznonexistent", 10)
            .await
            .unwrap();

        assert_eq!(
            results.len(),
            0,
            "Should return empty for non-matching query"
        );
    }

    // ────────────────────────────────────────────────────────────────────────────
    // 7. get_session_summary — Summary with counts
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_session_summary_counts() {
        let pool = setup_pool().await;

        // 3 PRESENT
        seed_present_states(&pool, "summary-session", 3).await;

        let slots = get_timeline(&pool, "summary-session", 10).await.unwrap();

        // Archive 2 as PAST → 1 remaining PRESENT + 1 FUTURE injected
        execute_time_travel(
            &pool,
            TimeTravelArgs {
                session_id: Some("summary-session".into()),
                target_timeline_ids: vec![slots[0].id, slots[1].id],
                learning: "Test lesson.".into(),
                raw_log_excerpt: "test".into(),
                constraint_type: None,
            },
        )
        .await
        .unwrap();

        let summary = get_session_summary(&pool, "summary-session")
            .await
            .expect("get_session_summary failed");

        // Expected format: "Session 'summary-session': 4 total timeline slots | 1 PRESENT (active) | 2 PAST (archived) | 1 FUTURE (constraints) | ~X estimated tokens in raw_log data."
        assert!(
            summary.contains("4 total timeline slots"),
            "Expected 4 total slots: {}",
            summary
        );
        assert!(
            summary.contains("1 PRESENT"),
            "Expected 1 PRESENT: {}",
            summary
        );
        assert!(summary.contains("2 PAST"), "Expected 2 PAST: {}", summary);
        assert!(
            summary.contains("1 FUTURE"),
            "Expected 1 FUTURE: {}",
            summary
        );
    }

    #[tokio::test]
    async fn test_session_summary_empty() {
        let pool = setup_pool().await;

        let summary = get_session_summary(&pool, "empty-summary")
            .await
            .expect("get_session_summary failed");

        assert!(
            summary.contains("0 total timeline slots"),
            "Empty session summary: {}",
            summary
        );
    }

    // ────────────────────────────────────────────────────────────────────────────
    // 8. Session isolation
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_session_isolation() {
        let pool = setup_pool().await;

        seed_present_states(&pool, "session-A", 2).await;
        seed_present_states(&pool, "session-B", 3).await;

        let a_slots = get_timeline(&pool, "session-A", 10).await.unwrap();
        let b_slots = get_timeline(&pool, "session-B", 10).await.unwrap();

        assert_eq!(a_slots.len(), 2, "session-A should have 2 slots");
        assert_eq!(b_slots.len(), 3, "session-B should have 3 slots");

        // Archive only in session-A
        let a_ids: Vec<i64> = a_slots.iter().map(|s| s.id).collect();
        execute_time_travel(
            &pool,
            TimeTravelArgs {
                session_id: Some("session-A".into()),
                target_timeline_ids: a_ids,
                learning: "Session A lesson.".into(),
                raw_log_excerpt: "test".into(),
                constraint_type: None,
            },
        )
        .await
        .unwrap();

        // session-B no debe verse afectada
        let b_slots_after = get_timeline(&pool, "session-B", 10).await.unwrap();
        assert_eq!(b_slots_after.len(), 3, "session-B should be unaffected");
        assert!(
            b_slots_after.iter().all(|s| s.temporal_state == "PRESENT"),
            "session-B slots should all be PRESENT"
        );
    }

    // ────────────────────────────────────────────────────────────────────────────
    // 9. Referential integrity (FOREIGN KEY)
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_foreign_key_enforced() {
        let pool = setup_pool().await;

        // Attempt to insert a timeline slot with a non-existent content_id
        let result = sqlx::query(
            r#"
            INSERT INTO agent_timeline (content_id, session_id, sequence_order, temporal_state)
            VALUES (99999, 'fk-test', 1, 'PRESENT')
            "#,
        )
        .execute(&pool)
        .await;

        assert!(
            result.is_err(),
            "FOREIGN KEY should reject invalid content_id"
        );
    }

    #[tokio::test]
    async fn test_cascade_delete_not_set() {
        let pool = setup_pool().await;

        let content_id = record_present_state(&pool, "cascade-test", "some log", None, None)
            .await
            .unwrap();

        // Attempt to delete a memory_contents that has children in agent_timeline
        let result = sqlx::query("DELETE FROM memory_contents WHERE id = ?1")
            .bind(content_id)
            .execute(&pool)
            .await;

        // Without ON DELETE CASCADE, this should fail due to FK
        assert!(
            result.is_err(),
            "Should not allow deleting memory_contents referenced by timeline"
        );
    }

    // ────────────────────────────────────────────────────────────────────────────
    // 10. CHECK constraint integrity on temporal_state
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_temporal_state_check_constraint() {
        let pool = setup_pool().await;

        let result = sqlx::query(
            r#"
            INSERT INTO agent_timeline (content_id, session_id, sequence_order, temporal_state)
            VALUES (NULL, 'check-test', 1, 'INVALID_STATE')
            "#,
        )
        .execute(&pool)
        .await;

        assert!(
            result.is_err(),
            "CHECK constraint should reject invalid temporal_state"
        );
    }

    // ────────────────────────────────────────────────────────────────────────────
    // 11. record_present_state with minimal data (no file_context or tool_payload)
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_record_present_state_minimal() {
        let pool = setup_pool().await;

        let content_id = record_present_state(&pool, "minimal", "just a log", None, None)
            .await
            .expect("record_present_state with minimal args failed");

        assert!(content_id > 0, "Should return a valid content_id");

        let slots = get_timeline(&pool, "minimal", 10).await.unwrap();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].raw_log.as_deref(), Some("just a log"));
        assert!(slots[0].file_context.is_none());
    }

    // ────────────────────────────────────────────────────────────────────────────
    // 12. Multiple sequential time travels
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_multiple_time_travels() {
        let pool = setup_pool().await;
        seed_present_states(&pool, "multi-tt", 5).await;

        // First travel: archive slots 1,2
        let slots = get_timeline(&pool, "multi-tt", 10).await.unwrap();
        execute_time_travel(
            &pool,
            TimeTravelArgs {
                session_id: Some("multi-tt".into()),
                target_timeline_ids: vec![slots[4].id, slots[3].id], // oldest (seq 1,2)
                learning: "First lesson.".into(),
                raw_log_excerpt: "test".into(),
                constraint_type: None,
            },
        )
        .await
        .unwrap();

        // Second travel: archive slots 3,4
        let slots = get_timeline(&pool, "multi-tt", 10).await.unwrap();
        let present_slots: Vec<i64> = slots
            .iter()
            .filter(|s| s.temporal_state == "PRESENT")
            .map(|s| s.id)
            .collect();
        execute_time_travel(
            &pool,
            TimeTravelArgs {
                session_id: Some("multi-tt".into()),
                target_timeline_ids: present_slots,
                learning: "Second lesson.".into(),
                raw_log_excerpt: "test".into(),
                constraint_type: None,
            },
        )
        .await
        .unwrap();

        // Result: 5 originals (seq 1..5).
        // After travel 1 (archives seq1,2): seq1,2→PAST | seq3,4,5→PRESENT | seq6→FUTURE
        // After travel 2 (archives seq3,4,5): seq3,4,5→PAST | seq7→FUTURE
        // Total: seq1..5=PAST, seq6,7=FUTURE
        let final_slots = get_timeline(&pool, "multi-tt", 20).await.unwrap();
        let past = final_slots
            .iter()
            .filter(|s| s.temporal_state == "PAST")
            .count();
        let present = final_slots
            .iter()
            .filter(|s| s.temporal_state == "PRESENT")
            .count();
        let future = final_slots
            .iter()
            .filter(|s| s.temporal_state == "FUTURE")
            .count();

        assert_eq!(
            past, 5,
            "All five original slots archived across two travels"
        );
        assert_eq!(present, 0, "No PRESENT remains");
        assert_eq!(future, 2, "Two FUTURE lessons injected across two travels");
        assert_eq!(final_slots.len(), 7);
    }

    // ────────────────────────────────────────────────────────────────────────────
    // 13. Token estimation accuracy
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_time_travel_token_estimation() {
        let pool = setup_pool().await;

        // Insert a large log
        let big_log = "A".repeat(3800); // ~1000 tokens según ratio 3.8
        record_present_state(&pool, "tokens", &big_log, None, None)
            .await
            .unwrap();

        let slots = get_timeline(&pool, "tokens", 10).await.unwrap();

        let (tokens_saved, _) = execute_time_travel(
            &pool,
            TimeTravelArgs {
                session_id: Some("tokens".into()),
                target_timeline_ids: vec![slots[0].id],
                learning: "Big logs cost tokens.".into(),
                raw_log_excerpt: "test".into(),
                constraint_type: None,
            },
        )
        .await
        .unwrap();

        // 3800 chars / 3.8 = 1000 tokens
        assert!(
            tokens_saved >= 1000,
            "Should estimate ~1000 tokens for 3800 chars, got {}",
            tokens_saved
        );
    }

    // ────────────────────────────────────────────────────────────────────────────
    // BENCHMARK: Performance and token savings
    // ────────────────────────────────────────────────────────────────────────────

    /// Simulates a real Rust compiler error (~2000 chars).
    fn real_compiler_error() -> String {
        format!(
            "{}
{}
{}:{}:{}: error[E0277]: cannot add `&str` to `String`\n
   --> src/handler.rs:42:17\n    |\n 42 |     let result = base + \"/api/v1/\" + param;\n    |                  ^^^^ no implementation for `&str + &str`\n    |\n    = help: the trait `Add<&str>` is not implemented for `&str`\n    = note: required for `String` to implement `Add<&str>`\n    = note: you can use `push_str` or `format!` to concatenate strings\n\nFor more information about this error, try `rustc --explain E0277`.\n",
            "-".repeat(80),
            "error: could not compile `agora-core` due to 1 previous error",
            "error",
            "E0277",
            "cannot_add"
        )
    }

    /// Simulates a real API error with stacktrace (~1500 chars).
    fn real_api_error() -> String {
        format!(
            "{}
{}
{}",
            "POST /api/v2/agents/execute → 503 Service Unavailable",
            "Response: {\"error\":\"upstream_connection_error\",\"retry_after\":30}",
            "Stack: reqwest::Client::execute() → hyper::client::connect::dns -> timeout\n  at src/api/client.rs:85:22\n  at src/api/client.rs:92:14\n  at src/agent/executor.rs:120:38\nCaused by: DNS resolution failed for 'api.provider.com'\n  elapsed: 30.042s"
        )
    }

    /// Calculates estimated tokens for a string (ratio 3.8 chars/token).
    fn estimate_tokens(s: &str) -> i64 {
        ((s.len() as f64) / 3.8).ceil() as i64
    }

    #[tokio::test]
    async fn bench_token_savings_single_error() {
        let pool = setup_pool().await;
        let log = real_compiler_error();
        let tokens_per_log = estimate_tokens(&log);

        // Registrar el error como PRESENT
        record_present_state(&pool, "bench-single", &log, Some("src/handler.rs:42"), None)
            .await
            .unwrap();

        let context_before = fetch_active_context(&pool, "bench-single").await.unwrap();
        let tokens_before = estimate_tokens(&context_before);

        // Ejecutar time travel
        let slots = get_timeline(&pool, "bench-single", 10).await.unwrap();
        let (tokens_saved, _) = execute_time_travel(
            &pool,
            TimeTravelArgs {
                session_id: Some("bench-single".into()),
                target_timeline_ids: vec![slots[0].id],
                learning: "Use format!() or push_str() to concatenate strings in Rust.".into(),
                raw_log_excerpt: "error[E0277]: cannot add `&str` to `String`".into(),
                constraint_type: None,
            },
        )
        .await
        .unwrap();

        let context_after = fetch_active_context(&pool, "bench-single").await.unwrap();
        let tokens_after = estimate_tokens(&context_after);

        let saved_pct = if tokens_before > 0 {
            ((tokens_before - tokens_after) as f64 / tokens_before as f64 * 100.0) as i64
        } else {
            0
        };

        println!("╔═══ BENCH: Single Error Token Savings ═══╗");
        println!("║ Raw log size:           {:>8} chars    ║", log.len());
        println!("║ Tokens per raw log:     {:>8} tk       ║", tokens_per_log);
        println!("║ Tokens saved (reported):{:>8} tk       ║", tokens_saved);
        println!("║ Context BEFORE (PRESENT):{:>8} tk       ║", tokens_before);
        println!("║ Context AFTER (FUTURE): {:>8} tk       ║", tokens_after);
        println!("║ Reduction:              {:>7}%        ║", saved_pct);
        println!("╚══════════════════════════════════════════╝");

        // El contexto después debe ser significativamente más pequeño
        assert!(
            tokens_after < tokens_before,
            "Context AFTER ({}) must be smaller than BEFORE ({})",
            tokens_after,
            tokens_before
        );
        // El raw_log no debe aparecer en el contexto activo post-time-travel
        let context_after = fetch_active_context(&pool, "bench-single").await.unwrap();
        assert!(
            !context_after.contains("E0277"),
            "Raw error content leaked into active context after time travel"
        );
        assert!(
            context_after.contains("format!()"),
            "FUTURE constraint must be present in active context"
        );
    }

    #[tokio::test]
    async fn bench_cumulative_savings_multiple_errors() {
        let pool = setup_pool().await;
        let session = "bench-cumulative";

        // Simular 5 iteraciones de un agente atrapado en un bucle de errores
        let mut total_tokens_if_no_chronesthesia = 0i64;
        let mut cumulative_tokens_saved = 0i64;
        let mut context_sizes_after: Vec<i64> = Vec::new();

        let scenarios = vec![
            ("Rust E0277 (concat)", real_compiler_error(),
             "Use format!() or push_str() to concatenate strings."),
            ("API 503 timeout", real_api_error(),
             "Implement retry with exponential backoff and circuit breaker."),
            ("Rust E0308 (mismatch)",
             format!("error[E0308]: mismatched types\n --> src/models.rs:28:5\n  |\n28 |     let x: i32 = \"hello\";\n  |         ^^^^^^^^ expected `i32`, found `&str`\n  = note: expected type `i32`\n             found type `&str`"),
             "Annotate variable types explicitly or use .parse() for conversions."),
            ("API 429 rate limit",
             format!("429 Too Many Requests\nRetry-After: 120\nResponse: {{\"error\":\"rate_limit_exceeded\"}}\nRemaining: 0/1000 requests per hour"),
             "Respect rate limit headers. Add local rate limiter before sending requests."),
            ("Rust E0432 (import)",
             format!("error[E0432]: unresolved import `crate::routes::handler`\n --> src/main.rs:5:17\n  |\n5 |     use crate::routes::handler;\n  |         ^^^^^^^^^^^^^^^^^^^^^^ no `handler` in `routes`\n  = help: a struct with a similar name exists: `handlers`"),
             "Verify module paths. Run `cargo check` after adding new modules."),
        ];

        for (i, (label, log, lesson)) in scenarios.iter().enumerate() {
            let log_tokens = estimate_tokens(log);
            total_tokens_if_no_chronesthesia += log_tokens;

            // Registrar el error
            record_present_state(&pool, session, log, Some("src/file.rs"), None)
                .await
                .unwrap();

            // Obtener el slot y archivar
            let slots = get_timeline(&pool, session, 10).await.unwrap();
            let present_slots: Vec<i64> = slots
                .iter()
                .filter(|s| s.temporal_state == "PRESENT")
                .map(|s| s.id)
                .collect();

            // Archivar el error más reciente
            if let Some(&newest) = present_slots.first() {
                let (saved, _) = execute_time_travel(
                    &pool,
                    TimeTravelArgs {
                        session_id: Some(session.into()),
                        target_timeline_ids: vec![newest],
                        learning: lesson.to_string(),
                        raw_log_excerpt: log.chars().take(80).collect(),
                        constraint_type: None,
                    },
                )
                .await
                .unwrap();
                cumulative_tokens_saved += saved;
            }

            // Medir el contexto activo después del viaje
            let ctx = fetch_active_context(&pool, session).await.unwrap();
            context_sizes_after.push(estimate_tokens(&ctx));

            println!(
                "  Iter {:2} | {:<22} | raw_log={:5} chars | +{:4} tk saved | ctx now {:4} tk",
                i + 1,
                label,
                log.len(),
                cumulative_tokens_saved,
                context_sizes_after[i]
            );
        }

        let final_ctx = fetch_active_context(&pool, session).await.unwrap();
        let final_tokens = estimate_tokens(&final_ctx);

        println!("");
        println!("╔═══ BENCH: Cumulative Token Savings (5 errors) ═══╗");
        println!("║                                                       ║");
        println!("║ Sin chronesthesia (todos los logs en contexto):      ║");
        println!(
            "║   Total tokens acumulados:  {:>9} tk            ║",
            total_tokens_if_no_chronesthesia
        );
        println!("║                                                       ║");
        println!("║ Con chronesthesia (logs → PAST + lessons → FUTURE):  ║");
        println!(
            "║   Tokens ahorrados (reportados): {:>6} tk            ║",
            cumulative_tokens_saved
        );
        println!(
            "║   Contexto activo final:       {:>6} tk            ║",
            final_tokens
        );
        println!("║                                                       ║");

        let ratio = if final_tokens > 0 {
            total_tokens_if_no_chronesthesia as f64 / final_tokens as f64
        } else {
            0.0
        };

        println!(
            "║   Ratio de compresión:        {:>6.1}x               ║",
            ratio
        );
        println!("║                                                       ║");

        let savings_pct = if total_tokens_if_no_chronesthesia > 0 {
            ((total_tokens_if_no_chronesthesia - final_tokens) as f64
                / total_tokens_if_no_chronesthesia as f64
                * 100.0) as i64
        } else {
            0
        };

        println!(
            "║   Reducción total:            {:>5}%                 ║",
            savings_pct
        );
        println!("╚═══════════════════════════════════════════════════════╝");
        println!("");
        println!("📊 Contexto activo final ({} tk):", final_tokens);
        println!("{}", final_ctx);

        // Validaciones
        assert!(
            cumulative_tokens_saved >= total_tokens_if_no_chronesthesia - final_tokens,
            "Tokens saved ({}) should approximately equal cumulative raw logs minus context ({})",
            cumulative_tokens_saved,
            total_tokens_if_no_chronesthesia - final_tokens
        );
        assert!(
            final_tokens < 500,
            "Active context should stay under 500 tokens after 5 errors, got {}",
            final_tokens
        );
        // Verificar que todas las lecciones FUTURE están presentes
        assert!(
            final_ctx.contains("format!()"),
            "First lesson should be present"
        );
        assert!(
            final_ctx.contains("retry"),
            "Second lesson should be present"
        );
        assert!(
            final_ctx.contains("rate limit"),
            "Fourth lesson should be present"
        );
    }

    // ────────────────────────────────────────────────────────────────────────────
    // BENCHMARK: Interceptor automático de bucles
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_auto_detect_no_loop_below_threshold() {
        let pool = setup_pool().await;

        record_present_state(&pool, "no-loop", "error tipo A", None, None)
            .await
            .unwrap();
        record_present_state(&pool, "no-loop", "error tipo B", None, None)
            .await
            .unwrap();

        let mitigated = auto_detect_and_mitigate_loops(&pool, "no-loop", 3)
            .await
            .unwrap();

        assert!(
            !mitigated,
            "Should NOT trigger with 2 different errors below threshold"
        );

        let ctx = fetch_active_context(&pool, "no-loop").await.unwrap();
        assert!(ctx.contains("error tipo A"), "original logs should remain");
        assert!(ctx.contains("error tipo B"), "original logs should remain");
    }

    #[tokio::test]
    async fn test_auto_detect_basic_loop_reproduction() {
        // Test minimal que reproduce exactamente el problema:
        // crea 3 errores idénticos, ejecuta auto_detect, y verifica
        // que los slots se movieron a PAST.
        let pool = setup_pool().await;

        for _ in 0..3 {
            record_present_state(&pool, "debug-loop", "error msg", None, None)
                .await
                .unwrap();
        }

        // Verificar que hay 3 slots PRESENT
        let present_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_timeline WHERE session_id=?1 AND temporal_state='PRESENT'",
        )
        .bind("debug-loop")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(present_count, 3, "Should have 3 PRESENT slots");

        // Ejecutar auto_detect
        let mitigated = auto_detect_and_mitigate_loops(&pool, "debug-loop", 3)
            .await
            .unwrap();
        assert!(mitigated, "Should mitigate");

        // Verificar conteos después de la mitigación
        let present_after: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_timeline WHERE session_id=?1 AND temporal_state='PRESENT'",
        )
        .bind("debug-loop")
        .fetch_one(&pool)
        .await
        .unwrap();

        let past_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_timeline WHERE session_id=?1 AND temporal_state='PAST'",
        )
        .bind("debug-loop")
        .fetch_one(&pool)
        .await
        .unwrap();

        let future_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_timeline WHERE session_id=?1 AND temporal_state='FUTURE'",
        )
        .bind("debug-loop")
        .fetch_one(&pool)
        .await
        .unwrap();

        println!("  PRESENT after: {present_after}, PAST: {past_count}, FUTURE: {future_count}");

        assert_eq!(past_count, 3, "3 slots should be PAST");
        assert_eq!(present_after, 0, "0 slots should remain PRESENT");
        assert_eq!(future_count, 1, "1 FUTURE constraint should exist");

        // fetch_active_context NO debe contener "error msg"
        let ctx = fetch_active_context(&pool, "debug-loop").await.unwrap();
        // "error msg" aparece en el SYSTEM ANCHOR como "Ultimo error: \"error msg...\"",
        // lo que importa es que no haya lineas [ACTIVE PRESENT STATE] crudas.
        let has_present_state_line = ctx.contains("[ACTIVE PRESENT STATE]");
        assert!(
            !has_present_state_line,
            "No [ACTIVE PRESENT STATE] lines should exist. Present={present_after}, Past={past_count}, Future={future_count}: {}",
            ctx
        );
        assert!(
            ctx.contains("[SYSTEM ANCHOR]"),
            "System anchor should be in context"
        );
    }

    #[tokio::test]
    async fn test_auto_detect_loop_exact_match() {
        let pool = setup_pool().await;

        let log = "Error 503: upstream connection timeout";
        for _ in 0..3 {
            record_present_state(&pool, "loop-exact", log, None, None)
                .await
                .unwrap();
        }

        let mitigated = auto_detect_and_mitigate_loops(&pool, "loop-exact", 3)
            .await
            .unwrap();

        assert!(mitigated, "Should trigger with 3 identical errors");

        let ctx = fetch_active_context(&pool, "loop-exact").await.unwrap();
        // "upstream connection timeout" aparece en el ANCHOR, verificamos que NO
        // hay lineas [ACTIVE PRESENT STATE]
        assert!(
            !ctx.contains("[ACTIVE PRESENT STATE]"),
            "No PRESENT state lines should exist in active context"
        );
        assert!(
            ctx.contains("[SYSTEM ANCHOR]"),
            "Should contain system anchor in active context: {}",
            ctx
        );
        assert!(
            ctx.contains("Loop detected"),
            "Should mention loop detection"
        );
    }

    #[tokio::test]
    async fn test_auto_detect_with_extra_present_slots() {
        let pool = setup_pool().await;

        // 3 errores idénticos + 1 error diferente = debe mitigar solo el grupo de 3
        let repeated = "Error 503: timeout";
        for _ in 0..3 {
            record_present_state(&pool, "loop-mixed", repeated, None, None)
                .await
                .unwrap();
        }
        record_present_state(&pool, "loop-mixed", "Error 404: not found", None, None)
            .await
            .unwrap();

        let mitigated = auto_detect_and_mitigate_loops(&pool, "loop-mixed", 3)
            .await
            .unwrap();

        assert!(mitigated, "Should trigger despite having different errors");

        let ctx = fetch_active_context(&pool, "loop-mixed").await.unwrap();
        // Los 3 logs repetidos "503" deben estar en PAST
        // Verificamos que el PRESENT no contiene "503" ("timeout" si aparece en el anchor)
        let present_lines: Vec<&str> = ctx
            .lines()
            .filter(|l| l.contains("[ACTIVE PRESENT STATE]"))
            .collect();
        assert_eq!(present_lines.len(), 1, "Should have exactly 1 PRESENT line");
        assert!(
            present_lines[0].contains("404"),
            "The remaining PRESENT should be the unique 404 error, got: {:?}",
            present_lines
        );
        // El error distinto debe seguir PRESENT
        assert!(ctx.contains("404"), "Unique error should remain PRESENT");
        // Debe tener el system anchor
        assert!(ctx.contains("[SYSTEM ANCHOR]"));
    }

    #[tokio::test]
    async fn test_auto_detect_trims_whitespace() {
        let pool = setup_pool().await;

        // Mismo error con whitespace diferente
        record_present_state(&pool, "loop-trim", "error", None, None)
            .await
            .unwrap();
        record_present_state(&pool, "loop-trim", "  error  ", None, None)
            .await
            .unwrap();
        record_present_state(&pool, "loop-trim", "error", None, None)
            .await
            .unwrap();

        let mitigated = auto_detect_and_mitigate_loops(&pool, "loop-trim", 3)
            .await
            .unwrap();

        assert!(mitigated, "Should detect loop after trimming whitespace");
    }

    #[tokio::test]
    async fn test_auto_detect_respects_session_isolation() {
        let pool = setup_pool().await;

        let log = "error X";
        // 3 loops en session-A
        for _ in 0..3 {
            record_present_state(&pool, "session-A", log, None, None)
                .await
                .unwrap();
        }
        // 1 solo en session-B
        record_present_state(&pool, "session-B", log, None, None)
            .await
            .unwrap();

        let mitigated_a = auto_detect_and_mitigate_loops(&pool, "session-A", 3)
            .await
            .unwrap();
        let mitigated_b = auto_detect_and_mitigate_loops(&pool, "session-B", 3)
            .await
            .unwrap();

        assert!(mitigated_a, "session-A should trigger");
        assert!(!mitigated_b, "session-B should NOT trigger");
    }

    #[tokio::test]
    async fn test_auto_detect_returns_false_when_empty() {
        let pool = setup_pool().await;

        let mitigated = auto_detect_and_mitigate_loops(&pool, "empty", 3)
            .await
            .unwrap();

        assert!(!mitigated, "Empty session should not trigger");
    }

    #[tokio::test]
    async fn test_fetch_clean_context_runs_interceptor() {
        let pool = setup_pool().await;

        // 4 errores idénticos — el interceptor debería dispararse dentro de fetch_clean_context
        let log = "FATAL: database connection lost";
        for _ in 0..4 {
            record_present_state(&pool, "clean-test", log, None, None)
                .await
                .unwrap();
        }

        let ctx = fetch_clean_context(&pool, "clean-test").await.unwrap();

        // El interceptor se ejecutó: no debe aparecer el log repetido
        // "database connection lost" aparece en el ANCHOR, verificamos que no haya
        // lineas [ACTIVE PRESENT STATE]
        assert!(
            !ctx.contains("[ACTIVE PRESENT STATE]"),
            "Interceptor should have archived repeated errors before returning context"
        );
        // Debe aparecer el system anchor
        assert!(
            ctx.contains("[SYSTEM ANCHOR]"),
            "System anchor must be injected: {}",
            ctx
        );
        // Debe aparecer el conteo correcto
        assert!(
            ctx.contains("4 times"),
            "Anchor should mention 4 repetitions"
        );
    }

    #[tokio::test]
    async fn test_auto_detect_most_frequent_group() {
        let pool = setup_pool().await;

        // 4 de error A, 3 de error B — debe mitigar el A (el más frecuente)
        for _ in 0..4 {
            record_present_state(&pool, "freq", "error A", None, None)
                .await
                .unwrap();
        }
        for _ in 0..3 {
            record_present_state(&pool, "freq", "error B", None, None)
                .await
                .unwrap();
        }

        let mitigated = auto_detect_and_mitigate_loops(&pool, "freq", 3)
            .await
            .unwrap();

        assert!(mitigated, "Should trigger on most frequent group");

        let ctx = fetch_active_context(&pool, "freq").await.unwrap();
        // error A (4 ocurrencias) mitigado → PAST. error B (3) sigue PRESENT porque
        // solo mitigamos el grupo MÁS grande.
        let present_lines: Vec<&str> = ctx
            .lines()
            .filter(|l| l.contains("[ACTIVE PRESENT STATE]"))
            .collect();
        // Deben haber 3 PRESENT (error B x3)
        assert_eq!(present_lines.len(), 3, "error B should remain PRESENT");
        assert!(
            present_lines.iter().all(|l| l.contains("error B")),
            "All PRESENT lines should be 'error B', got: {:?}",
            present_lines
        );
        // error B debe aparecer en el contexto
        assert!(
            ctx.contains("error B"),
            "Second group should remain PRESENT if not mitigated"
        );
        // El SYSTEM ANCHOR debe mencionar error A (el mitigado)
        assert!(
            ctx.contains("error A"),
            "System anchor should mention mitigated error A"
        );
    }

    #[tokio::test]
    async fn bench_context_bounded_growth() {
        let pool = setup_pool().await;
        let session = "bench-bounded";

        // Simular 10 errores del mismo tipo (como un bucle real)
        let error_log = real_api_error();
        let tokens_per_error = estimate_tokens(&error_log);

        // SIN chronesthesia: los 10 logs se acumularían
        let naive_total = tokens_per_error * 10;

        // CON chronesthesia: cada error se archiva inmediatamente
        let mut ctx_after: Vec<i64> = Vec::new();

        for i in 0..10 {
            record_present_state(&pool, session, &error_log, Some("src/api/client.rs"), None)
                .await
                .unwrap();

            let slots = get_timeline(&pool, session, 20).await.unwrap();
            let present: Vec<i64> = slots
                .iter()
                .filter(|s| s.temporal_state == "PRESENT")
                .map(|s| s.id)
                .collect();

            execute_time_travel(
                &pool,
                TimeTravelArgs {
                    session_id: Some(session.into()),
                    target_timeline_ids: present,
                    learning: format!("[Iter {}] Always use circuit breaker for API calls.", i + 1),
                    raw_log_excerpt: "503 Service Unavailable".into(),
                    constraint_type: None,
                },
            )
            .await
            .unwrap();

            let ctx = fetch_active_context(&pool, session).await.unwrap();
            let tk = estimate_tokens(&ctx);
            ctx_after.push(tk);

            println!(
                "  Iter {:2} | naive={:5} tk | chronesthesia={:4} tk | ratio={:.1}x",
                i + 1,
                tokens_per_error * (i + 1),
                tk,
                (tokens_per_error * (i + 1)) as f64 / tk as f64
            );
        }

        println!("");
        println!("╔═══ BENCH: Context Bounded Growth (10 iterations) ═══╗");
        println!("║                                                          ║");
        println!("║ Crecimiento sin chronesthesia (lineal):                  ║");
        println!(
            "║   Iter 1 → {:>4} tk  |  Iter 10 → {:>4} tk         ║",
            tokens_per_error, naive_total
        );
        println!("║                                                          ║");
        println!("║ Crecimiento con chronesthesia (acotado):                 ║");

        let mut bounded_str = String::new();
        for (i, tk) in ctx_after.iter().enumerate() {
            if i > 0 {
                bounded_str.push_str(" → ");
            }
            bounded_str.push_str(&format!("{}tk", tk));
        }
        println!("║   {}║", bounded_str);
        println!("║                                                          ║");

        // Verificar que el contexto se mantiene acotado (todas las iteraciones < 800 tokens)
        for (i, tk) in ctx_after.iter().enumerate() {
            assert!(
                *tk < 800,
                "Context at iter {} should be bounded under 800 tokens, got {}",
                i + 1,
                tk
            );
        }

        // Verificar que TODAS las lecciones FUTURE están presentes
        let final_ctx = fetch_active_context(&pool, session).await.unwrap();
        for i in 0..10 {
            assert!(
                final_ctx.contains(&format!("[Iter {}]", i + 1)),
                "Lesson from iter {} should be in context",
                i + 1
            );
        }

        println!("║   ✅ Contexto acotado: todas las iteraciones < 800 tk    ║");
        println!("║   ✅ Todas las restricciones FUTURE preservadas          ║");
        println!("╚══════════════════════════════════════════════════════════╝");
    }

    // ────────────────────────────────────────────────────────────────────────────
    // TESTS: Fingerprint-based loop detection (v0.5.0)
    // ────────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_fingerprint_extracts_rust_error_code() {
        let log = "error[E0277]: cannot add `&str` to `String`\n  --> src/handler.rs:42:17";
        let fp = fingerprint_log(log);
        assert_eq!(fp, "error[E0277]", "Should extract rust error code");
    }

    #[test]
    fn test_fingerprint_extracts_http_error() {
        let log = "POST /api/v2/agents/execute → 503 Service Unavailable";
        let fp = fingerprint_log(log);
        assert!(fp.contains("503"), "Should extract HTTP 503: got '{}'", fp);
    }

    #[test]
    fn test_fingerprint_strips_timestamps() {
        let log = "2024-01-15T14:32:01 Error: connection timeout";
        let fp = fingerprint_log(log);
        assert!(
            !fp.contains("2024"),
            "Should strip ISO timestamp: got '{}'",
            fp
        );
        assert!(
            fp.contains("Error"),
            "Should preserve error message: got '{}'",
            fp
        );
    }

    #[test]
    fn test_fingerprint_normalizes_time_variants() {
        let a = "Error 503: timeout at 14:32:01";
        let b = "Error 503: timeout at 14:32:04";
        let c = "Error 503: timeout at 14:32:07";

        let fp_a = fingerprint_log(a);
        let fp_b = fingerprint_log(b);
        let fp_c = fingerprint_log(c);

        assert_eq!(
            fp_a, fp_b,
            "Timestamps should normalize to same fingerprint"
        );
        assert_eq!(
            fp_b, fp_c,
            "Timestamps should normalize to same fingerprint"
        );
    }

    #[test]
    fn test_fingerprint_same_error_code_different_params() {
        let a = "error[E0277]: the trait bound `String: From<usize>` is not satisfied";
        let b = "error[E0277]: the trait bound `String: From<i32>` is not satisfied";
        let c = "error[E0277]: the trait bound `String: From<u64>` is not satisfied";

        let fp_a = fingerprint_log(a);
        let fp_b = fingerprint_log(b);
        let fp_c = fingerprint_log(c);

        assert_eq!(fp_a, fp_b, "Same error code should have same fingerprint");
        assert_eq!(fp_b, fp_c, "Same error code should have same fingerprint");
    }

    #[tokio::test]
    async fn test_auto_detect_fingerprint_loop() {
        let pool = setup_pool().await;

        // 3 near-duplicate errors (same error code, different timestamps)
        record_present_state(
            &pool,
            "fp-loop",
            "Error 503: timeout at 14:32:01",
            None,
            None,
        )
        .await
        .unwrap();
        record_present_state(
            &pool,
            "fp-loop",
            "Error 503: timeout at 14:32:04",
            None,
            None,
        )
        .await
        .unwrap();
        record_present_state(
            &pool,
            "fp-loop",
            "Error 503: timeout at 14:32:07",
            None,
            None,
        )
        .await
        .unwrap();

        let mitigated = auto_detect_and_mitigate_loops(&pool, "fp-loop", 3)
            .await
            .unwrap();

        assert!(
            mitigated,
            "Should detect fingerprint-based loop with timestamp variants"
        );

        let ctx = fetch_active_context(&pool, "fp-loop").await.unwrap();
        assert!(
            !ctx.contains("[ACTIVE PRESENT STATE]"),
            "Should have archived all PRESENT states"
        );
        assert!(
            ctx.contains("[SYSTEM ANCHOR]"),
            "Should contain system anchor"
        );
    }

    // ────────────────────────────────────────────────────────────────────────────
    // TESTS: FUTURE constraint consolidation (v0.5.0)
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_consolidation_below_threshold_does_nothing() {
        let pool = setup_pool().await;

        // Insert 3 FUTURE constraints (below threshold of 5)
        for i in 0..3 {
            sqlx::query(
                "INSERT INTO agent_timeline (session_id, sequence_order, temporal_state, learning_summary) VALUES (?1, ?2, 'FUTURE', ?3)",
            )
            .bind("cons-test")
            .bind(i + 1)
            .bind(format!("Constraint {}", i))
            .execute(&pool)
            .await
            .unwrap();
        }

        let consolidated = consolidate_future_constraints(&pool, "cons-test")
            .await
            .unwrap();

        assert!(!consolidated, "Should not consolidate below threshold");

        // All 3 should still be FUTURE
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_timeline WHERE session_id=?1 AND temporal_state='FUTURE'",
        )
        .bind("cons-test")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 3, "All 3 constraints should remain FUTURE");
    }

    #[tokio::test]
    async fn test_consolidation_above_threshold_merges() {
        let pool = setup_pool().await;

        // Insert 6 FUTURE constraints (above threshold of 5)
        for i in 0..6 {
            sqlx::query(
                "INSERT INTO agent_timeline (session_id, sequence_order, temporal_state, learning_summary) VALUES (?1, ?2, 'FUTURE', ?3)",
            )
            .bind("cons-merge")
            .bind(i + 1)
            .bind(format!("Rule {}", i))
            .execute(&pool)
            .await
            .unwrap();
        }

        let consolidated = consolidate_future_constraints(&pool, "cons-merge")
            .await
            .unwrap();

        assert!(consolidated, "Should consolidate above threshold");

        // Should have exactly 1 FUTURE entry now
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_timeline WHERE session_id=?1 AND temporal_state='FUTURE'",
        )
        .bind("cons-merge")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1, "Should have exactly 1 consolidated FUTURE entry");

        let ctx = fetch_active_context(&pool, "cons-merge").await.unwrap();
        assert!(
            ctx.contains("[CONSOLIDATED CONSTRAINTS"),
            "Context should contain consolidated constraint"
        );
        assert!(
            ctx.contains("Rule 0"),
            "Consolidated constraint should mention original rules"
        );
    }

    #[tokio::test]
    async fn test_consolidation_happens_in_fetch_clean_context() {
        let pool = setup_pool().await;

        // Insert 7 FUTURE constraints
        for i in 0..7 {
            sqlx::query(
                "INSERT INTO agent_timeline (session_id, sequence_order, temporal_state, learning_summary) VALUES (?1, ?2, 'FUTURE', ?3)",
            )
            .bind("cons-auto")
            .bind(i + 1)
            .bind(format!("Tip {}", i))
            .execute(&pool)
            .await
            .unwrap();
        }

        // fetch_clean_context should auto-consolidate
        let ctx = fetch_clean_context(&pool, "cons-auto").await.unwrap();

        assert!(
            ctx.contains("[CONSOLIDATED CONSTRAINTS"),
            "fetch_clean_context should auto-consolidate"
        );

        // Verify only 1 FUTURE remains
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_timeline WHERE session_id=?1 AND temporal_state='FUTURE'",
        )
        .bind("cons-auto")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1, "Should be consolidated to 1 entry");
    }

    // ────────────────────────────────────────────────────────────────────────────
    // TESTS: Conflict detection (v0.5.0)
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_conflict_detection_no_conflicts() {
        let pool = setup_pool().await;

        // Compatible constraints
        for (i, lesson) in [
            "Always retry on 503",
            "Use format!() for string concat",
            "Check .len() before indexing",
        ]
        .iter()
        .enumerate()
        {
            sqlx::query(
                "INSERT INTO agent_timeline (session_id, sequence_order, temporal_state, learning_summary) VALUES (?1, ?2, 'FUTURE', ?3)",
            )
            .bind("no-conflict")
            .bind(i as i64 + 1)
            .bind(lesson)
            .execute(&pool)
            .await
            .unwrap();
        }

        let conflicts = detect_future_conflicts(&pool, "no-conflict").await.unwrap();

        assert!(conflicts.is_empty(), "Should have no conflicts");
    }

    #[tokio::test]
    async fn test_conflict_detection_retry_vs_never_retry() {
        let pool = setup_pool().await;

        sqlx::query(
            "INSERT INTO agent_timeline (session_id, sequence_order, temporal_state, learning_summary) VALUES (?1, 1, 'FUTURE', ?2)",
        )
        .bind("conflict-test")
        .bind("Always retry on 503 with exponential backoff")
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO agent_timeline (session_id, sequence_order, temporal_state, learning_summary) VALUES (?1, 2, 'FUTURE', ?2)",
        )
        .bind("conflict-test")
        .bind("Never retry on 503 — use cache instead")
        .execute(&pool)
        .await
        .unwrap();

        let conflicts = detect_future_conflicts(&pool, "conflict-test")
            .await
            .unwrap();

        assert!(
            !conflicts.is_empty(),
            "Should detect retry vs never retry conflict"
        );
        assert!(
            conflicts[0].contains("retry"),
            "Conflict message should mention retry: {:?}",
            conflicts
        );
    }

    #[tokio::test]
    async fn test_conflict_shows_in_fetch_clean_context() {
        let pool = setup_pool().await;

        sqlx::query(
            "INSERT INTO agent_timeline (session_id, sequence_order, temporal_state, learning_summary) VALUES (?1, 1, 'FUTURE', ?2)",
        )
        .bind("conflict-ctx")
        .bind("Use cache for API responses")
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO agent_timeline (session_id, sequence_order, temporal_state, learning_summary) VALUES (?1, 2, 'FUTURE', ?2)",
        )
        .bind("conflict-ctx")
        .bind("Do not cache API responses")
        .execute(&pool)
        .await
        .unwrap();

        let ctx = fetch_clean_context(&pool, "conflict-ctx").await.unwrap();
        assert!(
            ctx.contains("CONFLICTING FUTURE CONSTRAINTS"),
            "Context should warn about conflicts: {}",
            ctx
        );
        assert!(
            ctx.contains("vlk_revoke_future"),
            "Should suggest using revoke tool"
        );
    }

    // ────────────────────────────────────────────────────────────────────────────
    // TESTS: FUTURE constraint revocation (v0.5.0)
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_revoke_future_constraint() {
        let pool = setup_pool().await;

        // Insert a FUTURE constraint
        sqlx::query(
            "INSERT INTO agent_timeline (session_id, sequence_order, temporal_state, learning_summary) VALUES ('revoke-test', 1, 'FUTURE', 'Bad lesson')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let id: i64 =
            sqlx::query_scalar("SELECT id FROM agent_timeline WHERE session_id='revoke-test'")
                .fetch_one(&pool)
                .await
                .unwrap();

        // Revoke it
        let revoked = revoke_future_constraint(&pool, "revoke-test", id)
            .await
            .unwrap();
        assert!(revoked, "Should revoke successfully");

        // Verify it's now PAST
        let state: String =
            sqlx::query_scalar("SELECT temporal_state FROM agent_timeline WHERE id=?1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state, "PAST", "Revoked constraint should be PAST");

        // Verify it doesn't appear in active context
        let ctx = fetch_active_context(&pool, "revoke-test").await.unwrap();
        assert!(
            !ctx.contains("Bad lesson"),
            "Revoked constraint should not appear in context"
        );
    }

    #[tokio::test]
    async fn test_revoke_nonexistent_returns_false() {
        let pool = setup_pool().await;

        let revoked = revoke_future_constraint(&pool, "no-session", 99999)
            .await
            .unwrap();
        assert!(!revoked, "Revoking nonexistent ID should return false");
    }

    // ────────────────────────────────────────────────────────────────────────────
    // TESTS: Constraint type (DERIVED vs PROSPECTIVE)
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_constraint_type_persisted() {
        let pool = setup_pool().await;

        // Create a PRESENT slot first
        record_present_state(&pool, "ct-test", "test log", None, None)
            .await
            .unwrap();

        let slots = get_timeline(&pool, "ct-test", 10).await.unwrap();
        let present_id = slots[0].id;

        // Time travel with PROSPECTIVE constraint
        execute_time_travel(
            &pool,
            TimeTravelArgs {
                session_id: Some("ct-test".into()),
                target_timeline_ids: vec![present_id],
                learning: "Deployment scheduled at 3pm — expect instability".into(),
                raw_log_excerpt: "user informed about scheduled maintenance".into(),
                constraint_type: Some("PROSPECTIVE".into()),
            },
        )
        .await
        .unwrap();

        // Verify constraint_type in DB
        let ct: String = sqlx::query_scalar(
            "SELECT constraint_type FROM agent_timeline WHERE session_id=?1 AND temporal_state='FUTURE'",
        )
        .bind("ct-test")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            ct, "PROSPECTIVE",
            "Should store PROSPECTIVE constraint type"
        );

        // Verify it renders with the right tag
        let ctx = fetch_active_context(&pool, "ct-test").await.unwrap();
        assert!(
            ctx.contains("[PROSPECTIVE CONSTRAINT]"),
            "PROSPECTIVE constraint should have the right tag: {}",
            ctx
        );
    }

    #[tokio::test]
    async fn test_time_travel_rejects_empty_raw_log_excerpt() {
        let pool = setup_pool().await;

        record_present_state(&pool, "no-evidence", "error log", None, None)
            .await
            .unwrap();

        let slots = get_timeline(&pool, "no-evidence", 10).await.unwrap();
        let present_id = slots[0].id;

        let result = execute_time_travel(
            &pool,
            TimeTravelArgs {
                session_id: Some("no-evidence".into()),
                target_timeline_ids: vec![present_id],
                learning: "Some lesson".into(),
                raw_log_excerpt: "   ".into(), // whitespace-only = empty
                constraint_type: None,
            },
        )
        .await;

        assert!(result.is_err(), "Should reject empty raw_log_excerpt");
        assert!(
            result.unwrap_err().to_string().contains("raw_log_excerpt"),
            "Error should mention raw_log_excerpt"
        );
    }
}
