//! `libra agent session …` subcommands. V1 surfaces rows from `agent_session`
//! and lets operators mark captured sessions stopped/active again without
//! rewriting provider transcripts. There is no `promote --as-intent`
//! surface: captured sessions stay on the observation path.

use clap::{Args, Subcommand};
use sea_orm::{ConnectionTrait, Statement};
use serde::Serialize;

use super::checkpoint::{
    PAGE_SCHEMA_VERSION, decode_page_cursor, encode_page_cursor, resolve_page_limit,
};
use crate::{
    internal::{ai::observed_agents::AgentKind, db::get_db_conn_instance},
    utils::{
        error::{CliError, CliResult},
        output::{OutputConfig, emit_json_data},
        text,
    },
};

#[derive(Subcommand, Debug)]
pub enum SessionSubcommand {
    /// List captured sessions.
    #[command(about = "List captured sessions")]
    List(SessionListArgs),
    /// Show a single session by id.
    #[command(about = "Show a captured session")]
    Show(SessionShowArgs),
    /// Stop a captured session.
    #[command(about = "Stop a captured session")]
    Stop(SessionStopArgs),
    /// Resume a stopped session.
    #[command(about = "Resume a stopped session")]
    Resume(SessionResumeArgs),
    /// Walk the session's normalized events and emit one
    /// `ToolCallRecord`-shaped JSON entry per pre/post tool use pair.
    /// Phase 4.3 (entire.md §14.4 item 3).
    #[command(about = "Derive ToolCallRecord entries from a captured session")]
    DeriveToolCalls(SessionDeriveToolCallsArgs),
}

#[derive(Args, Debug)]
pub struct SessionListArgs {
    /// Filter by agent kind (slug, e.g. `claude-code`).
    #[arg(long, value_name = "NAME")]
    pub agent: Option<String>,
    /// Filter by state (`active`, `stopped`, …).
    #[arg(long, value_name = "STATE")]
    pub state: Option<String>,
    /// Maximum rows to return (default 50, capped at 500) — AG-20
    /// metadata-first pagination.
    #[arg(long, value_name = "N")]
    pub limit: Option<u64>,
    /// Keyset cursor from the previous page's `next_cursor` (opaque;
    /// AG-20). Do not construct by hand.
    #[arg(long, value_name = "CURSOR")]
    pub cursor: Option<String>,
}

#[derive(Args, Debug)]
pub struct SessionShowArgs {
    /// `agent_session.session_id` of the session to inspect (from `libra agent session list`)
    #[arg(value_name = "SESSION_ID")]
    pub session_id: String,
    /// Materialise the captured transcript at the given path (Phase 2)
    #[arg(long, value_name = "PATH")]
    pub extract_transcript: Option<String>,
}

#[derive(Args, Debug)]
pub struct SessionStopArgs {
    /// `agent_session.session_id` of the session to mark as stopped
    #[arg(value_name = "SESSION_ID")]
    pub session_id: String,
}

#[derive(Args, Debug)]
pub struct SessionResumeArgs {
    /// `agent_session.session_id` of the stopped session to resume
    #[arg(value_name = "SESSION_ID")]
    pub session_id: String,
}

#[derive(Args, Debug)]
pub struct SessionDeriveToolCallsArgs {
    /// `agent_session.session_id` of the captured session whose
    /// SessionStore JSONL we should walk.
    pub session_id: String,
}

pub async fn execute_safe(cmd: SessionSubcommand, output: &OutputConfig) -> CliResult<()> {
    match cmd {
        SessionSubcommand::List(args) => list(args, output).await,
        SessionSubcommand::Show(args) => show(args, output).await,
        SessionSubcommand::Stop(args) => stop(args, output).await,
        SessionSubcommand::Resume(args) => resume(args, output).await,
        SessionSubcommand::DeriveToolCalls(args) => derive_tool_calls(args, output).await,
    }
}

