//! mtime-tracked config hot-reload.
//!
//! The caller owns the last-good [`Settings`]: [`ConfigWatcher::reload`] either
//! returns fresh settings or raises [`ConfigError`], leaving the previous value
//! untouched.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::adapters::config::{ConfigError, load_settings};
use crate::domain::settings::Settings;

fn mtime_ns(path: &Path) -> Option<u128> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(
        modified
            .duration_since(UNIX_EPOCH)
            .map_or(0, |delta| delta.as_nanos()),
    )
}

fn cannot_read(path: &Path, error: &std::io::Error) -> ConfigError {
    ConfigError::new(format!("config: cannot read {}: {error}", path.display()))
}

/// Reloads only when the file's mtime moves; a vanished file counts as changed.
#[derive(Debug, Clone)]
pub struct ConfigWatcher {
    path: PathBuf,
    mtime_ns: u128,
}

impl ConfigWatcher {
    /// Capture the file's current mtime.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the file cannot be stat'ed.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref().to_path_buf();
        let modified = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .map_err(|error| cannot_read(&path, &error))?;
        let mtime_ns = modified
            .duration_since(UNIX_EPOCH)
            .map_or(0, |delta| delta.as_nanos());
        Ok(Self { path, mtime_ns })
    }

    /// Whether the file's mtime moved since the last load.
    pub fn changed(&self) -> bool {
        mtime_ns(&self.path) != Some(self.mtime_ns)
    }

    /// Reload settings, recording the mtime read before parsing.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the file is unreadable or invalid.
    pub fn reload(&mut self) -> Result<Settings, ConfigError> {
        // Stat before load so the recorded mtime never describes a version newer
        // than what was parsed: a write racing the load leaves the file's mtime
        // ahead of ours, and changed() schedules another reload.
        let stat_before = mtime_ns(&self.path);
        let settings = load_settings(&self.path)?;
        if let Some(mtime_ns) = stat_before {
            self.mtime_ns = mtime_ns;
        }
        Ok(settings)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::domain::settings::default_curve;

    const IDENTITY: &str = "[settings]\nhost = \"h\"\nuser = \"u\"\n";

    fn bump(path: &Path, offset_ns: u128) {
        let current = fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let target = current + 1_000_000 + offset_ns;
        let time = UNIX_EPOCH + std::time::Duration::from_nanos(u64::try_from(target).unwrap());
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    #[test]
    fn hot_reload_picks_up_mtime_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hot.toml");
        fs::write(&path, "[settings]\nhost = \"first\"\nuser = \"u\"\n").unwrap();
        let mut watcher = ConfigWatcher::new(&path).unwrap();
        assert!(!watcher.changed());
        assert_eq!(watcher.reload().unwrap().host(), "first");

        fs::write(&path, "[settings]\nhost = \"second\"\nuser = \"u\"\n").unwrap();
        bump(&path, 0);
        assert!(watcher.changed());
        assert_eq!(watcher.reload().unwrap().host(), "second");
        assert!(!watcher.changed());
    }

    #[test]
    fn failed_reload_keeps_last_good_settings_usable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reload.toml");
        fs::write(&path, "[settings]\nhost = \"good\"\nuser = \"u\"\n").unwrap();
        let mut watcher = ConfigWatcher::new(&path).unwrap();
        let last_good = watcher.reload().unwrap();

        fs::write(&path, "[settings]\nhost = ").unwrap();
        bump(&path, 1_000_000);
        assert!(watcher.changed());
        assert!(watcher.reload().is_err());

        fs::write(&path, "[settings]\nhost = \"recovered\"\nuser = \"u\"\n").unwrap();
        bump(&path, 2_000_000);
        assert_eq!(watcher.reload().unwrap().host(), "recovered");
        assert_eq!(last_good.host(), "good");
        assert_eq!(last_good.curve().points(), default_curve().points());
    }

    #[test]
    fn vanished_file_after_load_surfaces_config_error_not_os_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vanish.toml");
        fs::write(&path, IDENTITY).unwrap();
        let mut watcher = ConfigWatcher::new(&path).unwrap();
        watcher.reload().unwrap();
        assert!(!watcher.changed());
        fs::remove_file(&path).unwrap();
        assert!(watcher.changed());
        assert!(watcher.reload().is_err());
    }
}
