//! One-time migration of configuration and state from legacy app paths to the
//! current `ri` paths.
//!
//! This runs at startup, before any other code reads the directories, so that
//! the rest of the application can work purely against the new paths.
//!
//! ## What is migrated
//!
//! Legacy apps: `tau` (ancient predecessor) and `xi` (upstream xi-agent). Both
//! are migrated to the `ri` directory layout.
//!
//! ### XDG / platform dirs (`{tau,xi}` → `ri` app name)
//! - `<config_dir>/config.toml`
//! - `<config_dir>/tools/`
//! - `<data_dir>/sessions/`
//!
//! (`auth.toml` is deliberately not migrated — ri no longer uses an OAuth
//! token store. Debug-log cache files are also skipped — they are ephemeral.)
//!
//! ### Home dot-directory (`~/.{tau,xi}/` → `~/.ri/`)
//! - `AGENTS.md`
//! - `skills/`
//! - `tools/`
//!
//! ## Behaviour
//! - Only migrates a source item when the *destination* does not yet exist.
//! - Unknown files / directories under a legacy home dot-directory are left in
//!   place.
//! - After all known items have been moved, the legacy home dot-directory is
//!   removed only if it is empty.
//! - Errors are logged at debug level and silently ignored — a failed
//!   migration is never fatal.

use std::path::{Path, PathBuf};

use directories::ProjectDirs;

// ── Public entry point ────────────────────────────────────────────────────────

/// Run the migration.  Safe to call on every startup — it is a no-op once the
/// legacy paths are gone or the new paths already exist.
pub fn run() {
    migrate_xdg();
    migrate_home_dot_dir();
}

// ── XDG migration ─────────────────────────────────────────────────────────────
//
// Legacy app names in priority order.  Each is migrated into the canonical `ri`
// directory layout.  `move_item` skips anything whose destination already
// exists, so later sources cannot clobber earlier ones.

const LEGACY_APP_NAMES: &[&str] = &["tau", "xi"];

fn migrate_xdg() {
    let Some(new_dirs) = ProjectDirs::from("", "", "ri") else {
        return;
    };

    for app in LEGACY_APP_NAMES {
        let Some(old_dirs) = ProjectDirs::from("", "", app) else {
            continue;
        };

        // config dir: config.toml, tools/
        move_item(
            &old_dirs.config_dir().join("config.toml"),
            &new_dirs.config_dir().join("config.toml"),
        );
        move_item(
            &old_dirs.config_dir().join("tools"),
            &new_dirs.config_dir().join("tools"),
        );

        // data dir: sessions/ (auth.toml is intentionally not migrated)
        move_item(
            &old_dirs.data_dir().join("sessions"),
            &new_dirs.data_dir().join("sessions"),
        );
    }
}

// ── Home dot-directory migration ──────────────────────────────────────────────

const LEGACY_HOME_DIR_NAMES: &[&str] = &[".tau", ".xi"];

fn migrate_home_dot_dir() {
    let Some(home) = std::env::var_os("HOME").filter(|s| !s.is_empty()) else {
        return;
    };
    let new_base = PathBuf::from(&home).join(".ri");

    for legacy in LEGACY_HOME_DIR_NAMES {
        let old_base = PathBuf::from(&home).join(legacy);
        if !old_base.exists() {
            continue;
        }

        // Known items to migrate.
        for name in &["AGENTS.md", "skills", "tools"] {
            move_item(&old_base.join(name), &new_base.join(name));
        }

        // Remove the old directory if it is now empty.
        if std::fs::read_dir(&old_base).is_ok_and(|mut e| e.next().is_none()) {
            match std::fs::remove_dir(&old_base) {
                Ok(()) => log::debug!("migrate: removed empty {}", old_base.display()),
                Err(e) => {
                    log::debug!(
                        "migrate: could not remove empty {}: {e}",
                        old_base.display()
                    )
                }
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Move `src` to `dst`, creating parent directories as needed.
///
/// Does nothing if `src` does not exist or `dst` already exists.
fn move_item(src: &Path, dst: &Path) {
    if !src.exists() || dst.exists() {
        return;
    }

    if dst
        .parent()
        .is_some_and(|p| std::fs::create_dir_all(p).is_err())
    {
        log::debug!("migrate: could not create parent of {}", dst.display());
        return;
    }

    match std::fs::rename(src, dst) {
        Ok(()) => {
            log::debug!("migrate: moved {} → {}", src.display(), dst.display());
        }
        Err(e) => {
            log::debug!(
                "migrate: could not move {} → {}: {e}",
                src.display(),
                dst.display()
            );
        }
    }
}
