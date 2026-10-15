//! LaunchAgent install/uninstall for the `--install-launch-agent` and `--uninstall-launch-agent`
//! flags.
//!
//! Only the plist is written; loading it stays a deliberate, visible step, so the flag prints the
//! exact `launchctl` commands instead of silently bootstrapping a login item.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// LaunchAgent label, also used as the file name stem.
pub const LABEL: &str = "sh.edifier.menubar";
/// Log file the agent's stdout is redirected to.
pub const STDOUT_LOG: &str = "edifier-menubar.out.log";
/// Log file the agent's stderr is redirected to.
pub const STDERR_LOG: &str = "edifier-menubar.err.log";

/// Outcome of [`install`] or [`uninstall`], rendered by the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Where the LaunchAgent plist lives.
    pub plist_path: PathBuf,
    /// Whether the plist exists after the call.
    pub present: bool,
    /// Directory of the agent's log files, when installing.
    pub log_dir: Option<PathBuf>,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let plist = self.plist_path.display();
        if self.present {
            writeln!(f, "wrote {plist}")?;
            writeln!(f)?;
            writeln!(f, "load it (the app then starts at every login):")?;
            writeln!(f, "    launchctl bootstrap gui/$UID {plist}")?;
            writeln!(f, "restart it after a rebuild:")?;
            writeln!(f, "    launchctl kickstart -k gui/$UID {LABEL}")?;
            if let Some(log_dir) = &self.log_dir {
                writeln!(f, "logs:")?;
                writeln!(f, "    {}", log_dir.join(STDOUT_LOG).display())?;
                writeln!(f, "    {}", log_dir.join(STDERR_LOG).display())?;
            }
        } else {
            writeln!(f, "removed {plist}")?;
            writeln!(f)?;
            writeln!(f, "unload it first if it is still running:")?;
            writeln!(f, "    launchctl bootout gui/$UID {plist}")?;
        }
        Ok(())
    }
}

/// File name of the LaunchAgent plist.
#[must_use]
pub fn plist_name() -> String {
    format!("{LABEL}.plist")
}

/// Path of the LaunchAgent plist for the current user.
///
/// # Errors
///
/// Fails when `HOME` is not set.
pub fn plist_path() -> Result<PathBuf> {
    Ok(plist_path_in(&crate::home()?))
}

/// Path of the LaunchAgent plist inside a given home directory.
#[must_use]
pub fn plist_path_in(home: &Path) -> PathBuf {
    home.join("Library").join("LaunchAgents").join(plist_name())
}

/// Directory the agent's logs are written to for the current user.
///
/// # Errors
///
/// Fails when `HOME` is not set.
pub fn log_dir() -> Result<PathBuf> {
    Ok(crate::home()?.join("Library").join("Logs"))
}

/// The LaunchAgent plist for a program at `program`, logging into `log_dir`.
#[must_use]
pub fn plist_contents(program: &Path, log_dir: &Path) -> String {
    let program = escape(&program.display().to_string());
    let stdout = escape(&log_dir.join(STDOUT_LOG).display().to_string());
    let stderr = escape(&log_dir.join(STDERR_LOG).display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{program}</string>
    </array>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>StandardOutPath</key>
    <string>{stdout}</string>
    <key>StandardErrorPath</key>
    <string>{stderr}</string>
</dict>
</plist>
"#
    )
}

/// Write the LaunchAgent plist for this executable.
///
/// # Errors
///
/// Fails when the running executable cannot be located, when the LaunchAgents directory cannot be
/// created, or when the plist cannot be written.
pub fn install() -> Result<Report> {
    let program = std::env::current_exe().context("cannot locate the running executable")?;
    install_in(&crate::home()?, &program)
}

/// Remove the LaunchAgent plist for this user.
///
/// # Errors
///
/// Fails when `HOME` is not set or the plist cannot be removed.
pub fn uninstall() -> Result<Report> {
    uninstall_in(&crate::home()?)
}

