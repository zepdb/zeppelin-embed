//! Controlled-install boundary smoke for the private ZE-45 acceptance suite.

#[test]
fn native_read_controlled_install_boundary_is_test_support_only() {
    assert!(zeppelin_embed::graph_read_view_test_support::controlled_boundary_is_available());
}
