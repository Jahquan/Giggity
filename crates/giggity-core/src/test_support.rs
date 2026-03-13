use std::ffi::OsString;

pub struct EnvVarGuard {
    original: Vec<(String, Option<OsString>)>,
}

impl EnvVarGuard {
    pub fn set(name: impl Into<String>, value: impl Into<OsString>) -> Self {
        Self::set_many([(name.into(), Some(value.into()))])
    }

    pub fn set_many<I, K, V>(vars: I) -> Self
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
mod tests {
    use super::EnvVarGuard;

    #[test]
    fn env_var_guard_sets_and_restores_values() {
        let original = std::env::var_os("GIGGITY_CORE_TEST_ENV");
        {
            let _guard = EnvVarGuard::set("GIGGITY_CORE_TEST_ENV", "set");
            assert_eq!(std::env::var("GIGGITY_CORE_TEST_ENV").as_deref(), Ok("set"));
        }
        assert_eq!(std::env::var_os("GIGGITY_CORE_TEST_ENV"), original);
    }

    #[test]
    fn env_var_guard_can_remove_and_restore_values() {
        let original = std::env::var_os("PATH");
        {
            let _guard = EnvVarGuard::set_many([("PATH", None::<std::ffi::OsString>)]);
            assert!(std::env::var_os("PATH").is_none());
        }
        assert_eq!(std::env::var_os("PATH"), original);
    }
}