async fn derive_tool_calls(
    args: SessionDeriveToolCallsArgs,
    output: &OutputConfig,
) -> CliResult<()> {
    use crate::{
        internal::ai::{observed_agents::derive_tool_call_records, session::SessionStore},
        utils::util,
    };

    // Load the SessionState directly from the agent capture's SessionStore
    // (Phase 3.4 partition: `<libra_dir>/sessions/agent/<session_id>/`).
    let repo_path = util::try_get_storage_path(None).map_err(|_| CliError::repo_not_found())?;
    let store = SessionStore::from_storage_path_with_subdir(&repo_path, "agent");
    let session = store.load(&args.session_id).map_err(|e| {
        CliError::fatal(format!(
            "failed to load SessionStore JSONL for '{}': {e}. \
             Was the session captured by the hook runtime under sessions/agent/?",
            args.session_id
        ))
    })?;
    let records = derive_tool_call_records(&session);

    if output.is_json() {
        return emit_json_data(
            "agent_session_derive_tool_calls",
            &serde_json::json!({
                "session_id": args.session_id,
                "records": records,
                "count": records.len(),
            }),
            output,
        );
    }
    if output.quiet {
        return Ok(());
    }
    if records.is_empty() {
        println!(
            "(no tool_use events derived from session '{}')",
            args.session_id
        );
        return Ok(());
    }
    println!(
        "Derived {} tool call(s) from '{}':",
        records.len(),
        args.session_id
    );
    println!("{:<24}  {:<14}  success", "tool_name", "action");
    for r in &records {
        println!("{:<24}  {:<14}  {}", r.tool_name, r.action, r.success);
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct SessionRow {
    session_id: String,
    agent_kind: String,
    state: String,
    working_dir: String,
    started_at: i64,
    last_event_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    capture_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capture_error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capture_error_stage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capture_failed_at: Option<i64>,
}

#[derive(Debug, Serialize)]
struct SessionMutationOutput {
    action: &'static str,
    session_id: String,
    previous_state: String,
    state: String,
    updated: bool,
    stopped_at: Option<i64>,
    last_event_at: i64,
}

#[derive(Debug, Serialize)]
struct TranscriptExtraction {
    source_path: String,
    output_path: String,
    bytes: u64,
}

#[derive(Debug, Clone, Copy)]
enum SessionMutationKind {
    Stop,
    Resume,
}

impl SessionMutationKind {
    fn action(self) -> &'static str {
        match self {
            SessionMutationKind::Stop => "stop",
            SessionMutationKind::Resume => "resume",
        }
    }

    fn json_kind(self) -> &'static str {
        match self {
            SessionMutationKind::Stop => "agent_session_stop",
            SessionMutationKind::Resume => "agent_session_resume",
        }
    }
}

fn extract_transcript_from_metadata(
    metadata_json: &str,
    output_path: &str,
) -> CliResult<TranscriptExtraction> {
    let metadata: serde_json::Value = serde_json::from_str(metadata_json).map_err(|e| {
        CliError::fatal(format!(
            "captured session metadata_json is not valid JSON; cannot extract transcript: {e}"
        ))
    })?;
    let source = metadata
        .get("transcript_path")
        .and_then(|value| value.as_str())
        .filter(|path| !path.trim().is_empty())
        .ok_or_else(|| {
            CliError::fatal(
                "captured session metadata_json does not contain transcript_path; cannot extract transcript",
            )
        })?;
    let source_path = std::path::PathBuf::from(source);
    let output = std::path::PathBuf::from(output_path);
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| {
            CliError::fatal(format!(
                "failed to create transcript output directory '{}': {e}",
                parent.display()
            ))
        })?;
    }
    let bytes = std::fs::copy(&source_path, &output).map_err(|e| {
        CliError::fatal(format!(
            "failed to copy transcript from '{}' to '{}': {e}",
            source_path.display(),
            output.display()
        ))
    })?;
    Ok(TranscriptExtraction {
        source_path: source_path.display().to_string(),
        output_path: output.display().to_string(),
        bytes,
    })
}

/// Build the paginated `session list` SQL. Extracted so the EXPLAIN QUERY
/// PLAN tests run the exact production statement against the
/// `idx_agent_session_started_paging` index (never a table SCAN, never a
/// temp B-tree). Placeholder order: `[agent_kind,] [state,] [started_at,
/// started_at, session_id,] limit`. Keyset shape matches the index:
/// `(started_at DESC, session_id ASC)` — see
/// [`super::checkpoint::encode_page_cursor`].
pub(super) fn session_page_sql(with_agent: bool, with_state: bool, with_cursor: bool) -> String {
    let mut sql = String::from(
        "SELECT session_id, agent_kind, state, working_dir, started_at, last_event_at \
         FROM agent_session WHERE 1=1",
    );
    if with_agent {
        sql.push_str(" AND agent_kind = ?");
    }
    if with_state {
        sql.push_str(" AND state = ?");
    }
    if with_cursor {
        sql.push_str(" AND (started_at < ? OR (started_at = ? AND session_id > ?))");
    }
    sql.push_str(" ORDER BY started_at DESC, session_id ASC LIMIT ?");
    sql
}

/// One page of `session list` output. The JSON `data` payload carries the
/// rows under `sessions` (per-row schema unchanged from the
/// pre-pagination output) plus `next_cursor` — the opaque `--cursor`
/// token for the next page, `null` once the listing is exhausted.
#[derive(Debug, Serialize)]
struct SessionListPage {
    schema_version: u32,
    sessions: Vec<SessionRow>,
    next_cursor: Option<String>,
}

