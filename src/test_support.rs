//! Test-only helpers shared across modules.

/// Restores a set of environment variables to what they were when it was created.
///
/// The suite is full of tests that set `HOME`, `AM_DOCKER_BIN` and friends to point at a
/// fixture, then *clear* them again at the end. Clearing is not restoring: it silently
/// discards whatever the developer running the suite had set, and every later test in the
/// binary — they share one process, and `.cargo/config.toml` runs them in a fixed order on a
/// single thread — sees the variable as unset.
///
/// That is invisible on a machine whose defaults happen to match the fallbacks, and it is not
/// invisible anywhere else. A `--features integration` run on a podman-only host had five
/// differential tests fail with `failed to run pull: No such file or directory`, because an
/// earlier test had cleared the `AM_DOCKER_BIN` that pointed them at podman and they fell back
/// to a `/usr/bin/docker` that does not exist there. Each one passed when run on its own.
///
/// Restoring on `Drop` also survives a panicking assertion, which a trailing `remove_var` never
/// reaches — so one failing test can no longer cascade into the rest of the binary.
pub struct EnvGuard(Vec<(&'static str, Option<String>)>);

impl EnvGuard {
    /// Remember the current value of each variable, whether or not it is set.
    pub fn saving(vars: &[&'static str]) -> Self {
        Self(vars.iter().map(|k| (*k, std::env::var(k).ok())).collect())
    }

    /// Remember them, then clear them — for a lookup that must not see the developer's own
    /// configuration at all.
    pub fn clearing(vars: &[&'static str]) -> Self {
        let guard = Self::saving(vars);
        for (key, _) in &guard.0 {
            std::env::remove_var(key);
        }
        guard
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}
