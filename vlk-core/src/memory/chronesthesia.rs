// ── Capa Chronestésica (Corteza Parietal Izquierda) ─────────────────────────
// Basado en Nyberg & Tulving (2010): "Consciousness of subjective time in the brain"
//
// El cerebro no usa el hipocampo (contenido) para viajar en el tiempo, sino una
// red diferenciada en la corteza parietal lateral izquierda. Esta implementación
// emula esa especialización: separamos el contenido (hipocampo) de la conciencia
// temporal (parietal) para que el agente pueda posicionarse ante su propia historia.
//
// TemporalState representa los tres modos de la conciencia del tiempo subjetivo:
//   PRESENT → lo que el agente está procesando ahora (contexto activo)
//   PAST    → callejones sin salida, ya aprendidos (ocultos del contexto)
//   FUTURE  → restricciones preventivas extrapoladas de la experiencia
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

/// Los tres modos de la conciencia del tiempo subjetivo del agente.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[allow(dead_code)]
pub enum TemporalState {
    /// Contexto activo: lo que el agente está procesando actualmente.
    /// Se inyecta en el prompt del LLM con su contenido completo.
    PRESENT,
    /// Callejón sin salida: el agente ya aprendió de esta experiencia.
    /// Se oculta del contexto activo para ahorrar tokens.
    PAST,
    /// Restricción preventiva: una lección extrapolada que modula el comportamiento
    /// futuro del agente. Se inyecta siempre como [PREVENTIVE FUTURE CONSTRAINT].
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

/// CAPA HIPOCAMPAL: El contenido puro e inmutable.
/// Almacena logs, stacktraces, payloads de herramientas. No cambia.
/// Es el "qué pasó" despojado de toda interpretación temporal.
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
#[allow(dead_code)]
pub struct MemoryContent {
    pub id: i64,
    pub raw_log: String,
    pub file_context: Option<String>,
    pub tool_payload: Option<String>,
}

/// CAPA PARIETAL: La línea temporal subjetiva del agente.
/// Cada slot asocia un contenido (opcional) con un estado temporal y una posición
/// en la cronología subjetiva del agente.
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct TimelineSlot {
    pub id: i64,
    pub content_id: Option<i64>,
    pub session_id: String,
    pub sequence_order: i64,
    pub temporal_state: String,
    pub learning_summary: Option<String>,
    pub created_at: Option<String>,
    // Campos enriquecidos vía JOIN con memory_contents
    pub raw_log: Option<String>,
    pub file_context: Option<String>,
}

/// Argumentos de entrada para el comando `vlk_time_travel`.
#[derive(Debug, Deserialize)]
pub struct TimeTravelArgs {
    pub session_id: Option<String>,
    /// IDs de los timeline slots en estado PRESENT que deben transicionar a PAST.
    pub target_timeline_ids: Vec<i64>,
    /// Lección aprendida — se inyecta como una restricción FUTURE.
    pub learning: String,
}

// ── Inicialización de la Base de Datos ───────────────────────────────────────

/// Crea las tablas `memory_contents` (hipocampo) y `agent_timeline` (parietal).
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

    info!("Chronesthesia tables initialized (memory_contents + agent_timeline)");
    Ok(())
}

// ── Operación Central: vlk_time_travel ──────────────────────────────────────

/// Transiciona slots del timeline de PRESENT → PAST e inyecta una restricción
/// FUTURE. Es el equivalente computacional del viaje mental en el tiempo:
/// el agente "cierra" el presente estancado y proyecta una regla hacia adelante.
pub async fn execute_time_travel(pool: &SqlitePool, args: TimeTravelArgs) -> Result<(i64, String)> {
    let session_id = args.session_id.unwrap_or_else(|| "default".to_string());
    let learning = args.learning.trim().to_string();

    if learning.is_empty() {
        anyhow::bail!("Field 'learning' is required and cannot be empty.");
    }
    if args.target_timeline_ids.is_empty() {
        anyhow::bail!("Field 'target_timeline_ids' must contain at least one ID.");
    }

    let mut tx = pool.begin().await?;

    // 1. Calcular tokens que se ahorrarán al mover estos slots a PAST
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

    // 2. Transicionar los slots seleccionados de PRESENT → PAST
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

    // 3. Obtener el siguiente número de secuencia
    let max_seq: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(sequence_order), 0) FROM agent_timeline WHERE session_id = ?1",
    )
    .bind(&session_id)
    .fetch_one(&mut *tx)
    .await?;

    // 4. Inyectar la restricción FUTURE (sin contenido pesado, solo la lección)
    sqlx::query(
        r#"
        INSERT INTO agent_timeline (content_id, session_id, sequence_order, temporal_state, learning_summary)
        VALUES (NULL, ?1, ?2, 'FUTURE', ?3)
        "#,
    )
    .bind(&session_id)
    .bind(max_seq + 1)
    .bind(&learning)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok((tokens_saved, learning))
}