async fn list(args: SessionListArgs, output: &OutputConfig) -> CliResult<()> {
    let (limit, clamp_note) = resolve_page_limit(args.limit);
    if let Some(note) = &clamp_note {
        eprintln!("{note}");
    }
    // Decode the cursor before touching the database so a malformed value
    // is a pure usage error.
    let cursor = args.cursor.as_deref().map(decode_page_cursor).transpose()?;

    let conn = get_db_conn_instance().await;
    let backend = conn.get_database_backend();

    if !table_exists(&conn, "agent_session").await? {
        return emit_list(
            &SessionListPage {
                schema_version: PAGE_SCHEMA_VERSION,
                sessions: Vec::new(),
                next_cursor: None,
            },
            output,
        );
    }

    let sql = session_page_sql(args.agent.is_some(), args.state.is_some(), cursor.is_some());
    let mut values: Vec<sea_orm::Value> = Vec::new();
    if let Some(agent) = &args.agent {
        // The CLI accepts hyphenated slugs (`claude-code`) but the database
        // stores the snake_case `agent_kind` (`claude_code`). Translate to
        // the storage form so a `--agent claude-code` filter actually
        // matches rows. Codex review P1 #6.
        let normalized = match AgentKind::from_cli_slug(agent) {
            Some(kind) => kind.as_db_str().to_string(),
            None => agent.clone(),
        };
        values.push(normalized.into());
    }
    if let Some(state) = &args.state {
        values.push(state.clone().into());
    }
    if let Some((timestamp, id)) = &cursor {
        values.push((*timestamp).into());
        values.push((*timestamp).into());
        values.push(id.clone().into());
    }
    // Fetch one row beyond the page to learn whether another page exists
    // without a second COUNT query.
    values.push((limit as i64 + 1).into());

    let stmt = Statement::from_sql_and_values(backend, &sql, values);
    let rows = conn
        .query_all_raw(stmt)
        .await
        .map_err(|e| CliError::fatal(format!("failed to query agent_session: {e}")))?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(SessionRow {
            session_id: row
                .try_get_by::<String, _>("session_id")
                .unwrap_or_default(),
            agent_kind: row
                .try_get_by::<String, _>("agent_kind")
                .unwrap_or_default(),
            state: row.try_get_by::<String, _>("state").unwrap_or_default(),
            working_dir: row
                .try_get_by::<String, _>("working_dir")
                .unwrap_or_default(),
            started_at: row.try_get_by::<i64, _>("started_at").unwrap_or_default(),
            last_event_at: row
                .try_get_by::<i64, _>("last_event_at")
                .unwrap_or_default(),
            capture_status: None,
            capture_error_code: None,
            capture_error_stage: None,
            capture_failed_at: None,
        });
    }
    let next_cursor = if out.len() as u64 > limit {
        out.truncate(limit as usize);
        out.last()
            .map(|row| encode_page_cursor(row.started_at, &row.session_id))
    } else {
        None
    };
    emit_list(
        &SessionListPage {
            schema_version: PAGE_SCHEMA_VERSION,
            sessions: out,
            next_cursor,
        },
        output,
    )
}

async fn show(args: SessionShowArgs, output: &OutputConfig) -> CliResult<()> {
    let conn = get_db_conn_instance().await;
    let backend = conn.get_database_backend();

    if !table_exists(&conn, "agent_session").await? {
        return Err(CliError::fatal(format!(
            "no captured session matches '{}': agent_session table not yet present (run `libra init`?)",
            args.session_id
        )));
    }

    let stmt = Statement::from_sql_and_values(
        backend,
        "SELECT session_id, agent_kind, state, working_dir, started_at, last_event_at, \
                COALESCE(metadata_json, '{}') AS metadata_json \
         FROM agent_session WHERE session_id = ? LIMIT 1",
        [args.session_id.clone().into()],
    );
    let row = conn
        .query_one_raw(stmt)
        .await
        .map_err(|e| CliError::fatal(format!("failed to query agent_session: {e}")))?;
    match row {
        Some(row) => {
            let metadata_json = row
                .try_get_by::<String, _>("metadata_json")
                .unwrap_or_else(|_| "{}".to_string());
            let capture = capture_diagnostic_from_metadata(&metadata_json);
            let payload = SessionRow {
                session_id: row
                    .try_get_by::<String, _>("session_id")
                    .unwrap_or_default(),
                agent_kind: row
                    .try_get_by::<String, _>("agent_kind")
                    .unwrap_or_default(),
                state: row.try_get_by::<String, _>("state").unwrap_or_default(),
                working_dir: row
                    .try_get_by::<String, _>("working_dir")
                    .unwrap_or_default(),
                started_at: row.try_get_by::<i64, _>("started_at").unwrap_or_default(),
                last_event_at: row
                    .try_get_by::<i64, _>("last_event_at")
                    .unwrap_or_default(),
                capture_status: capture.status,
                capture_error_code: capture.error_code,
                capture_error_stage: capture.error_stage,
                capture_failed_at: capture.failed_at,
            };
            let transcript = if let Some(path) = args.extract_transcript.as_deref() {
                let metadata_json = row.try_get_by::<String, _>("metadata_json").map_err(|e| {
                    CliError::fatal(format!(
                        "agent_session.metadata_json for '{}' could not be decoded as TEXT: {e}",
                        args.session_id
                    ))
                })?;
                Some(extract_transcript_from_metadata(&metadata_json, path)?)
            } else {
                None
            };
            if output.is_json() && transcript.is_some() {
                return emit_json_data(
                    "agent_session",
                    &serde_json::json!({
                        "session": payload,
                        "extracted_transcript": transcript,
                    }),
                    output,
                );
            }
            emit_one(&payload, output)?;
            if let Some(transcript) = transcript
                && !output.quiet
            {
                println!("transcript     : {}", transcript.output_path);
                println!("transcript_src : {}", transcript.source_path);
                println!("transcript_len : {} bytes", transcript.bytes);
            }
            Ok(())
        }
        None => Err(CliError::fatal(format!(
            "no captured session matches id '{}'",
            args.session_id
        ))),
    }
}

