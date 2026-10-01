use rudy_core::diagnostics::{payload_error_prefix, SerialLogAnalyzer};

#[test]
fn test_serial_log_analyzer_detects_kernel_panic() {
    let raw_log = r#"
[    0.000000] Linux version 6.10.0 (fedora)
[    1.234567] Kernel panic - not syncing: VFS: Unable to mount root fs on unknown-block(0,0)
[    1.234580] CPU: 0 PID: 1 Comm: swapper/0 Not tainted
[    1.234590] Call Trace:
[    1.234600]  dump_stack_lvl+0x44/0x60
"#;

    let errors = SerialLogAnalyzer::analyze(raw_log);
    assert!(!errors.is_empty());
    assert!(errors.iter().any(|e| e.contains("Kernel panic")));
}

#[test]
fn test_serial_log_analyzer_detects_payload_failures_by_prefix() {
    // Matched on the prefix, not the wording, so the payload can say
    // more without an edit here.
    let raw_log = "rudy: menu starting\n\
                   rudy: error: no supported boot layout in /debian.iso\n";

    let errors = SerialLogAnalyzer::analyze(raw_log);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].starts_with(payload_error_prefix()));
    assert!(errors[0].contains("/debian.iso"));
}
