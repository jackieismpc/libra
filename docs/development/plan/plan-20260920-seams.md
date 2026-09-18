# plan-20260920 缝清单（RC-00）

> 扫描日：2026-09-18。只登记生产代码（已排除 `#[cfg(test)]` 模块与 `**/tests/**` 路径，除非该命中是 RC-16 的默认套件 `mod` 清单）。
>
> 归类：`extract` = Phase 1 抽到 KEEP；`cli-only` = Phase 2 只断 argv；`delete-after-importer-gone` = 等 importer 消失再 `rm`；`forbidden` = KEEP 不得再新增这条边。

## 扫描命令

```bash
rg -n "command::code|command::graph|code_control_files|internal::ai::web|internal::ai::runtime|internal::ai::orchestrator|internal::ai::providers|internal::ai::tools|runtime::hardening|classify_ai_command_safety" src src/command tests/command/mod.rs --glob '!**/tests/**'
rg -n "UsageRecorder|INSERT INTO agent_usage_stats" src --glob '!**/tests/**'
rg -n "Commands::Package|command::package::" src/cli.rs   # 必须 exit 1
rg -n "dispatch_current_repo_vcs_event_to_history" src/command
rg -n "mod code_test|mod graph_test|mod usage_help_test|mod code_control_|mod publish_test" tests/command/mod.rs
rg -n "internal::ai::projection|ProjectionRebuilder|MaterializedProjection" src/command/publish.rs src/internal/publish
rg -n "CloudPublishSource|parse_cloud_publish_source|PublishStorage" src/command/clone.rs
```

`Commands::Package` 在 `src/cli.rs` 零命中（exit 1）。自动化仍被 `add`/`commit`/`push`/`branch`/`switch` 调用。

## 命中表（KEEP → 将删模块）

| 文件 | import / 符号 | 归类 | 承接卡 |
|---|---|---|---|
| `src/internal/ai/hooks/lifecycle.rs:25` | `runtime::event::Event` | `extract` | RC-01 |
| `src/internal/ai/history.rs:92` | `AI_REF` / intent+traces 混写 | `extract`（traces API） | RC-02 |
| `src/internal/ai/review/mod.rs:74` | `agent::runtime::sub_agent_dispatcher::materialize_isolated_workspace` | `extract` | RC-03 |
| `src/internal/ai/review/runner.rs:76` | `WorkspaceIsolationConfig` + `materialize_isolated_workspace` | `extract` | RC-03 / RC-07 |
| `src/internal/ai/investigate/runner.rs:49` | 同上 | `extract` | RC-03 / RC-07 |
| `src/internal/ai/observed_agents/derived.rs:78` | `orchestrator::types::ToolCallRecord` | `extract` | RC-04 |
| `src/command/service.rs:49` | `command::code_control_files::{…}` | `extract` | RC-05 |
| `src/internal/ai/sandbox/mod.rs:859` | `tools::AiOperationContext` | `extract` | RC-06 |
| `src/internal/ai/sandbox/mod.rs:31,4278,4553` | `runtime::hardening::{SafetyDecision,SafetyDisposition,BlastRadius}` | `extract` | RC-09 |
| `src/internal/ai/automation/executor.rs:12-13` | `runtime::hardening` + `tools::utils::classify_ai_command_safety` | `extract` | RC-09 |
| `src/internal/ai/session/jsonl.rs:24-26,424` | `context_budget::*` / `goal::GoalEventEnvelope` / `runtime::PlanExecutionRepairState` / `runtime::event::Event` | `extract` | RC-08（Event 先由 RC-01 外迁，jsonl 在 RC-08 脱离 runtime/goal/context_budget） |
| `src/internal/ai/permission/inheritance.rs:28` | `agent::profile::AgentPermissionSpec` | `extract` | RC-19 |
| `src/internal/ai/completion/request.rs:11` | `tools::ToolDefinition` | `extract` | RC-19 |
| `src/internal/ai/runtime/fix_control.rs:16-18` | Code control HTTP | `delete-with-RC-10`（`--fix`，非捕获主线） | RC-10 |
| `src/command/agent/review.rs` `--fix` | `runtime::REVIEW_FIX_*` / control | `cli-only` 然后删桥 | RC-10 |
| `src/command/agent/session.rs` `promote --as-intent` | `history` / `AI_REF` | `cli-only` | RC-11 |
| `src/cli.rs` `Commands::Graph` | `command::graph` | `cli-only` | RC-12 |
| `src/cli.rs` `Commands::Code` | `command::code` | `cli-only` | RC-13 |
| `src/cli.rs` `Commands::Usage` | `command::usage` | `cli-only` | RC-15 |
| `src/cli.rs` `Commands::Publish` | `command::publish` | `cli-only` | RC-33 |
| `src/command/clone.rs:48,66,1997,2479` | `PublishStorage` / `CloudPublishSource` / `libra+cloud` | `cli-only`（改写 clone） | RC-34 |
| `src/command/publish.rs:42,1429` | `projection::ProjectionRebuilder` | `delete-after-importer-gone` | RC-35 后 RC-23 |
| `src/internal/publish/ai_export.rs:24` | `LiveContextPinKind` / `MaterializedProjection` | `delete-after-importer-gone` | RC-35 |
| `src/command/cloud.rs:2029` | `create_publish_storage` | `delete-after-importer-gone` | RC-35 |
| `src/internal/ai/sources/mod.rs:17` | `tools::` | `delete-after-importer-gone` | RC-23 收薄 |
| `src/internal/ai/sources/config.rs:16` | `mcp::server` | `delete-after-importer-gone` | RC-23 |
| `src/internal/ai/web/**` ↔ `command::code` / `runtime` / `mcp` | SCC | `delete-after-importer-gone` | RC-23 |
| `worker/` | 版本面 + 模板 | `delete-after-importer-gone` | RC-36 |