async fn stop(args: SessionStopArgs, output: &OutputConfig) -> CliResult<()> {
    let conn = get_db_conn_instance().await;
    let result = mutate_session_state(&conn, &args.session_id, SessionMutationKind::Stop).await?;
    emit_session_mutation(&result, output)
}

async fn resume(args: SessionResumeArgs, output: &OutputConfig) -> CliResult<()> {
    let conn = get_db_conn_instance().await;
    let result = mutate_session_state(&conn, &args.session_id, SessionMutationKind::Resume).await?;
    emit_session_mutation(&result, output)
}

async fn mutate_session_state(
    conn: &(impl ConnectionTrait + ?Sized),
    session_id: &str,
    kind: SessionMutationKind,
) -> CliResult<SessionMutationOutput> {
    let backend = conn.get_database_backend();
    if !table_exists(conn, "agent_session").await? {
        return Err(CliError::fatal(format!(
            "no captured session matches '{session_id}': agent_session table not yet present (run `libra init`?)"
        )));
    }

    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            backend,
            "SELECT state, stopped_at, last_event_at, scope_state, worktree_id \
             FROM agent_session WHERE session_id = ? LIMIT 1",
            [session_id.into()],
        ))
        .await
        .map_err(|e| CliError::fatal(format!("failed to query agent_session: {e}")))?
        .ok_or_else(|| CliError::fatal(format!("no captured session matches id '{session_id}'")))?;

    let current_state = row.try_get_by::<String, _>("state").map_err(|e| {
        CliError::fatal(format!(
            "agent_session.state for '{session_id}' could not be decoded as TEXT: {e}"
        ))
    })?;
    let current_stopped_at = row
        .try_get_by::<Option<i64>, _>("stopped_at")
        .map_err(|e| {
            CliError::fatal(format!(
                "agent_session.stopped_at for '{session_id}' could not be decoded as INTEGER: {e}"
            ))
        })?;
    let current_last_event_at = row.try_get_by::<i64, _>("last_event_at").map_err(|e| {
        CliError::fatal(format!(
            "agent_session.last_event_at for '{session_id}' could not be decoded as INTEGER: {e}"
        ))
    })?;

    if current_state == "quarantined" {
        return Err(CliError::fatal(format!(
            "cannot {} captured session '{session_id}' because it is quarantined; inspect the capture state before mutating it",
            kind.action()
        )));
    }

    match kind {
        SessionMutationKind::Stop if current_state == "stopped" => {
            return Ok(SessionMutationOutput {
                action: kind.action(),
                session_id: session_id.to_string(),
                previous_state: current_state.clone(),
                state: current_state,
                updated: false,
                stopped_at: current_stopped_at,
                last_event_at: current_last_event_at,
            });
        }
        SessionMutationKind::Resume if current_state == "active" => {
            return Ok(SessionMutationOutput {
                action: kind.action(),
                session_id: session_id.to_string(),
                previous_state: current_state.clone(),
                state: current_state,
                updated: false,
                stopped_at: current_stopped_at,
                last_event_at: current_last_event_at,
            });
        }
        SessionMutationKind::Resume if current_state != "stopped" => {
            return Err(CliError::fatal(format!(
                "cannot resume captured session '{session_id}' from state '{current_state}'; only stopped sessions can be resumed"
            )));
        }
        _ => {}
    }

    // §C.4.1.1: lifecycle mutations carry the WRITER's scope. A row another
    // worktree's scope owns is refused (a cross-scope stop would make the
    // foreign session eligible for `agent clean --gc` retention and bump its
    // replication revision), and `legacy_unknown` rows are excluded from
    // every new write until explicitly adopted — the same rule the capture
    // writers enforce.
    let scope_state = row
        .try_get_by::<Option<String>, _>("scope_state")
        .unwrap_or(None);
    if scope_state.as_deref() == Some("legacy_unknown") {
        return Err(CliError::fatal(format!(
            "session '{session_id}' predates workspace scoping and is excluded from new \
             writes; adopt it first with `libra worktree doctor <workspace-id> \
             --adopt-capture-session {session_id} --confirm`"
        )));
    }
    if scope_state.as_deref() == Some("scoped") {
        let row_worktree = row
            .try_get_by::<Option<String>, _>("worktree_id")
            .unwrap_or(None)
            .unwrap_or_default();
        let scope = crate::internal::worktree_scope::WorktreeScope::for_request();
        let current_worktree = scope.worktree_id().unwrap_or_default().to_string();
        if row_worktree != current_worktree {
            return Err(CliError::fatal(format!(
                "session '{session_id}' is owned by another worktree scope ('{row_worktree}'); \
                 run this from that worktree, or inspect with `libra worktree doctor`"
            )));
        }
    }
    let now = chrono::Utc::now().timestamp();
    let (new_state, new_stopped_at) = match kind {
        SessionMutationKind::Stop => ("stopped", Some(now)),
        SessionMutationKind::Resume => ("active", None),
    };
    conn.execute_raw(Statement::from_sql_and_values(
        backend,
        "UPDATE agent_session \
         SET state = ?, last_event_at = ?, stopped_at = ?, \
             sync_revision = sync_revision + 1 \
         WHERE session_id = ?",
        vec![
            new_state.into(),
            now.into(),
            new_stopped_at.into(),
            session_id.to_string().into(),
        ],
    ))
    .await
    .map_err(|e| {
        CliError::fatal(format!(
            "failed to update agent_session state for '{session_id}': {e}"
        ))
    })?;

    Ok(SessionMutationOutput {
        action: kind.action(),
        session_id: session_id.to_string(),
        previous_state: current_state,
        state: new_state.to_string(),
        updated: true,
        stopped_at: new_stopped_at,
        last_event_at: now,
    })
}

