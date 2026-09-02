-- Guarded rollback for 2026090101_operation_v2.
--
-- v2 rows contain immutable object references that have no lossless mapping to
-- the legacy view tables. Refuse to roll back a non-empty v2 database rather
-- than silently deleting operation history.

PRAGMA foreign_keys = OFF;

CREATE TABLE `operation_v2_down_guard` (
    `guard` TEXT NOT NULL CHECK (`guard` = 'empty')
);
INSERT INTO `operation_v2_down_guard` (`guard`)
SELECT 'non-empty'
WHERE EXISTS (SELECT 1 FROM `operation` LIMIT 1)
   OR EXISTS (SELECT 1 FROM `operation_parent` LIMIT 1)
   OR EXISTS (SELECT 1 FROM `operation_head` LIMIT 1)
   OR EXISTS (SELECT 1 FROM `operation_journal` LIMIT 1)
   OR EXISTS (SELECT 1 FROM `change_identity` LIMIT 1)
   OR EXISTS (SELECT 1 FROM `change_revision` LIMIT 1)
   OR EXISTS (SELECT 1 FROM `change_predecessor` LIMIT 1)
   OR EXISTS (SELECT 1 FROM `ai_operation_link` LIMIT 1);
DROP TABLE `operation_v2_down_guard`;

DROP TABLE IF EXISTS `ai_operation_link`;
DROP TABLE IF EXISTS `change_predecessor`;
DROP TABLE IF EXISTS `change_revision`;
DROP TABLE IF EXISTS `change_identity`;
DROP TABLE IF EXISTS `operation_journal`;
DROP TABLE IF EXISTS `operation_head`;
DROP TABLE IF EXISTS `operation_parent`;
DROP TABLE IF EXISTS `operation`;

CREATE TABLE IF NOT EXISTS `operation` (
    `op_id` TEXT PRIMARY KEY,
    `repo_id` TEXT NOT NULL,
    `view_id` TEXT NOT NULL,
    `command_name` TEXT NOT NULL,
    `description` TEXT NOT NULL,
    `actor` TEXT NOT NULL,
    `args_digest` TEXT,
    `start_ts` INTEGER NOT NULL,
    `end_ts` INTEGER,
    `status` TEXT NOT NULL,
    `worktree_id` TEXT NOT NULL DEFAULT '',
    `scope_provenance` TEXT NOT NULL DEFAULT 'declared',
    `restorable` INTEGER NOT NULL DEFAULT 1,
    `control_slot` TEXT,
    `claim_owner` TEXT,
    `scope_kind` TEXT NOT NULL DEFAULT 'main'
);
CREATE INDEX IF NOT EXISTS `idx_operation_repo_order`
    ON `operation`(`repo_id`, `end_ts` DESC, `start_ts` DESC, `op_id` DESC);
CREATE UNIQUE INDEX IF NOT EXISTS `idx_operation_control_slot`
    ON `operation`(`repo_id`, `worktree_id`, `control_slot`)
    WHERE `status` = 'running' AND `control_slot` IS NOT NULL;
CREATE INDEX IF NOT EXISTS `idx_operation_dedup_scope`
    ON `operation`(`repo_id`, `worktree_id`, `command_name`, `args_digest`, `status`, `end_ts`);

CREATE TABLE IF NOT EXISTS `operation_parent` (
    `op_id` TEXT NOT NULL,
    `parent_op_id` TEXT NOT NULL,
    PRIMARY KEY (`op_id`, `parent_op_id`)
);
CREATE INDEX IF NOT EXISTS `idx_operation_parent_parent`
    ON `operation_parent`(`parent_op_id`, `op_id`);

CREATE TABLE IF NOT EXISTS `operation_view` (
    `view_id` TEXT PRIMARY KEY,
    `repo_id` TEXT NOT NULL,
    `head_kind` TEXT NOT NULL,
    `head_target` TEXT NOT NULL,
    `created_at` INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS `idx_operation_view_repo_created`
    ON `operation_view`(`repo_id`, `created_at` DESC);

CREATE TABLE IF NOT EXISTS `operation_view_ref` (
    `view_id` TEXT NOT NULL,
    `ref_kind` TEXT NOT NULL,
    `ref_name` TEXT NOT NULL,
    `ref_remote` TEXT NOT NULL,
    `target_oid` TEXT NOT NULL,
    PRIMARY KEY (`view_id`, `ref_kind`, `ref_name`, `ref_remote`)
);

CREATE TABLE IF NOT EXISTS `operation_view_workspace` (
    `view_id` TEXT NOT NULL,
    `pointer_kind` TEXT NOT NULL,
    `pointer_value` TEXT NOT NULL,
    PRIMARY KEY (`view_id`, `pointer_kind`)
);