## ADR-RC-05 抽缝对账

| ADR 项 | 卡 | 本清单边 | 状态 |
|---|---|---|---|
| 1 Event trait | RC-01 | `hooks/lifecycle.rs:25` | 对齐 |
| 2 traces API | RC-02 | `history.rs` traces 半边 | 对齐 |
| 3 isolated workspace | RC-03 | `review/mod.rs:74` | 对齐 |
| 4 ToolCallRecord | RC-04 | `derived.rs:78` | 对齐 |
| 5 workspace types | RC-07 | `review/runner.rs` / `investigate/runner.rs` | 对齐 |
| 6 session/jsonl | RC-08 | `session/jsonl.rs:24-26,424` | 对齐 |
| 7 hardening / Shell classify | RC-09 | `sandbox` + `automation/executor.rs` | 对齐 |
| 8 permission/completion | RC-19 | `permission/inheritance.rs` / `completion/request.rs` | 对齐 |
| 9 publish projection | ~~RC-17~~ | **已撤销**（ADR-RC-09）；边改 `delete-after-importer-gone` | 对齐 |
| fix_control | RC-10 | `runtime/fix_control.rs` | 对齐（非抽缝） |

未发现未登记的 KEEP 生产 import。`session/jsonl.rs:26` 的 `runtime::event::Event` 由 RC-01 先迁 trait、RC-08 再脱离 `runtime` 模块。

## ADR-RC-07 复核

| 表面 | 成稿判定 | 现场 `rg` | 结论 |
|---|---|---|---|
| `libra usage` + `usage/` | 删除 | `UsageRecorder` / `INSERT INTO agent_usage_stats` 仅 `usage/recorder.rs` + `command/{code,usage}.rs` + `web/` + `agent/runtime/*`。捕获/hooks/`command/agent` 无写入 | 一致 |
| `libra_vcs` / `workspace_snapshot` / `generated_artifacts` | 删除 | KEEP 不 import | 一致 |
| `package` / `capability_package` | 删除 | `cli.rs` 无 `Commands::Package` | 一致 |
| `automation` | 保留 | `add`/`commit`/`push`/`branch`/`switch` 仍 `dispatch_current_repo_vcs_event_to_history` | 一致 |
| `sandbox` | 保留（先抽 hardening） | 生产仍 `use runtime::hardening` | 一致 |
| `service` | 保留（先抽锁） | `code_control_files` | 一致 |
| `projection/` | 先抽后删 → **改为删除** | publish 仍 `use`；ADR-RC-09 不再抽 | 已由 ADR-RC-09 改写，无需再改 ADR |
| `sources/security`+`resolver` | 保留 | sandbox/hooks/automation 用 | 一致 |

## ADR-RC-09 复核

| 表面 | 判定 | 现场 |
|---|---|---|
| `libra publish` | `cli-only` → RC-35 删文件 | `cli.rs:724-725` |
| `clone libra+cloud://` | `cli-only` 改写 | `clone.rs:2479` |
| `worker/` | RC-36 删树 | 整树已删；版本面改为四处 |
| `libra cloud` | KEEP | `create_r2_storage` 留下；只删 `create_publish_storage` |
| `upgrade_publish_contract_test` | KEEP | 不在本清单删除集 |

## RC-16 `command_test` `mod` 清单

`tests/command/mod.rs`：`code_control_files_test` / `code_control_stdio_test` / `code_test` / `graph_test` / `publish_test` / `usage_help_test`。另有 `mod code_*` 须在开工时按卡体现场 `rg` 补全。

## `#[cfg(test)]`

上表生产边已剔除测试模块内 import（例如 `review/runner.rs` / `investigate/runner.rs` 的 `#[cfg(test)]` `TaskWorkspaceBackend`、`session/jsonl.rs` 测试里的 `Event`）。RC-08 仍须改 jsonl **生产**路径上的 `PlanExecutionRepairState` / `GoalEventEnvelope` / `context_budget` / `Event`。