fn emit_session_mutation(result: &SessionMutationOutput, output: &OutputConfig) -> CliResult<()> {
    if output.is_json() {
        let kind = match result.action {
            "stop" => SessionMutationKind::Stop,
            "resume" => SessionMutationKind::Resume,
            _ => SessionMutationKind::Stop,
        };
        return emit_json_data(kind.json_kind(), result, output);
    }
    if output.quiet {
        return Ok(());
    }
    if result.updated {
        println!(
            "session '{}' {}: {} -> {}",
            result.session_id, result.action, result.previous_state, result.state
        );
    } else {
        println!("session '{}' already {}", result.session_id, result.state);
    }
    Ok(())
}

fn emit_list(page: &SessionListPage, output: &OutputConfig) -> CliResult<()> {
    if output.is_json() {
        return emit_json_data("agent_sessions", page, output);
    }
    if output.quiet {
        return Ok(());
    }
    if page.sessions.is_empty() {
        println!("(no captured sessions)");
        return Ok(());
    }
    for line in format_session_list_human(page) {
        println!("{line}");
    }
    Ok(())
}

fn format_session_list_human(page: &SessionListPage) -> Vec<String> {
    format_session_list_human_at(page, chrono::Local::now().timestamp())
}

fn format_session_list_human_at(page: &SessionListPage, now: i64) -> Vec<String> {
    let started_at_values: Vec<String> = page
        .sessions
        .iter()
        .map(|row| text::relative_date_at(now, row.started_at))
        .collect();
    let (session_id_width, agent_kind_width, state_width, started_at_width) =
        session_list_column_widths(&page.sessions, &started_at_values);
    let mut lines =
        Vec::with_capacity(page.sessions.len() + usize::from(page.next_cursor.is_some()) + 1);
    lines.push(format!(
        "{:<session_id_width$}  {:<agent_kind_width$}  {:<state_width$}  {:<started_at_width$}",
        "session_id", "agent_kind", "state", "started_at"
    ));
    for (r, started_at) in page.sessions.iter().zip(started_at_values.iter()) {
        lines.push(format!(
            "{:<session_id_width$}  {:<agent_kind_width$}  {:<state_width$}  {:<started_at_width$}",
            r.session_id, r.agent_kind, r.state, started_at
        ));
    }
    if let Some(cursor) = &page.next_cursor {
        lines.push(format!(
            "(more rows available — next page: --cursor {cursor})"
        ));
    }
    lines
}

fn session_list_column_widths(
    rows: &[SessionRow],
    started_at_values: &[String],
) -> (usize, usize, usize, usize) {
    let session_id_width = rows
        .iter()
        .map(|row| row.session_id.len())
        .chain(std::iter::once("session_id".len()))
        .max()
        .unwrap_or("session_id".len())
        .max(37);
    let agent_kind_width = rows
        .iter()
        .map(|row| row.agent_kind.len())
        .chain(std::iter::once("agent_kind".len()))
        .max()
        .unwrap_or("agent_kind".len())
        .max(14);
    let state_width = rows
        .iter()
        .map(|row| row.state.len())
        .chain(std::iter::once("state".len()))
        .max()
        .unwrap_or("state".len())
        .max(10);
    let started_at_width = started_at_values
        .iter()
        .map(String::len)
        .chain(std::iter::once("started_at".len()))
        .max()
        .unwrap_or("started_at".len())
        .max(20);
    (
        session_id_width,
        agent_kind_width,
        state_width,
        started_at_width,
    )
}

