use std::path::{Path, PathBuf};

use anyhow::Context;

pub async fn install_service(config_path: &Path, activate: bool) -> anyhow::Result<PathBuf> {
    let current_exe = std::env::current_exe().context("resolving current executable")?;
    install_service_for_platform(&current_exe, config_path, activate).await
}

#[cfg(target_os = "macos")]
async fn install_service_for_platform(
    current_exe: &Path,
    config_path: &Path,
    activate: bool,
) -> anyhow::Result<PathBuf> {
    install_launchd_service(current_exe, config_path, activate).await
}

#[cfg(target_os = "linux")]
async fn install_service_for_platform(
    current_exe: &Path,
    config_path: &Path,
    activate: bool,
) -> anyhow::Result<PathBuf> {
    install_systemd_service(current_exe, config_path, activate).await
}

async fn install_launchd_service(
    current_exe: &Path,
    config_path: &Path,
    activate: bool,
) -> anyhow::Result<PathBuf> {
    let home = PathBuf::from(std::env::var("HOME").context("HOME not set")?);
    install_launchd_service_at(&home, current_exe, config_path, activate).await
}

async fn install_launchd_service_at(
    home: &Path,
    current_exe: &Path,
    config_path: &Path,
    activate: bool,
) -> anyhow::Result<PathBuf> {
    let path = launchd_destination(home);
    write_service_file(&path, &launchd_plist(current_exe, config_path)).await?;
    if activate {
        activate_launchd(Path::new("launchctl"), &uid()?, &path).await;
    }
    Ok(path)
}

#[cfg(target_os = "linux")]
async fn install_systemd_service(
    current_exe: &Path,
    config_path: &Path,
    activate: bool,
) -> anyhow::Result<PathBuf> {
    let home = PathBuf::from(std::env::var("HOME").context("HOME not set")?);
    install_systemd_service_at(&home, current_exe, config_path, activate).await
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
async fn install_systemd_service_at(
    home: &Path,
    current_exe: &Path,
    config_path: &Path,
    activate: bool,
) -> anyhow::Result<PathBuf> {
    let path = systemd_destination(home);
    write_service_file(&path, &systemd_unit(current_exe, config_path)).await?;
    if activate {
        activate_systemd(Path::new("systemctl")).await;
    }
    Ok(path)
}

fn uid() -> anyhow::Result<String> {
    if let Ok(uid) = std::env::var("UID")
        && !uid.trim().is_empty()
    {
        return Ok(uid);
    }

    let output = std::process::Command::new("id")
        .arg("-u")
        .output()
        .context("running 'id -u' to determine the current uid")?;
    if !output.status.success() {
        anyhow::bail!("id -u exited with {}", output.status);
    }

    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if uid.is_empty() {
        anyhow::bail!("id -u returned an empty uid");
    }
    Ok(uid)
}

fn launchd_destination(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents/com.giggity.daemon.plist")
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn systemd_destination(home: &Path) -> PathBuf {
    home.join(".config/systemd/user/giggity.service")
}

async fn write_service_file(path: &Path, content: &str) -> anyhow::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, content).await?;
    Ok(())
}

