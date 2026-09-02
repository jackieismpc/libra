-- OL-02: replace the development-only operation schema with v2.
--
-- The upgrade is guarded: an existing legacy operation table must be empty.
-- The down migration is also guarded and only reconstructs the legacy schema
-- when every v2 table is empty.

PRAGMA foreign_keys = OFF;

-- A repository may be running this migration from the baseline schema, where
-- these legacy tables do not exist yet. Creating empty compatibility shapes
-- makes the non-empty check safe in both cases.
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
    `worktree_id` TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS `operation_parent` (
    `op_id` TEXT NOT NULL,
    `parent_op_id` TEXT NOT NULL,
    PRIMARY KEY (`op_id`, `parent_op_id`)
);
CREATE TABLE IF NOT EXISTS `operation_view` (
    `view_id` TEXT PRIMARY KEY,
    `repo_id` TEXT NOT NULL,
    `head_kind` TEXT NOT NULL,
    `head_target` TEXT NOT NULL,
    `created_at` INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS `operation_view_ref` (
    `view_id` TEXT NOT NULL,
    `ref_name` TEXT NOT NULL,
    `target_oid` TEXT NOT NULL,
    PRIMARY KEY (`view_id`, `ref_name`)
);
CREATE TABLE IF NOT EXISTS `operation_view_workspace` (
    `view_id` TEXT NOT NULL,
    `workspace_name` TEXT NOT NULL,
    `pointer_kind` TEXT NOT NULL,
    `pointer_value` TEXT NOT NULL,
    PRIMARY KEY (`view_id`, `workspace_name`)
);

CREATE TABLE `operation_v2_guard` (
    `guard` TEXT NOT NULL CHECK (`guard` = 'empty')
);
INSERT INTO `operation_v2_guard` (`guard`)
SELECT 'non-empty'
WHERE EXISTS (SELECT 1 FROM `operation` LIMIT 1)
   OR EXISTS (SELECT 1 FROM `operation_parent` LIMIT 1)
   OR EXISTS (SELECT 1 FROM `operation_view` LIMIT 1)
   OR EXISTS (SELECT 1 FROM `operation_view_ref` LIMIT 1)
   OR EXISTS (SELECT 1 FROM `operation_view_workspace` LIMIT 1);
DROP TABLE `operation_v2_guard`;

DROP TABLE IF EXISTS `operation_view_workspace`;
DROP TABLE IF EXISTS `operation_view_ref`;
DROP TABLE IF EXISTS `operation_view`;
DROP TABLE IF EXISTS `operation_parent`;
DROP TABLE IF EXISTS `operation`;

CREATE TABLE IF NOT EXISTS `operation` (
    `op_id`               TEXT PRIMARY KEY,
    `repo_id`             TEXT NOT NULL,
    `format_version`      INTEGER NOT NULL DEFAULT 2,
    `kind`                TEXT NOT NULL,
    `status`              TEXT NOT NULL,
    `command_name`        TEXT,
    `description`         TEXT,
    `args_digest`         TEXT,
    `actor`               TEXT,
    `worktree_id`         TEXT,
    `scope_kind`          TEXT NOT NULL,
    `pre_view_oid`        TEXT NOT NULL,
    `post_view_oid`       TEXT NOT NULL,
    `restores_op_id`      TEXT,
    `reverts_op_id`       TEXT,
    `predecessor_map_oid` TEXT,
    `causal_context_id`   TEXT,
    `start_ts`            INTEGER NOT NULL,
    `end_ts`              INTEGER,
    `scope_provenance`    TEXT NOT NULL DEFAULT 'declared',
    `restorable`          INTEGER NOT NULL DEFAULT 1,
    `control_slot`        TEXT,
    `claim_owner`         TEXT
);
CREATE INDEX IF NOT EXISTS `idx_operation_v2_repo_order`
    ON `operation`(`repo_id`, `end_ts` DESC, `start_ts` DESC, `op_id` DESC);
CREATE UNIQUE INDEX IF NOT EXISTS `idx_operation_v2_control_claim`
    ON `operation`(`repo_id`, `worktree_id`, `control_slot`)
    WHERE `status` = 'running' AND `control_slot` IS NOT NULL;
CREATE INDEX IF NOT EXISTS `idx_operation_dedup_scope`
    ON `operation`(`repo_id`, `worktree_id`, `command_name`, `args_digest`, `status`, `end_ts`);

CREATE TABLE IF NOT EXISTS `operation_parent` (
    `op_id`        TEXT NOT NULL,
    `parent_op_id` TEXT NOT NULL,
    `ordinal`      INTEGER NOT NULL,
    PRIMARY KEY (`op_id`, `parent_op_id`)
);
CREATE INDEX IF NOT EXISTS `idx_operation_parent_v2_parent`
    ON `operation_parent`(`parent_op_id`, `op_id`);

CREATE TABLE IF NOT EXISTS `operation_head` (
    `repo_id`    TEXT NOT NULL,
    `scope_key`  TEXT NOT NULL,
    `op_id`      TEXT NOT NULL,
    `generation` INTEGER NOT NULL,
    PRIMARY KEY (`repo_id`, `scope_key`, `op_id`)
);
CREATE INDEX IF NOT EXISTS `idx_operation_head_v2_scope_generation`
    ON `operation_head`(`repo_id`, `scope_key`, `generation` DESC, `op_id`);

CREATE TABLE IF NOT EXISTS `operation_journal` (
    `journal_id`       TEXT PRIMARY KEY,
    `op_id`            TEXT NOT NULL,
    `phase`            TEXT NOT NULL,
    `pre_view_oid`     TEXT,
    `target_view_oid`  TEXT,
    `owner`            TEXT NOT NULL,
    `updated_at`       INTEGER NOT NULL,
    `recovery_payload` TEXT
);
CREATE INDEX IF NOT EXISTS `idx_operation_journal_v2_op`
    ON `operation_journal`(`op_id`, `updated_at` DESC);

CREATE TABLE IF NOT EXISTS `change_identity` (
    `change_id`     TEXT PRIMARY KEY,
    `repo_id`       TEXT NOT NULL,
    `origin`        TEXT NOT NULL,
    `created_op_id` TEXT NOT NULL,
    `created_at`    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS `change_revision` (
    `change_id`        TEXT NOT NULL,
    `commit_oid`       TEXT NOT NULL,
    `created_op_id`    TEXT NOT NULL,
    `visibility`       TEXT NOT NULL,
    `revision_ordinal` INTEGER NOT NULL,
    PRIMARY KEY (`change_id`, `commit_oid`)
);
CREATE INDEX IF NOT EXISTS `idx_change_revision_v2_commit`
    ON `change_revision`(`commit_oid`);

CREATE TABLE IF NOT EXISTS `change_predecessor` (
    `successor_oid`   TEXT NOT NULL,
    `predecessor_oid` TEXT NOT NULL,
    `op_id`           TEXT NOT NULL,
    `relation_kind`   TEXT NOT NULL,
    `ordinal`         INTEGER NOT NULL,
    PRIMARY KEY (`successor_oid`, `predecessor_oid`, `op_id`)
);

CREATE TABLE IF NOT EXISTS `ai_operation_link` (
    `operation_id`             TEXT PRIMARY KEY,
    `session_id`               TEXT,
    `run_id`                   TEXT,
    `tool_invocation_id`       TEXT,
    `intent_id`                TEXT,
    `repo_id`                  TEXT NOT NULL,
    `worktree_id`              TEXT,
    `workspace_id`             TEXT,
    `lease_generation`         INTEGER,
    `config_provenance_digest` TEXT,
    `redaction_version`        TEXT NOT NULL
);