/// [`install`] against an explicit home directory, so it can be tested without touching the real one.
fn install_in(home: &Path, program: &Path) -> Result<Report> {
    let plist_path = plist_path_in(home);
    let log_dir = home.join("Library").join("Logs");
    let parent = plist_path
        .parent()
        .context("the LaunchAgent plist path has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("cannot create {}", parent.display()))?;
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("cannot create {}", log_dir.display()))?;
    let contents = plist_contents(program, &log_dir);
    std::fs::write(&plist_path, contents)
        .with_context(|| format!("cannot write {}", plist_path.display()))?;
    Ok(Report {
        plist_path,
        present: true,
        log_dir: Some(log_dir),
    })
}

/// [`uninstall`] against an explicit home directory.
fn uninstall_in(home: &Path) -> Result<Report> {
    let plist_path = plist_path_in(home);
    if plist_path.exists() {
        std::fs::remove_file(&plist_path)
            .with_context(|| format!("cannot remove {}", plist_path.display()))?;
    }
    Ok(Report {
        plist_path,
        present: false,
        log_dir: None,
    })
}

/// Escape the characters that are special inside an XML text node.
fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn plist_path_lives_in_the_launch_agents_directory() {
        assert_eq!(
            plist_path_in(Path::new("/Users/example")),
            PathBuf::from("/Users/example/Library/LaunchAgents/sh.edifier.menubar.plist")
        );
    }

    #[test]
    fn plist_carries_the_label_program_and_log_paths() {
        let program = Path::new("/Applications/Edifier/edifier-menubar");
        let logs = Path::new("/Users/example/Library/Logs");
        let plist = plist_contents(program, logs);

        assert!(plist.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
        assert!(plist.contains("<key>Label</key>\n    <string>sh.edifier.menubar</string>"));
        assert!(plist.contains("<string>/Applications/Edifier/edifier-menubar</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(plist.contains("<key>SuccessfulExit</key>\n        <false/>"));
        assert!(plist.contains("<key>LimitLoadToSessionType</key>\n    <string>Aqua</string>"));
        assert!(plist.contains(
            "<key>StandardErrorPath</key>\n    <string>/Users/example/Library/Logs/edifier-menubar.err.log</string>"
        ));
        assert!(plist.contains(
            "<key>StandardOutPath</key>\n    <string>/Users/example/Library/Logs/edifier-menubar.out.log</string>"
        ));
        assert!(plist.ends_with("</plist>\n"));
    }

    #[test]
    fn plist_escapes_xml_special_characters_in_paths() {
        let plist = plist_contents(
            Path::new("/Users/a&b/<headset>"),
            Path::new("/Users/a&b/Library/Logs"),
        );
        assert!(plist.contains("<string>/Users/a&amp;b/&lt;headset&gt;</string>"));
        assert!(!plist.contains("<headset>"));
    }

    #[test]
    fn install_then_uninstall_round_trips() {
        let dir = TempDir::new("launch-agent");
        let home = dir.path();
        let program = Path::new("/usr/local/bin/edifier-menubar");

        let installed = install_in(home, program).expect("installs");
        assert!(installed.present);
        assert_eq!(installed.plist_path, plist_path_in(home));
        let written = std::fs::read_to_string(&installed.plist_path).expect("readable");
        assert!(written.contains("/usr/local/bin/edifier-menubar"));
        assert!(installed.log_dir.as_deref().is_some_and(|dir| dir.is_dir()));

        let rendered = installed.to_string();
        assert!(rendered.contains("launchctl bootstrap gui/$UID"));
        assert!(rendered.contains("sh.edifier.menubar.plist"));

        let removed = uninstall_in(home).expect("uninstalls");
        assert!(!removed.present);
        assert!(!removed.plist_path.exists());
        assert!(removed.to_string().contains("launchctl bootout gui/$UID"));
    }

    #[test]
    fn uninstall_is_idempotent() {
        let dir = TempDir::new("launch-agent-twice");
        let home = dir.path();
        assert!(!uninstall_in(home).expect("uninstalls").present);
        install_in(home, Path::new("/usr/local/bin/edifier-menubar")).expect("installs");
        assert!(!uninstall_in(home).expect("uninstalls").present);
    }
}