async fn activate_launchd(command: &Path, uid: &str, plist_path: &Path) {
    let _ = tokio::process::Command::new(command)
        .args([
            "bootstrap",
            &format!("gui/{uid}"),
            &plist_path.display().to_string(),
        ])
        .status()
        .await;
    let _ = tokio::process::Command::new(command)
        .args(["kickstart", "-k", &format!("gui/{uid}/com.giggity.daemon")])
        .status()
        .await;
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
async fn activate_systemd(command: &Path) {
    let _ = tokio::process::Command::new(command)
        .args(["--user", "daemon-reload"])
        .status()
        .await;
    let _ = tokio::process::Command::new(command)
        .args(["--user", "enable", "--now", "giggity.service"])
        .status()
        .await;
}

fn launchd_plist(current_exe: &Path, config_path: &Path) -> String {
    let current_exe = escape_xml(&current_exe.display().to_string());
    let config_path = escape_xml(&config_path.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.giggity.daemon</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
    <string>daemon</string>
    <string>--config</string>
    <string>{}</string>
  </array>
  <key>KeepAlive</key>
  <true/>
  <key>RunAtLoad</key>
  <true/>
</dict>
</plist>
"#,
        current_exe, config_path
    )
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn systemd_unit(current_exe: &Path, config_path: &Path) -> String {
    let current_exe = systemd_quote_arg(current_exe);
    let config_path = systemd_quote_arg(config_path);
    format!(
        r#"[Unit]
Description=Giggity developer dashboard daemon

[Service]
ExecStart={} daemon --config {}
Restart=always

[Install]
WantedBy=default.target
"#,
        current_exe, config_path
    )
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn systemd_quote_arg(path: &Path) -> String {
    let escaped = path
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)]
mod tests {
    use std::path::Path;
    use std::sync::{Mutex, OnceLock};

    use tempfile::tempdir;

    use super::{
        activate_launchd, activate_systemd, install_launchd_service, install_launchd_service_at,
        install_service, install_systemd_service_at, launchd_destination, launchd_plist,
        systemd_destination, systemd_unit, uid, write_service_file,
    };
    use crate::test_support::EnvVarGuard;

    static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    #[test]
    fn launchd_plist_contains_expected_arguments() {
        let plist = launchd_plist(
            Path::new("/tmp/gig&gity"),
            Path::new("/tmp/config<demo>.toml"),
        );
        assert!(plist.contains("com.giggity.daemon"));
        assert!(plist.contains("/tmp/gig&amp;gity"));
        assert!(plist.contains("/tmp/config&lt;demo&gt;.toml"));
        assert!(!plist.contains("StandardOutPath"));
        assert!(!plist.contains("StandardErrorPath"));
    }

    #[test]
    fn systemd_unit_contains_expected_execstart() {
        let unit = systemd_unit(
            Path::new("/opt/giggity app"),
            Path::new("/tmp/config path.toml"),
        );
        assert!(
            unit.contains(
                "ExecStart=\"/opt/giggity app\" daemon --config \"/tmp/config path.toml\""
            )
        );
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn service_destinations_are_derived_from_home() {
        let home = Path::new("/tmp/home");
        assert_eq!(
            launchd_destination(home),
            Path::new("/tmp/home/Library/LaunchAgents/com.giggity.daemon.plist")
        );
        assert_eq!(
            systemd_destination(home),
            Path::new("/tmp/home/.config/systemd/user/giggity.service")
        );
    }

    #[tokio::test]
    async fn write_service_file_creates_parent_directories() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("nested/service/file.txt");
        write_service_file(&path, "hello").await.expect("write");
        let written = tokio::fs::read_to_string(&path).await.expect("read");
        assert_eq!(written, "hello");
    }

    #[tokio::test]
    async fn write_service_file_supports_relative_paths_without_parents() {
        let _guard = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock");
        let dir = tempdir().expect("tempdir");
        let original = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(dir.path()).expect("set cwd");

        let result = write_service_file(Path::new("service.txt"), "hello").await;
        let restore = std::env::set_current_dir(&original);

        result.expect("write");
        restore.expect("restore cwd");
        let written = tokio::fs::read_to_string(dir.path().join("service.txt"))
            .await
            .expect("read");
        assert_eq!(written, "hello");
    }

    #[tokio::test]
    async fn install_launchd_service_writes_expected_plist() {
        let dir = tempdir().expect("tempdir");
        let path = install_launchd_service_at(
            dir.path(),
            Path::new("/tmp/giggity"),
            Path::new("/tmp/config.toml"),
            false,
        )
        .await
        .expect("install");
        let content = tokio::fs::read_to_string(&path).await.expect("read");
        assert!(content.contains("com.giggity.daemon"));
        assert!(content.contains("/tmp/config.toml"));
    }

    #[tokio::test]
    async fn install_service_wrapper_uses_current_platform() {
        let _guard = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock");
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "").expect("config");
        let _env = EnvVarGuard::set("HOME", dir.path().as_os_str().to_os_string());

        let path = install_service(&config_path, false).await.expect("install");
        assert_eq!(
            path,
            dir.path()
                .join("Library/LaunchAgents/com.giggity.daemon.plist")
        );
        assert!(path.exists());
    }

    #[tokio::test]
    async fn install_launchd_service_wrapper_resolves_home_env() {
        let _guard = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock");
        let dir = tempdir().expect("tempdir");
        let _env = EnvVarGuard::set("HOME", dir.path().as_os_str().to_os_string());

        let path = install_launchd_service(
            Path::new("/tmp/giggity"),
            Path::new("/tmp/config.toml"),
            false,
        )
        .await
        .expect("install");
        assert_eq!(
            path,
            dir.path()
                .join("Library/LaunchAgents/com.giggity.daemon.plist")
        );
    }

    #[tokio::test]
    async fn install_systemd_service_writes_expected_unit() {
        let dir = tempdir().expect("tempdir");
        let path = install_systemd_service_at(
            dir.path(),
            Path::new("/tmp/giggity app"),
            Path::new("/tmp/config path.toml"),
            false,
        )
        .await
        .expect("install");
        let content = tokio::fs::read_to_string(&path).await.expect("read");
        assert!(
            content.contains(
                "ExecStart=\"/tmp/giggity app\" daemon --config \"/tmp/config path.toml\""
            )
        );
        assert!(content.contains("Restart=always"));
    }

    #[tokio::test]
    async fn install_service_activation_paths_invoke_platform_commands() {
        let _guard = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock");
        let dir = tempdir().expect("tempdir");
        let launchctl = dir.path().join("launchctl");
        let systemctl = dir.path().join("systemctl");
        std::fs::write(
            &launchctl,
            "#!/bin/sh\nDIR=${0%/*}\necho launchctl:$@ >> \"$DIR/calls.log\"\n",
        )
        .expect("launchctl");
        std::fs::write(
            &systemctl,
            "#!/bin/sh\nDIR=${0%/*}\necho systemctl:$@ >> \"$DIR/calls.log\"\n",
        )
        .expect("systemctl");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [&launchctl, &systemctl] {
                let mut perms = std::fs::metadata(path).expect("metadata").permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(path, perms).expect("chmod");
            }
        }

        let _env = EnvVarGuard::set_many([
            ("PATH", Some(dir.path().as_os_str().to_os_string())),
            ("UID", Some(std::ffi::OsString::from("501"))),
        ]);

        install_launchd_service_at(
            dir.path(),
            Path::new("/tmp/giggity"),
            Path::new("/tmp/config.toml"),
            true,
        )
        .await
        .expect("launchd activate");
        install_systemd_service_at(
            dir.path(),
            Path::new("/tmp/giggity"),
            Path::new("/tmp/config.toml"),
            true,
        )
        .await
        .expect("systemd activate");

        let calls = tokio::fs::read_to_string(dir.path().join("calls.log"))
            .await
            .expect("calls");
        assert!(calls.contains("launchctl:bootstrap gui/501"));
        assert!(calls.contains("systemctl:--user daemon-reload"));
    }

    #[tokio::test]
    async fn activate_launchd_invokes_expected_commands() {
        let dir = tempdir().expect("tempdir");
        let script = dir.path().join("launchctl");
        std::fs::write(
            &script,
            "#!/bin/sh\nDIR=${0%/*}\necho \"$@\" >> \"$DIR/calls.log\"\n",
        )
        .expect("script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).expect("metadata").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).expect("chmod");
        }

        activate_launchd(&script, "501", Path::new("/tmp/com.giggity.daemon.plist")).await;
        let calls = tokio::fs::read_to_string(dir.path().join("calls.log"))
            .await
            .expect("calls");
        assert!(calls.contains("bootstrap gui/501 /tmp/com.giggity.daemon.plist"));
        assert!(calls.contains("kickstart -k gui/501/com.giggity.daemon"));
    }

    #[test]
    fn uid_prefers_uid_env_and_falls_back_to_id_command() {
        let _guard = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock");
        {
            let _env = EnvVarGuard::set("UID", "1234");
            assert_eq!(uid().expect("uid"), "1234");
        }

        let dir = tempdir().expect("tempdir");
        let script = dir.path().join("id");
        std::fs::write(&script, "#!/bin/sh\nprintf '4321\\n'\n").expect("script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).expect("metadata").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).expect("chmod");
        }
        let _env = EnvVarGuard::set_many([
            ("UID", None::<std::ffi::OsString>),
            ("PATH", Some(dir.path().as_os_str().to_os_string())),
        ]);
        assert_eq!(uid().expect("fallback uid"), "4321");
    }

    #[test]
    fn uid_errors_when_id_command_fails() {
        let _guard = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock");
        let dir = tempdir().expect("tempdir");
        let script = dir.path().join("id");
        std::fs::write(&script, "#!/bin/sh\necho nope >&2\nexit 1\n").expect("script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).expect("metadata").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).expect("chmod");
        }
        let _env = EnvVarGuard::set_many([
            ("UID", None::<std::ffi::OsString>),
            ("PATH", Some(dir.path().as_os_str().to_os_string())),
        ]);
        let error = uid().expect_err("id command failure");
        assert!(error.to_string().contains("id -u exited"));
    }

    #[test]
    fn uid_errors_when_id_command_returns_empty_output() {
        let _guard = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock");
        let dir = tempdir().expect("tempdir");
        let script = dir.path().join("id");
        std::fs::write(&script, "#!/bin/sh\nprintf ''\n").expect("script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).expect("metadata").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).expect("chmod");
        }
        let _env = EnvVarGuard::set_many([
            ("UID", None::<std::ffi::OsString>),
            ("PATH", Some(dir.path().as_os_str().to_os_string())),
        ]);
        let error = uid().expect_err("empty uid");
        assert!(error.to_string().contains("empty uid"));
    }

    #[tokio::test]
    async fn activate_systemd_invokes_expected_commands() {
        let dir = tempdir().expect("tempdir");
        let script = dir.path().join("systemctl");
        std::fs::write(
            &script,
            "#!/bin/sh\nDIR=${0%/*}\necho \"$@\" >> \"$DIR/calls.log\"\n",
        )
        .expect("script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).expect("metadata").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).expect("chmod");
        }

        activate_systemd(&script).await;
        let calls = tokio::fs::read_to_string(dir.path().join("calls.log"))
            .await
            .expect("calls");
        assert!(calls.contains("--user daemon-reload"));
        assert!(calls.contains("--user enable --now giggity.service"));
    }
}
