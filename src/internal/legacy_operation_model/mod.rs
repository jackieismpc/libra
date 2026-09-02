//! Legacy SeaORM entities kept until the v1 operation service is removed by OL-15.
//!
//! These entities are deliberately outside `internal::model`: the live
//! database schema is v2.  The remaining v1 service tests and compatibility
//! code can still compile while the service is retired in a later task.

pub mod operation;
pub mod operation_parent;
pub mod operation_view;
pub mod operation_view_ref;
pub mod operation_view_workspace;
