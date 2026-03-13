use std::process::Command;

use tempfile::tempdir;

#[test]
fn binary_runs_config_validate() {
    let dir = tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "refresh_seconds = 0\n").expect("config");

    let output = Command::new(env!("CARGO_BIN_EXE_giggity"))
        .arg("--config")
        .arg(&config_path)
        .arg("config")
        .arg("validate")
        .output()
        .expect("run giggity");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("refresh_seconds should be at least 1"));
}
