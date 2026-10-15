//! Menu bar control surface for EDIFIER headsets on macOS.
//!
//! macOS drives its own Control Center noise-control tile through Apple's proprietary accessory
//! protocol, which the Edifier firmware does not speak, so this app *is* the noise-control panel:
//! a status item that shows the battery level and switches ANC, ambient sound, EQ, game mode and
//! LDAC the way the AirPods panel does.
//!
//! ```text
//! main thread (AppKit)                        bluetooth thread (Tokio)
//! ────────────────────                        ───────────────────────
//! status item + menu  ── Action ───────────▶  Session (edifier_link)
//! UiState snapshot    ◀── SharedState ──────  events + status
//! ```
//!
//! The two sides only share [`menu::SharedState`]; the runtime never touches AppKit, it only asks
//! [`ui::MainQueueWaker`] to redraw.
//!
//! ```text
//! edifier-menubar                        run the menu bar app
//! edifier-menubar --install-launch-agent write the LaunchAgent plist
//! edifier-menubar --uninstall-launch-agent remove it again
//! ```

/// Process wiring: AppKit on the main thread, BLE on a background runtime.
pub mod app;
/// Settings remembered between runs.
pub mod config;
/// LaunchAgent (`--install-launch-agent`) management.
pub mod launch_agent;
/// Discovery, the BLE session and the command dispatch behind every menu item.
pub mod link;
/// The pure view model: state, labels and the tag ↔ action mapping.
pub mod menu;
/// AppKit: the status item, its menu and the click target.
pub mod ui;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

/// `edifier-menubar` — control an EDIFIER headset from the macOS menu bar.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "edifier-menubar",
    version,
    about = "Battery, ANC, EQ, game mode and LDAC in the macOS menu bar"
)]
pub struct Cli {
    /// Write `~/Library/LaunchAgents/sh.edifier.menubar.plist` and print how to load it.
    #[arg(long, conflicts_with = "uninstall_launch_agent")]
    pub install_launch_agent: bool,

    /// Remove the LaunchAgent plist installed by `--install-launch-agent`.
    #[arg(long)]
    pub uninstall_launch_agent: bool,

    /// Use this headset instead of the remembered one: a CoreBluetooth identifier or a name
    /// fragment.
    #[arg(long, value_name = "SELECTOR")]
    pub device: Option<String>,

    /// Raise log verbosity (`-v`, `-vv`); `RUST_LOG` overrides this.
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

/// Run whatever the command line asked for: a LaunchAgent change, or the menu bar app.
///
/// # Errors
///
/// Fails when the LaunchAgent plist cannot be written or removed, when the settings cannot be
/// read, or when the menu bar app cannot start.
pub fn run(cli: &Cli) -> Result<()> {
    if cli.install_launch_agent {
        print!("{}", launch_agent::install()?);
        return Ok(());
    }
    if cli.uninstall_launch_agent {
        print!("{}", launch_agent::uninstall()?);
        return Ok(());
    }
    app::run(cli.device.as_deref())
}

/// Home directory of the current user.
///
/// # Errors
///
/// Fails when `HOME` is not set, which leaves nothing to anchor the settings, logs and LaunchAgent
/// paths to.
pub(crate) fn home() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home))
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Helpers shared by the unit tests.

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A uniquely named temporary directory that removes itself when dropped.
    pub(crate) struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        /// Create the directory under the system temporary directory.
        pub(crate) fn new(tag: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let unique = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "edifier-menubar-{tag}-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("the temporary directory can be created");
            Self { path }
        }

        /// The directory itself.
        pub(crate) fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_are_mutually_exclusive() {
        let cli =
            Cli::try_parse_from(["edifier-menubar", "--install-launch-agent"]).expect("installs");
        assert!(cli.install_launch_agent);
        assert!(!cli.uninstall_launch_agent);
        assert_eq!(cli.device, None);
        assert_eq!(cli.verbose, 0);

        assert!(
            Cli::try_parse_from([
                "edifier-menubar",
                "--install-launch-agent",
                "--uninstall-launch-agent",
            ])
            .is_err(),
            "installing and uninstalling at once is rejected",
        );
    }

    #[test]
    fn running_without_arguments_asks_for_the_app() {
        let cli = Cli::try_parse_from(["edifier-menubar"]).expect("parses");
        assert!(!cli.install_launch_agent);
        assert!(!cli.uninstall_launch_agent);
        assert_eq!(cli.device, None);
    }

    #[test]
    fn device_and_verbosity_flags_are_carried() {
        let cli = Cli::try_parse_from(["edifier-menubar", "--device", "EDIFIER W820NB", "-vv"])
            .expect("parses");
        assert_eq!(cli.device.as_deref(), Some("EDIFIER W820NB"));
        assert_eq!(cli.verbose, 2);
    }
}
