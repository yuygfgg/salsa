#[test]
fn v1_ui_has_no_export_action() {
    for source in [
        include_str!("../src/app.rs"),
        include_str!("../src/ui/output.rs"),
        include_str!("../src/ui/transport.rs"),
    ] {
        assert!(
            !source.contains("\"Export\""),
            "v1 must not expose export controls"
        );
        assert!(
            !source.contains("ExportAction"),
            "v1 must not expose export actions"
        );
    }
}
