use std::path::PathBuf;
use std::process::Stdio;

use tokio::process::Command;
use tracing::debug;

#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::ffi::OsString;
#[cfg(test)]
use std::path::Path;
#[cfg(test)]
use std::sync::{Mutex, OnceLock};

#[cfg(test)]
static COMMAND_OVERRIDES: OnceLock<Mutex<BTreeMap<String, PathBuf>>> = OnceLock::new();
#[cfg(test)]
static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub async fn run_command(context: &str, program: &str, args: &[&str]) -> anyhow::Result<String> {
    debug!(
        collector = context,
        ?program,
        ?args,
        "running collector command"
    );
    let output = Command::new(resolve_program(program))
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "{} failed: {}",
            program,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn resolve_program(program: &str) -> PathBuf {
    #[cfg(test)]
    if let Some(path) = command_overrides()
        .lock()
        .expect("lock")
        .get(program)
        .cloned()
    {
        return path;
    }

    PathBuf::from(program)
}

#[cfg(test)]
pub(crate) fn command_overrides() -> &'static Mutex<BTreeMap<String, PathBuf>> {
    COMMAND_OVERRIDES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[cfg(test)]
pub(crate) fn test_lock() -> &'static Mutex<()> {
    TEST_LOCK.get_or_init(|| Mutex::new(()))
}

#[cfg(test)]
pub(crate) struct EnvVarGuard {
    original: Vec<(String, Option<OsString>)>,
}

#[cfg(test)]
impl EnvVarGuard {
    pub(crate) fn set(name: impl Into<String>, value: impl Into<OsString>) -> Self {
        Self::set_many([(name.into(), Some(value.into()))])
    }

    pub(crate) fn set_many<I, K, V>(vars: I) -> Self
    where
        I: IntoIterator<Item = (K, Option<V>)>,
        K: Into<String>,
        V: Into<OsString>,
    {
        let mut original = Vec::new();

        for (name, value) in vars {
            let name = name.into();
            original.push((name.clone(), std::env::var_os(&name)));
            match value.map(Into::into) {
                Some(value) => unsafe {
                    std::env::set_var(&name, value);
                },
                None => unsafe {
                    std::env::remove_var(&name);
                },
            }
        }

        Self { original }
    }
}

#[cfg(test)]
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        for (name, value) in self.original.drain(..).rev() {
            match value {
                Some(value) => unsafe {
                    std::env::set_var(name, value);
                },
                None => unsafe {
                    std::env::remove_var(name);
                },
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
    }
    path
}