fn emit_one(row: &SessionRow, output: &OutputConfig) -> CliResult<()> {
    if output.is_json() {
        return emit_json_data("agent_session", row, output);
    }
    if output.quiet {
        return Ok(());
    }
    println!("session_id    : {}", row.session_id);
    println!("agent_kind    : {}", row.agent_kind);
    println!("state         : {}", row.state);
    println!("working_dir   : {}", row.working_dir);
    println!("started_at    : {}", row.started_at);
    println!("last_event_at : {}", row.last_event_at);
    if let Some(status) = row.capture_status.as_deref() {
        println!("capture_status: {status}");
        if let Some(code) = row.capture_error_code.as_deref() {
            println!("capture_error : {code}");
        }
        if let Some(stage) = row.capture_error_stage.as_deref() {
            println!("capture_stage : {stage}");
        }
        if let Some(failed_at) = row.capture_failed_at {
            println!("capture_failed: {failed_at}");
        }
        println!(
            "capture_retry : next Codex Stop or `libra agent import --session <id> --agent codex --yes`"
        );
    }
    Ok(())
}

#[derive(Debug, Default)]
struct CaptureDiagnostic {
    status: Option<String>,
    error_code: Option<String>,
    error_stage: Option<String>,
    failed_at: Option<i64>,
}

fn capture_diagnostic_from_metadata(metadata_json: &str) -> CaptureDiagnostic {
    let Ok(metadata) = serde_json::from_str::<serde_json::Value>(metadata_json) else {
        return CaptureDiagnostic::default();
    };
    CaptureDiagnostic {
        status: metadata
            .get("capture_status")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        error_code: metadata
            .get("capture_error_code")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        error_stage: metadata
            .get("capture_error_stage")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        failed_at: metadata
            .get("capture_failed_at")
            .and_then(serde_json::Value::as_i64),
    }
}

