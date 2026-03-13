#[cfg(unix)]
mod tests {
    use std::process::{Command, Stdio};
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use tempfile::tempdir;

    fn wait_for_path(path: &std::path::Path, timeout: Duration) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if path.exists() {
                return;
            }
            sleep(Duration::from_millis(100));
        }
        panic!("path did not appear in time: {}", path.display());
    }

    fn wait_for_exit(
        child: &mut std::process::Child,
        timeout: Duration,
    ) -> std::process::ExitStatus {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if let Some(status) = child.try_wait().expect("poll child") {
                return status;
            }
            sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        panic!("child did not exit in time");
    }

    fn daemon_exits_cleanly_on_signal(signal: &str) {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let socket_path = dir.path().join("giggity.sock");
        std::fs::write(
            &config_path,
            format!(
                "cache_dir = '{}'\nsocket_path = '{}'\nrefresh_seconds = 1\n[sources]\ndocker = false\npodman = false\nnerdctl = false\nhost_listeners = false\nlaunchd = false\nsystemd = false\n",
                dir.path().display(),
                socket_path.display()
            ),
        )
        .expect("config");

        let mut child = Command::new(env!("CARGO_BIN_EXE_giggity"))
            .arg("--config")
            .arg(&config_path)
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn daemon");

        wait_for_path(&socket_path, Duration::from_secs(5));

        let status = Command::new("/bin/kill")
            .arg(signal)
            .arg(child.id().to_string())
            .status()
            .expect("send signal");
        assert!(status.success());

        let status = wait_for_exit(&mut child, Duration::from_secs(5));
        assert!(status.success(), "daemon exited unsuccessfully: {status}");

        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            if !socket_path.exists() {
                return;
            }
            sleep(Duration::from_millis(50));
        }
        panic!("socket was not cleaned up after shutdown");
    }

    #[test]
    fn binary_daemon_exits_cleanly_on_sigterm() {
        daemon_exits_cleanly_on_signal("-TERM");
    }

    #[test]
    fn binary_daemon_exits_cleanly_on_sigint() {
        daemon_exits_cleanly_on_signal("-INT");
    }
}
