#![cfg(unix)]
#![forbid(unsafe_code)]

use std::process::Command;

#[test]
fn test_command_reports_cargo_success_and_failure() {
    for (code, succeeds) in [("0", true), ("7", false)] {
        let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
            .arg("test")
            .env(
                "CARGO",
                concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cargo.sh"),
            )
            .env("XTASK_CARGO_EXIT", code)
            .output()
            .expect("run xtask test");
        assert_eq!(
            output.status.success(),
            succeeds,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
