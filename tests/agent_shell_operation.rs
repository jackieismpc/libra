//! OL-09 Agent shell mutation classification coverage.

use libra::internal::operation::middleware::{MutationClass, classify_command};

#[test]
fn shell_is_external_and_read_only_tools_remain_read_only() {
    assert_eq!(
        classify_command("external").expect("external"),
        MutationClass::External
    );
    assert_eq!(
        classify_command("status").expect("status"),
        MutationClass::ReadOnly
    );
    assert!(classify_command("unclassified-agent-tool").is_err());
}
