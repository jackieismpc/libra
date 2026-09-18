//! Capture-side tool-call projection — plan-20260920 RC-04.
//!
//! `observed_agents/derived.rs` and `agent session derive-tool-calls`
//! depend on this module instead of `orchestrator::types`. Orchestrator
//! re-exports the same types until RC-23.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A summary of a tool call executed within a task.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ToolDiffRecord {
    pub path: String,
    pub change_type: String,
    pub diff: String,
}

/// A summary of a tool call executed within a task.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub tool_name: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_json: Option<Value>,
    #[serde(default)]
    pub paths_read: Vec<String>,
    #[serde(default)]
    pub paths_written: Vec<String>,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default)]
    pub diffs: Vec<ToolDiffRecord>,
}
