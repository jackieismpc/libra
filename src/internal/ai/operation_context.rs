//! Runtime-owned AI causality identifiers — plan-20260920 RC-06.
//!
//! KEEP `sandbox` depends on this module instead of `tools::AiOperationContext`.
//! `tools` re-exports the same type until RC-23.

/// Redacted causal identifiers attached to a tool invocation.
///
/// These values are runtime-owned identifiers, not model-provided payload. They
/// are carried to mutating handlers so the handler can persist an auditable link
/// to the current stable change projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AiOperationContext {
    pub operation_id: String,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub tool_invocation_id: String,
    pub intent_id: Option<String>,
    pub repo_id: Option<String>,
    /// Earlier successful mutating operations in this tool-loop mutation batch.
    /// A later commit/rewrite may consume this explicit set, never a repo-wide
    /// or run-wide scan.
    pub pending_operation_ids: Vec<String>,
}
