# `libra op` Development Notes

## Command Goal

`libra op` exposes Libra's command-level operation history. It is a Libra-native
extension rather than a Git command. The current public surface supports:

- `libra op log`
- `libra op show`
- `libra op restore`

## Compatibility

- Tier: `intentionally-different`.
- Rationale: Git has reflog and reset/restore flows, but it does not expose this
  Libra operation-graph model or the command-level restore view used here.

## Implementation

- CLI entry: `src/cli.rs::Commands::Op`.
- Command implementation: `src/command/op.rs`.
- Storage/service layer: `src/internal/operation/store.rs` (v2); the legacy
  command adapter is retained only until the later OL-15 command-path cutover.
- Transaction wrapper: `src/internal/operation_wrapper.rs`.
- Operation tables are installed by the versioned v2 migration when a database
  is created or opened for upgrade.
- OL-02 replaces the development-only v1 operation tables with the v2 tables
  `operation`, `operation_parent`, `operation_head`, `operation_journal`,
  `change_identity`, `change_revision`, `change_predecessor`, and
  `ai_operation_link`. The migration is forward-only because the v1 shape
  cannot represent v2 workspace snapshots or journal state. Export any legacy
  audit data before upgrading if it must be retained.

## Current Behavior

- `op log` lists operations by repository with pagination and exact command
  filtering.
- `op show` resolves an operation id or `@{n}` reference and can print the
  captured view snapshot.
- `op restore` restores HEAD and captured branch refs from a previous operation
  view and records a new successful restore operation. It also **prunes** local
  branches that are absent from the target view, so restore reproduces that
  operation's exact local-branch set rather than only updating named refs. Never
  pruned: the restored HEAD branch, remote-tracking refs, the locked branches
  (`main`/`intent`/`traces`/`agent-traces`), and the reserved `libra/` namespace
  (AI history `libra/intent`, orchestrator `libra/src`/`libra/target`).
  `--dry-run` previews the prune (and the restore) without writing.

## Remaining Gaps

- Broader command coverage is incremental. At present, branch creation is wired
  through operation logging as the first command integration target.
