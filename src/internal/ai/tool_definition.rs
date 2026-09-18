//! Provider-neutral tool schema — plan-20260920 RC-19.
//!
//! KEEP `completion` depends on this module instead of `tools::ToolDefinition`.
//! `tools` re-exports the same type until RC-23.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}