// ── Consulta de Contexto Activo ─────────────────────────────────────────────

/// Genera el payload limpio para inyectar en la ventana de contexto del IDE.
/// Filtra el "Pasado" ruidoso y prioriza las reglas de "Futuro" y el "Presente"
/// inmediato. Esta es la función que el sistema de prompt del agente llamará
/// antes de cada iteración para obtener solo lo relevante.
pub async fn fetch_active_context(pool: &SqlitePool, session_id: &str) -> Result<String> {
    let rows: Vec<(String, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        r#"
        SELECT t.temporal_state, t.learning_summary, mc.raw_log, mc.file_context
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

    for (state, summary, raw_log, file_context) in rows {
        match state.as_str() {
            "FUTURE" => {
                if let Some(sum) = summary {
                    buf.push_str(&format!("[PREVENTIVE FUTURE CONSTRAINT]: {}\n", sum));
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

// ── Nivel 1: Hook de Intercepción Automática (Detección de Bucles) ──────────

/// Umbral por defecto: si el mismo raw_log aparece 3+ veces en PRESENT, se
/// considera un bucle y se mitiga automáticamente.
const LOOP_THRESHOLD_DEFAULT: usize = 3;

/// Escanea los slots PRESENT en busca del mismo raw_log repetido.
/// Si encuentra >= `threshold` ocurrencias idénticas, ejecuta `execute_time_travel`
/// de forma autónoma, inyectando una restricción FUTURE con formato de "system anchor".
///
/// Devuelve `true` si se ejecutó una mitigación, `false` si no había bucle.
pub async fn auto_detect_and_mitigate_loops(
    pool: &SqlitePool,
    session_id: &str,
    threshold: usize,
) -> Result<bool> {
    // 1. Obtener todos los slots PRESENT con su raw_log
    // Usamos Row manualmente porque query_as con tuplas es sensible a la nulabilidad
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

    // 2. Agrupar por raw_log idéntico y contar
    let mut log_counts: std::collections::HashMap<String, Vec<i64>> =
        std::collections::HashMap::new();

    for (id, raw_log) in &present_slots {
        let log_trimmed = raw_log.trim().to_string();
        log_counts.entry(log_trimmed).or_default().push(*id);
    }

    // 3. Buscar el grupo más grande que supere el threshold
    let mut target_ids: Vec<i64> = Vec::new();
    let mut target_signature = String::new();

    for (log, ids) in &log_counts {
        if ids.len() >= threshold && ids.len() > target_ids.len() {
            target_ids = ids.clone();
            target_signature = log.chars().take(80).collect();
        }
    }

    if target_ids.is_empty() {
        return Ok(false);
    }

    // 4. Ejecutar time travel autónomo con un "system anchor" agresivo
    let count = target_ids.len();

    let automated_learning = format!(
        "[SYSTEM ANCHOR] Bucle detectado: el mismo error apareció {count} veces. Último error: \"{target_signature}...\". ESTRATEGIA ACTUAL AGOTADA. Obligatorio: cambiar completamente el enfoque. No repetir la misma acción. Probar otra herramienta, otro archivo, o consultar al usuario."
    );

    // Usamos una transacción única para detection + mitigation + verification
    let mut tx = pool.begin().await?;

    // 4a. Mover los slots a PAST
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

    // 4b. Calcular tokens ahorrados
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

    // 4c. Obtener siguiente sequence_order
    let max_seq: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(sequence_order), 0) FROM agent_timeline WHERE session_id = ?1",
    )
    .bind(session_id)
    .fetch_one(&mut *tx)
    .await?;

    // 4d. Insertar FUTURE constraint
    sqlx::query(
        r#"
        INSERT INTO agent_timeline (content_id, session_id, sequence_order, temporal_state, learning_summary)
        VALUES (NULL, ?1, ?2, 'FUTURE', ?3)
        "#,
    )
    .bind(session_id)
    .bind(max_seq + 1)
    .bind(&automated_learning)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    tracing::info!(
        "[AUTO-INTERCEPT] Session '{}': mitigated {count} looped slots (rows_affected={}), ~{tokens_saved} tokens saved. System anchor injected.",
        session_id, rows_affected
    );

    Ok(true)
}

/// Versión mejorada de `fetch_active_context` que ejecuta el interceptor
/// automático antes de devolver el contexto. El agente nunca ve los errores
/// repetidos — ya llegan como restricciones FUTURE.
pub async fn fetch_clean_context(pool: &SqlitePool, session_id: &str) -> Result<String> {
    // Nivel 1: poda automática de bucles antes de armar el contexto
    let _mitigated =
        auto_detect_and_mitigate_loops(pool, session_id, LOOP_THRESHOLD_DEFAULT).await?;

    // Contexto limpio — el LLM solo ve PRESENT + FUTURE, los bucles ya están en PAST
    fetch_active_context(pool, session_id).await
}

/// Almacena un contenido en la capa hipocampal y crea un slot PRESENT en la
/// línea temporal. Se llama automáticamente cuando el agente encuentra un error
/// o estado relevante que debe trackear.
#[allow(dead_code)]
pub async fn record_present_state(
    pool: &SqlitePool,
    session_id: &str,
    raw_log: &str,
    file_context: Option<&str>,
    tool_payload: Option<&str>,
) -> Result<i64> {
    let mut tx = pool.begin().await?;

    // Insertar contenido en la capa hipocampal
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

    // Obtener el siguiente número de secuencia
    let max_seq: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(sequence_order), 0) FROM agent_timeline WHERE session_id = ?1",
    )
    .bind(session_id)
    .fetch_one(&mut *tx)
    .await?;

    // Crear slot PRESENT en el timeline
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

// ── Consultas de Historial ──────────────────────────────────────────────────

/// Obtiene el timeline completo de una sesión (todos los estados), enriquecido
/// con el contenido de memory_contents vía JOIN.
pub async fn get_timeline(
    pool: &SqlitePool,
    session_id: &str,
    limit: i64,
) -> Result<Vec<TimelineSlot>> {
    let slots = sqlx::query_as::<_, TimelineSlot>(
        r#"
        SELECT t.id, t.content_id, t.session_id, t.sequence_order,
               t.temporal_state, t.learning_summary, t.created_at,
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

/// Busca en el timeline por contenido textual (raw_log o learning_summary).
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
               t.temporal_state, t.learning_summary, t.created_at,
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

/// Genera un resumen textual de la sesión, incluyendo conteo por estado temporal.
pub async fn get_session_summary(pool: &SqlitePool, session_id: &str) -> Result<String> {
    // Conteo por estado
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

    // Calcular tokens totales aproximados
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

    /// Helper: crea una base de datos en memoria e inicializa las tablas.
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

    /// Helper: registra N slots PRESENT con logs dummy para una sesión.
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
    // 1. Inicialización
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_init_creates_tables() {
        let pool = setup_pool().await;

        // Verificar que memory_contents existe
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memory_contents'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1, "memory_contents table must exist");

        // Verificar que agent_timeline existe
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='agent_timeline'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1, "agent_timeline table must exist");

        // Verificar el CHECK constraint en temporal_state
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

        // Verificar que existe el índice compuesto para la consulta de contexto activo
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
    // 2. record_present_state — Capa Hipocampal + Slot PRESENT
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

        // Verificar que se insertó en memory_contents
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

        // Verificar que se creó el slot PRESENT en agent_timeline
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

        // Verificar: 2 slots ahora son PAST, 1 sigue PRESENT, 1 FUTURE inyectado
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
            },
        )
        .await
        .expect("execute_time_travel failed");

        // Buscar el slot FUTURE
        let future_slots: Vec<TimelineSlot> = sqlx::query_as(
            r#"SELECT t.id, t.content_id, t.session_id, t.sequence_order,
                      t.temporal_state, t.learning_summary, t.created_at,
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
            },
        )
        .await;

        assert!(result.is_err(), "Should reject empty target_timeline_ids");
    }

    // ────────────────────────────────────────────────────────────────────────────
    // 4. fetch_active_context — Filtrado quirúrgico
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
            },
        )
        .await
        .unwrap();

        let context = fetch_active_context(&pool, "session-ctx")
            .await
            .expect("fetch_active_context failed");

        // Los slots archivados (los 2 más recientes: seq 3 y 2 = "#2" y "#1") NO deben aparecer
        assert!(
            !context.contains("Error log entry #1"),
            "PAST slot (#1) raw_log leaked into active context: {}",
            context
        );
        assert!(
            !context.contains("Error log entry #2"),
            "PAST slot (#2) raw_log leaked into active context"
        );

        // Debe contener el slot PRESENT restante (el más viejo, seq 1 = "#0")
        assert!(
            context.contains("Error log entry #0"),
            "PRESENT slot (#0) should appear in active context"
        );

        // Debe contener la restricción FUTURE
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
        // Sin slots, no debe mostrar ni PRESENT ni FUTURE
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
            },
        )
        .await
        .unwrap();

        let context = fetch_active_context(&pool, "all-past")
            .await
            .expect("fetch_active_context failed");

        // Solo debe aparecer la restricción FUTURE, no los logs PAST
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
    // 5. get_timeline — Historial completo
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_get_timeline_ordered_by_sequence() {
        let pool = setup_pool().await;
        seed_present_states(&pool, "timeline-order", 5).await;

        let slots = get_timeline(&pool, "timeline-order", 10).await.unwrap();

        // Verificar orden descendente (más reciente primero)
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
    // 6. search_timeline — Búsqueda por contenido
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
    // 7. get_session_summary — Resumen con conteos
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_session_summary_counts() {
        let pool = setup_pool().await;

        // 3 PRESENT
        seed_present_states(&pool, "summary-session", 3).await;

        let slots = get_timeline(&pool, "summary-session", 10).await.unwrap();

        // Archivar 2 como PAST → 1 PRESENT restante + 1 FUTURE inyectado
        execute_time_travel(
            &pool,
            TimeTravelArgs {
                session_id: Some("summary-session".into()),
                target_timeline_ids: vec![slots[0].id, slots[1].id],
                learning: "Test lesson.".into(),
            },
        )
        .await
        .unwrap();

        let summary = get_session_summary(&pool, "summary-session")
            .await
            .expect("get_session_summary failed");

        // Formato esperado: "Session 'summary-session': 4 total timeline slots | 1 PRESENT (active) | 2 PAST (archived) | 1 FUTURE (constraints) | ~X estimated tokens in raw_log data."
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
    // 8. Aislamiento entre sesiones
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

        // Archivar solo en session-A
        let a_ids: Vec<i64> = a_slots.iter().map(|s| s.id).collect();
        execute_time_travel(
            &pool,
            TimeTravelArgs {
                session_id: Some("session-A".into()),
                target_timeline_ids: a_ids,
                learning: "Session A lesson.".into(),
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
    // 9. Integridad referencial (FOREIGN KEY)
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_foreign_key_enforced() {
        let pool = setup_pool().await;

        // Intentar insertar un timeline slot con content_id que no existe
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

        // Intentar borrar un memory_contents que tiene hijos en agent_timeline
        let result = sqlx::query("DELETE FROM memory_contents WHERE id = ?1")
            .bind(content_id)
            .execute(&pool)
            .await;

        // Sin ON DELETE CASCADE, esto debería fallar por la FK
        assert!(
            result.is_err(),
            "Should not allow deleting memory_contents referenced by timeline"
        );
    }

    // ────────────────────────────────────────────────────────────────────────────
    // 10. Integridad del CHECK constraint en temporal_state
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
    // 11. record_present_state con datos mínimos (sin file_context ni tool_payload)
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
    // 12. Múltiples viajes temporales en secuencia
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_multiple_time_travels() {
        let pool = setup_pool().await;
        seed_present_states(&pool, "multi-tt", 5).await;

        // Primer viaje: archivar slots 1,2
        let slots = get_timeline(&pool, "multi-tt", 10).await.unwrap();
        execute_time_travel(
            &pool,
            TimeTravelArgs {
                session_id: Some("multi-tt".into()),
                target_timeline_ids: vec![slots[4].id, slots[3].id], // los más viejos (seq 1,2)
                learning: "First lesson.".into(),
            },
        )
        .await
        .unwrap();

        // Segundo viaje: archivar slots 3,4
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
            },
        )
        .await
        .unwrap();

        // Resultado: 5 originales (seq 1..5).
        // Tras viaje1 (archiva seq1,2): seq1,2→PAST | seq3,4,5→PRESENT | seq6→FUTURE
        // Tras viaje2 (archiva seq3,4,5): seq3,4,5→PAST | seq7→FUTURE
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
    // 13. Tokens estimados se calculan correctamente
    // ────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_time_travel_token_estimation() {
        let pool = setup_pool().await;

        // Insertar un log grande
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
    // BENCHMARK: Rendimiento y ahorro de tokens
    // ────────────────────────────────────────────────────────────────────────────

    /// Simula el tamaño de un error real de compilación en Rust (~2000 chars).
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

    /// Simula el tamaño de un error de API con stacktrace (~1500 chars).
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

    /// Calcula tokens estimados de un string (ratio 3.8 chars/token).
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
            ctx.contains("Bucle detectado"),
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
            ctx.contains("4 veces"),
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
}