async fn table_exists(conn: &(impl ConnectionTrait + ?Sized), name: &str) -> CliResult<bool> {
    let backend = conn.get_database_backend();
    let stmt = Statement::from_sql_and_values(
        backend,
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ? LIMIT 1",
        [name.into()],
    );
    conn.query_one_raw(stmt)
        .await
        .map(|row| row.is_some())
        .map_err(|e| CliError::fatal(format!("failed to query sqlite_master: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §C.4.1.1 fail-closed witness (W4 review): lifecycle mutations refuse
    /// (a) `legacy_unknown` rows — excluded from every new write until
    /// explicitly adopted — and (b) rows another worktree scope owns. Both
    /// name the way out.
    #[tokio::test]
    #[serial_test::serial]
    async fn stop_refuses_legacy_and_foreign_scope_rows() {
        use sea_orm::{ConnectionTrait, Database, DbBackend, Statement};

        let db = Database::connect("sqlite::memory:").await.expect("connect");
        db.execute_raw(Statement::from_string(
            DbBackend::Sqlite,
            "CREATE TABLE agent_session (
                session_id TEXT PRIMARY KEY,
                agent_kind TEXT NOT NULL,
                provider_session_id TEXT NOT NULL,
                state TEXT NOT NULL,
                working_dir TEXT NOT NULL,
                worktree_id TEXT,
                metadata_json TEXT NOT NULL DEFAULT '{}',
                started_at INTEGER NOT NULL DEFAULT 0,
                last_event_at INTEGER NOT NULL DEFAULT 0,
                stopped_at INTEGER,
                schema_version INTEGER NOT NULL DEFAULT 1,
                sync_revision INTEGER NOT NULL DEFAULT 0,
                scope_state TEXT
            )",
        ))
        .await
        .expect("create table");
        for (id, scope_state, worktree) in [
            ("legacy-1", "legacy_unknown", ""),
            ("foreign-1", "scoped", "wt-other"),
        ] {
            db.execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO agent_session (session_id, agent_kind, provider_session_id, \
                 state, working_dir, worktree_id, scope_state) VALUES (?, 'claude_code', ?, \
                 'active', '/tmp', ?, ?)",
                [id.into(), id.into(), worktree.into(), scope_state.into()],
            ))
            .await
            .expect("seed row");
        }

        let legacy = mutate_session_state(&db, "legacy-1", SessionMutationKind::Stop)
            .await
            .expect_err("legacy rows are excluded from new writes");
        assert!(
            legacy.to_string().contains("adopt"),
            "the refusal names the adoption path: {legacy}"
        );

        let foreign = mutate_session_state(&db, "foreign-1", SessionMutationKind::Stop)
            .await
            .expect_err("another scope's row is not this worktree's to stop");
        assert!(
            foreign.to_string().contains("another worktree scope"),
            "the refusal says whose it is: {foreign}"
        );
    }

    #[test]
    fn session_list_human_output_aligns_after_long_session_id() {
        let now = 1_700_000_000;
        let page = SessionListPage {
            schema_version: PAGE_SCHEMA_VERSION,
            sessions: vec![
                SessionRow {
                    session_id: format!("claude__{}", "x".repeat(80)),
                    agent_kind: "claude_code".to_string(),
                    state: "active".to_string(),
                    working_dir: "/tmp/repo".to_string(),
                    started_at: now - 2 * 3_600,
                    last_event_at: 1_700_000_100,
                    capture_status: None,
                    capture_error_code: None,
                    capture_error_stage: None,
                    capture_failed_at: None,
                },
                SessionRow {
                    session_id: "short-session".to_string(),
                    agent_kind: "codex".to_string(),
                    state: "stopped".to_string(),
                    working_dir: "/tmp/repo".to_string(),
                    started_at: now - 20 * 86_400,
                    last_event_at: 1_700_000_110,
                    capture_status: None,
                    capture_error_code: None,
                    capture_error_stage: None,
                    capture_failed_at: None,
                },
            ],
            next_cursor: Some("v1:1700000010:short-session".to_string()),
        };

        let lines = format_session_list_human_at(&page, now);
        let header = &lines[0];
        let first_row = &lines[1];
        let second_row = &lines[2];

        let agent_col = header.find("agent_kind").unwrap();
        let state_col = header.find("state").unwrap();
        let started_col = header.find("started_at").unwrap();

        assert_eq!(first_row.find("claude_code").unwrap(), agent_col);
        assert_eq!(second_row.find("codex").unwrap(), agent_col);
        assert_eq!(first_row.find("active").unwrap(), state_col);
        assert_eq!(second_row.find("stopped").unwrap(), state_col);
        assert_eq!(first_row.find("2 hours ago").unwrap(), started_col);
        assert_eq!(second_row.find("3 weeks ago").unwrap(), started_col);
        assert_eq!(
            lines.last().unwrap(),
            "(more rows available — next page: --cursor v1:1700000010:short-session)"
        );
    }

    use sea_orm::{ConnectOptions, Database, DatabaseConnection, ExecResult};
    use tempfile::TempDir;

    use crate::internal::{
        db::{ensure_ai_runtime_contract_schema, migration::run_builtin_migrations},
        worktree_scope::{RequestScope, with_request_scope},
    };

    const LEGACY_BOOTSTRAP_SQL: &str = include_str!("../../../sql/sqlite_20260309_init.sql");

    /// Fresh-DB fixture mirroring the hook runtime / cloud-restore tests.
    /// `repo_path` is rooted at `<tempdir>/.libra/` so `objects/` and
    /// `libra.db` co-locate the way production does.
    async fn fresh_repo() -> (TempDir, DatabaseConnection, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo_path = dir.path().join(".libra");
        std::fs::create_dir(&repo_path).unwrap();
        let db_path = repo_path.join(crate::utils::util::DATABASE);
        std::fs::File::create(&db_path).unwrap();
        let url = format!("sqlite://{}", db_path.display());
        let mut opts = ConnectOptions::new(url);
        opts.sqlx_logging(false);
        let conn = Database::connect(opts).await.unwrap();
        let backend = conn.get_database_backend();
        for raw in LEGACY_BOOTSTRAP_SQL.split(';') {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let _: ExecResult = conn
                .execute_raw(Statement::from_string(backend, trimmed.to_string()))
                .await
                .unwrap_or_else(|e| panic!("legacy bootstrap stmt failed: {trimmed}\n{e}"));
        }
        ensure_ai_runtime_contract_schema(&conn).await.unwrap();
        run_builtin_migrations(&conn).await.unwrap();
        (dir, conn, repo_path)
    }

    async fn insert_agent_session_fixture(
        conn: &DatabaseConnection,
        session_id: &str,
        state: &str,
        stopped_at: Option<i64>,
    ) {
        insert_agent_session_fixture_with_metadata(conn, session_id, state, stopped_at, "{}").await;
    }

    async fn insert_agent_session_fixture_with_metadata(
        conn: &DatabaseConnection,
        session_id: &str,
        state: &str,
        stopped_at: Option<i64>,
        metadata_json: &str,
    ) {
        let worktree_id = crate::internal::worktree_scope::WorktreeScope::for_request()
            .storage_key()
            .to_string();
        let backend = conn.get_database_backend();
        conn.execute_raw(Statement::from_sql_and_values(
            backend,
            // scope_state='scoped' + the ambient worktree models a MODERN
            // capture: the lifecycle gate excludes legacy_unknown rows from
            // every new write (its own test covers that refusal), and this
            // fixture stays valid when the suite runs from a linked worktree.
            "INSERT INTO agent_session (
                session_id, agent_kind, provider_session_id, state, working_dir,
                metadata_json, redaction_report, started_at, last_event_at, stopped_at,
                scope_state, repo_id, worktree_id, workspace_id, workspace_fence
             ) VALUES (?, 'claude_code', ?, ?, '/tmp/repo', ?, '{}', 1700000000, \
                       1700000100, ?, 'scoped', 'test-repo', ?, 'ws-test', 1)",
            vec![
                session_id.to_string().into(),
                format!("{session_id}-provider").into(),
                state.to_string().into(),
                metadata_json.to_string().into(),
                stopped_at.into(),
                worktree_id.into(),
            ],
        ))
        .await
        .unwrap();
    }

    async fn read_agent_session_state(
        conn: &DatabaseConnection,
        session_id: &str,
    ) -> (String, Option<i64>, i64) {
        let backend = conn.get_database_backend();
        let row = conn
            .query_one_raw(Statement::from_sql_and_values(
                backend,
                "SELECT state, stopped_at, last_event_at \
                 FROM agent_session WHERE session_id = ? LIMIT 1",
                [session_id.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        (
            row.try_get_by::<String, _>("state").unwrap(),
            row.try_get_by::<Option<i64>, _>("stopped_at").unwrap(),
            row.try_get_by::<i64, _>("last_event_at").unwrap(),
        )
    }

    #[tokio::test]
    async fn agent_session_stop_marks_active_session_stopped() {
        let (dir, conn, _repo_path) = fresh_repo().await;
        let request_scope = RequestScope::resolve(dir.path().to_path_buf())
            .expect("fresh repository should resolve its request scope");

        with_request_scope(Some(request_scope), async {
            insert_agent_session_fixture(&conn, "claude__stop-active", "active", None).await;

            let result =
                mutate_session_state(&conn, "claude__stop-active", SessionMutationKind::Stop)
                    .await
                    .unwrap();

            assert_eq!(result.action, "stop");
            assert!(result.updated);
            assert_eq!(result.previous_state, "active");
            assert_eq!(result.state, "stopped");
            assert_eq!(result.stopped_at, Some(result.last_event_at));

            let (state, stopped_at, last_event_at) =
                read_agent_session_state(&conn, "claude__stop-active").await;
            assert_eq!(state, "stopped");
            assert_eq!(stopped_at, result.stopped_at);
            assert_eq!(last_event_at, result.last_event_at);
        })
        .await;
    }

    #[tokio::test]
    async fn agent_session_resume_marks_stopped_session_active() {
        let (dir, conn, _repo_path) = fresh_repo().await;
        let request_scope = RequestScope::resolve(dir.path().to_path_buf())
            .expect("fresh repository should resolve its request scope");

        with_request_scope(Some(request_scope), async {
            insert_agent_session_fixture(
                &conn,
                "claude__resume-stopped",
                "stopped",
                Some(1_700_000_100),
            )
            .await;

            let result =
                mutate_session_state(&conn, "claude__resume-stopped", SessionMutationKind::Resume)
                    .await
                    .unwrap();

            assert_eq!(result.action, "resume");
            assert!(result.updated);
            assert_eq!(result.previous_state, "stopped");
            assert_eq!(result.state, "active");
            assert_eq!(result.stopped_at, None);

            let (state, stopped_at, last_event_at) =
                read_agent_session_state(&conn, "claude__resume-stopped").await;
            assert_eq!(state, "active");
            assert_eq!(stopped_at, None);
            assert_eq!(last_event_at, result.last_event_at);
        })
        .await;
    }

    #[tokio::test]
    async fn agent_session_resume_rejects_non_stopped_session_states() {
        let (dir, conn, _repo_path) = fresh_repo().await;
        let request_scope = RequestScope::resolve(dir.path().to_path_buf())
            .expect("fresh repository should resolve its request scope");

        with_request_scope(Some(request_scope), async {
            insert_agent_session_fixture(&conn, "claude__resume-condensed", "condensed", None)
                .await;

            let err = mutate_session_state(
                &conn,
                "claude__resume-condensed",
                SessionMutationKind::Resume,
            )
            .await
            .unwrap_err();

            assert!(
                err.to_string()
                    .contains("only stopped sessions can be resumed"),
                "{err}"
            );
        })
        .await;
    }

    #[test]
    fn agent_session_extract_transcript_copies_metadata_path() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("captured.jsonl");
        let output = dir.path().join("nested").join("copy.jsonl");
        std::fs::write(&source, "{\"type\":\"message\"}\n").unwrap();
        let metadata = serde_json::json!({
            "transcript_path": source,
        })
        .to_string();

        let result =
            extract_transcript_from_metadata(&metadata, output.to_string_lossy().as_ref()).unwrap();

        assert_eq!(result.bytes, 19);
        assert_eq!(result.source_path, source.display().to_string());
        assert_eq!(result.output_path, output.display().to_string());
        assert_eq!(
            std::fs::read_to_string(output).unwrap(),
            "{\"type\":\"message\"}\n"
        );
    }
}
