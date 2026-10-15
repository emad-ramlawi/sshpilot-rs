//! `sshpilot-rs --install`: add the app's menu entry and icon for this user.
//!
//! Desktops find a window's icon through the `.desktop` file named after the
//! app ID (on Wayland there is no other way), so a bare binary shows a
//! generic icon until this is done. Both files are built into the binary.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::{APP_ID, store};

const DESKTOP_ENTRY: &str = include_str!("../data/io.github.emad_ramlawi.SshPilotRs.desktop");
const ICON: &str = include_str!("../data/icons/io.github.emad_ramlawi.SshPilotRs.svg");

/// `$XDG_DATA_HOME`, normally ~/.local/share.
fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| store::home_dir().join(".local").join("share"))
}

fn paths(data: &Path) -> (PathBuf, PathBuf) {
    (
        data.join("applications").join(format!("{APP_ID}.desktop")),
        data.join("icons/hicolor/scalable/apps")
            .join(format!("{APP_ID}.svg")),
    )
}

/// One `Exec=` argument for `program`, quoted as the desktop entry spec asks.
fn exec_arg(program: &str) -> Result<String> {
    if program.chars().any(char::is_control) {
        bail!("The program's path contains control characters");
    }
    if program
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._+-".contains(c))
    {
        return Ok(program.to_owned());
    }
    let mut arg = String::from("\"");
    for c in program.chars() {
        match c {
            // Quoting escape, then the string escape on top: `\\` + char.
            '"' | '`' | '$' | '\\' => {
                arg.push_str("\\\\");
                arg.push(c);
            }
            '%' => arg.push_str("%%"),
            _ => arg.push(c),
        }
    }
    arg.push('"');
    Ok(arg)
}

/// The menu entry, starting this very binary.
fn entry_for(program: &str) -> Result<String> {
    let exec = format!("Exec={}", exec_arg(program)?);
    Ok(DESKTOP_ENTRY
        .lines()
        .map(|line| {
            if line.starts_with("Exec=") {
                &exec
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n")
}

pub fn install() -> Result<()> {
    let program = std::env::current_exe().context("Cannot find this program's path")?;
    let program = program
        .to_str()
        .context("This program's path is not valid UTF-8")?;
    let (entry, icon) = paths(&data_dir());
    for (path, text) in [(&entry, entry_for(program)?), (&icon, ICON.to_owned())] {
        fs::create_dir_all(path.parent().expect("has a folder"))?;
        fs::write(path, text).with_context(|| format!("Cannot write {}", path.display()))?;
        println!("Wrote {}", path.display());
    }
    println!("The app menu now starts {program}. Run --install again if you move it.");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let (entry, icon) = paths(&data_dir());
    for path in [entry, icon] {
        match fs::remove_file(&path) {
            Ok(()) => println!("Removed {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("Cannot remove {}", path.display())),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_line_quotes_odd_paths() {
        assert_eq!(
            exec_arg("/usr/local/bin/sshpilot-rs").unwrap(),
            "/usr/local/bin/sshpilot-rs"
        );
        assert_eq!(exec_arg("/home/a b/x").unwrap(), "\"/home/a b/x\"");
        assert_eq!(exec_arg("/a$b%c").unwrap(), "\"/a\\\\$b%%c\"");
        assert!(exec_arg("/a\nb").is_err());
    }

    #[test]
    fn entry_points_at_this_binary() {
        let entry = entry_for("/opt/x/sshpilot-rs").unwrap();
        assert!(entry.contains("\nExec=/opt/x/sshpilot-rs\n"));
        assert!(entry.contains(&format!("Icon={APP_ID}\n")));
        assert!(entry.contains(&format!("StartupWMClass={APP_ID}\n")));
        assert_eq!(entry.matches("Exec=").count(), 1);
    }
}
