-- Operation Log v2 (plan-20260822 OL-02).
--
-- Development-time replacement: the five v1 operation tables are removed and
-- rebuilt as the eight v2 tables.  The migration is intentionally
-- forward-only; repositories that need the old audit rows must export them
-- before upgrading.  No prompt, transcript, secret, or other AI payload is
-- present in this schema.

DROP TABLE IF EXISTS `operation_view_workspace`;
DROP TABLE IF EXISTS `operation_view_ref`;
DROP TABLE IF EXISTS `operation_view`;
DROP TABLE IF EXISTS `operation_journal`;
DROP TABLE IF EXISTS `operation_head`;
DROP TABLE IF EXISTS `operation_parent`;
DROP TABLE IF EXISTS `operation`;
DROP TABLE IF EXISTS `change_identity`;
DROP TABLE IF EXISTS `change_revision`;
DROP TABLE IF EXISTS `change_predecessor`;
DROP TABLE IF EXISTS `ai_operation_link`;

CREATE TABLE `operation` (
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
    `end_ts`              INTEGER
);
CREATE INDEX `idx_operation_repo_order`
    ON `operation`(`repo_id`, `end_ts` DESC, `start_ts` DESC, `op_id` DESC);
CREATE INDEX `idx_operation_repo_scope_order`
    ON `operation`(`repo_id`, `scope_kind`, `end_ts` DESC, `op_id` DESC);

CREATE TABLE `operation_parent` (
    `op_id`        TEXT NOT NULL,
    `parent_op_id` TEXT NOT NULL,
    `ordinal`      INTEGER NOT NULL,
    PRIMARY KEY (`op_id`, `parent_op_id`)
);
CREATE INDEX `idx_operation_parent_parent`
    ON `operation_parent`(`parent_op_id`, `op_id`);

CREATE TABLE `operation_head` (
    `repo_id`    TEXT NOT NULL,
    `scope_key`  TEXT NOT NULL,
    `op_id`      TEXT NOT NULL,
    `generation` INTEGER NOT NULL,
    PRIMARY KEY (`repo_id`, `scope_key`, `op_id`)
);
CREATE INDEX `idx_operation_head_generation`
    ON `operation_head`(`repo_id`, `scope_key`, `generation` DESC);

CREATE TABLE `operation_journal` (
    `journal_id`       TEXT PRIMARY KEY,
    `op_id`            TEXT NOT NULL,
    `phase`            TEXT NOT NULL,
    `pre_view_oid`     TEXT,
    `target_view_oid`  TEXT,
    `owner`            TEXT NOT NULL,
    `updated_at`       INTEGER NOT NULL,
    `recovery_payload` TEXT
);
CREATE INDEX `idx_operation_journal_op`
    ON `operation_journal`(`op_id`, `updated_at` DESC);

CREATE TABLE `change_identity` (
    `change_id`    TEXT PRIMARY KEY,
    `repo_id`      TEXT NOT NULL,
    `origin`       TEXT NOT NULL,
    `created_op_id` TEXT NOT NULL,
    `created_at`   INTEGER NOT NULL
);
CREATE INDEX `idx_change_identity_repo`
    ON `change_identity`(`repo_id`, `created_at` DESC);

CREATE TABLE `change_revision` (
    `change_id`        TEXT NOT NULL,
    `commit_oid`       TEXT NOT NULL,
    `created_op_id`    TEXT NOT NULL,
    `visibility`       TEXT NOT NULL,
    `revision_ordinal` INTEGER NOT NULL,
    PRIMARY KEY (`change_id`, `commit_oid`)
);
CREATE INDEX `idx_change_revision_commit`
    ON `change_revision`(`commit_oid`);

CREATE TABLE `change_predecessor` (
    `successor_oid`   TEXT NOT NULL,
    `predecessor_oid` TEXT NOT NULL,
    `op_id`           TEXT NOT NULL,
    `relation_kind`   TEXT NOT NULL,
    `ordinal`         INTEGER NOT NULL,
    PRIMARY KEY (`successor_oid`, `predecessor_oid`, `op_id`)
);
CREATE INDEX `idx_change_predecessor_predecessor`
    ON `change_predecessor`(`predecessor_oid`, `ordinal`);

CREATE TABLE `ai_operation_link` (
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
    `redaction_version`       TEXT NOT NULL
);
CREATE INDEX `idx_ai_operation_link_repo`
    ON `ai_operation_link`(`repo_id`, `operation_id`);
