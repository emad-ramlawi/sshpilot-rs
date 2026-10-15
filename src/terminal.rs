//! Terminal tabs: the system `ssh` to a host, or a local shell.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use vte4::prelude::*;

use crate::{App, ssh, store};

/// A terminal running `ssh` to `alias`.
pub fn open(app: &Rc<App>, alias: &str) {
    app.connected_to(alias);
    let args = ssh::terminal_args(alias);
    let _ = add_tab(
        app,
        &args,
        None,
        alias,
        alias,
        "utilities-terminal-symbolic",
    );
}

/// A terminal running the user's shell in their home folder.
pub fn open_local(app: &Rc<App>) {
    let home = store::home_dir();
    let page = add_tab(
        app,
        &[user_shell()],
        home.to_str(),
        "",
        "Local",
        "computer-symbolic",
    );
    // Show the title the shell sets (fish does by default; bash and zsh
    // through their prompt settings), like other terminals do.
    if let Ok(terminal) = page.child().downcast::<vte4::Terminal>() {
        terminal.connect_window_title_changed(glib::clone!(
            #[weak]
            page,
            move |terminal| {
                let title = terminal.window_title().filter(|t| !t.trim().is_empty());
                page.set_title(title.as_deref().unwrap_or("Local"));
            }
        ));
    }
}

/// The user's shell (bash, zsh, fish, …): the login shell from the user
/// database, which follows `chsh` right away, else `$SHELL` (which only
/// changes at the next login), else bash, else sh. A shell that is no longer
/// installed is skipped.
fn user_shell() -> String {
    pick_shell(
        [
            login_shell(),
            std::env::var("SHELL").ok(),
            Some("/bin/bash".to_owned()),
        ],
        is_executable,
    )
    .unwrap_or_else(|| "/bin/sh".to_owned())
}

/// The shell field of this user's `passwd` entry (also for LDAP or
/// systemd-homed users, through `getent`).
fn login_shell() -> Option<String> {
    let uid = std::fs::metadata("/proc/self").ok()?.uid();
    let output = std::process::Command::new("getent")
        .args(["passwd", &uid.to_string()])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    // name:password:uid:gid:gecos:home:shell
    let line = String::from_utf8(output.stdout).ok()?;
    line.trim_end().rsplit(':').next().map(str::to_owned)
}

fn pick_shell(
    candidates: impl IntoIterator<Item = Option<String>>,
    usable: impl Fn(&Path) -> bool,
) -> Option<String> {
    candidates.into_iter().flatten().find(|shell| {
        let path = Path::new(shell);
        // `nologin` and `false` are for accounts that must not get a shell.
        let blocked = matches!(
            path.file_name().and_then(|n| n.to_str()),
            Some("nologin" | "false")
        );
        path.is_absolute() && !blocked && usable(path)
    })
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `host` is the alias for ssh tabs, empty for a local shell.
fn add_tab(
    app: &Rc<App>,
    argv: &[String],
    dir: Option<&str>,
    host: &str,
    title: &str,
    icon: &str,
) -> adw::TabPage {
    let terminal = vte4::Terminal::new();
    terminal.set_vexpand(true);
    terminal.set_hexpand(true);
    terminal.set_scrollback_lines(10_000);
    terminal.set_font_scale(font_scale(app));
    add_shortcuts(&terminal);

    let program = argv[0].clone();
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    terminal.spawn_async(
        vte4::PtyFlags::DEFAULT,
        dir,
        &argv,
        &[],
        glib::SpawnFlags::SEARCH_PATH,
        || {},
        -1,
        None::<&gio::Cancellable>,
        glib::clone!(
            #[weak]
            terminal,
            move |result| {
                if let Err(e) = result {
                    terminal.feed(format!("Could not start {program}: {e}\r\n").as_bytes());
                }
            }
        ),
    );
    terminal.connect_child_exited(|terminal, _status| {
        terminal.feed(b"\r\n[Session ended]\r\n");
    });

    let page = app.tabs.append(&terminal);
    page.set_title(title);
    // Which host the tab is for (the tab menu duplicates it).
    page.set_keyword(host);
    page.set_icon(Some(&gio::ThemedIcon::new(icon)));
    app.tabs.set_selected_page(&page);
    terminal.grab_focus();
    page
}

/// Text sizes the zoom buttons step through.
const SCALES: [f64; 13] = [
    0.5, 0.67, 0.8, 0.9, 1.0, 1.1, 1.2, 1.33, 1.5, 1.7, 2.0, 2.4, 3.0,
];

pub fn font_scale(app: &App) -> f64 {
    app.layout.borrow().font_scale.unwrap_or(1.0)
}

/// Make terminal text one step larger (`Some(true)`), smaller
/// (`Some(false)`) or the default size (`None`), in every tab.
pub fn zoom(app: &Rc<App>, larger: Option<bool>) {
    let now = font_scale(app);
    let scale = match larger {
        Some(true) => SCALES.iter().copied().find(|&s| s > now + 0.001),
        Some(false) => SCALES.iter().copied().rev().find(|&s| s < now - 0.001),
        None => Some(1.0),
    }
    .unwrap_or(now);
    app.layout.borrow_mut().font_scale = (scale != 1.0).then_some(scale);
    if let Err(e) = app.layout.borrow().save(&app.path) {
        app.toast(&e.to_string());
    }
    for i in 0..app.tabs.n_pages() {
        if let Ok(terminal) = app.tabs.nth_page(i).child().downcast::<vte4::Terminal>() {
            terminal.set_font_scale(scale);
        }
    }
    show_zoom(app);
}

/// Show the text size on the reset button.
pub fn show_zoom(app: &App) {
    app.zoom_label
        .set_label(&format!("{:.0}%", font_scale(app) * 100.0));
}

/// Copy and paste with Ctrl+Shift+C / Ctrl+Shift+V (VTE has no built-in keys;
/// plain Ctrl+C must still reach the remote program).
fn add_shortcuts(terminal: &vte4::Terminal) {
    let shortcuts = gtk::ShortcutController::new();
    let add = |trigger: &str, run: fn(&vte4::Terminal)| {
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string(trigger),
            Some(gtk::CallbackAction::new(move |widget, _| {
                if let Some(terminal) = widget.downcast_ref::<vte4::Terminal>() {
                    run(terminal);
                }
                glib::Propagation::Stop
            })),
        ));
    };
    add("<Control><Shift>c", |t| {
        if t.has_selection() {
            t.copy_clipboard_format(vte4::Format::Text);
        }
    });
    add("<Control><Shift>v", |t| t.paste_clipboard());
    terminal.add_controller(shortcuts);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_first_usable_shell() {
        let installed = |p: &Path| p != Path::new("/usr/bin/fish");
        // A shell that was uninstalled after `chsh` falls through to $SHELL.
        let shell = pick_shell(
            [Some("/usr/bin/fish".into()), Some("/usr/bin/zsh".into())],
            installed,
        );
        assert_eq!(shell.as_deref(), Some("/usr/bin/zsh"));
        for blocked in ["/usr/sbin/nologin", "/bin/false", "zsh"] {
            assert_eq!(
                pick_shell([Some(blocked.into())], |_| true),
                None,
                "{blocked}"
            );
        }
        let fallback = pick_shell([None, Some("/bin/bash".into())], |_| true);
        assert_eq!(fallback.as_deref(), Some("/bin/bash"));
    }

    #[test]
    fn finds_a_real_shell_here() {
        assert!(is_executable(Path::new("/bin/sh")));
        assert!(!is_executable(Path::new("/etc/passwd")));
        assert!(is_executable(Path::new(&user_shell())));
    }
}
