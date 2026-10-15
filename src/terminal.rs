//! Terminal tabs: the system `ssh` to a host, or a local shell.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib, pango};
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
    open_local_in(app, None);
}

/// A terminal running the user's shell in `dir`, else the home folder.
pub fn open_local_in(app: &Rc<App>, dir: Option<PathBuf>) {
    let home = store::home_dir();
    let dir = dir.filter(|dir| dir.to_str().is_some()).unwrap_or(home);
    let page = add_tab(
        app,
        &[user_shell()],
        dir.to_str(),
        "",
        "Local",
        "computer-symbolic",
    );
    // Show the title the shell sets (fish does by default; bash and zsh
    // through their prompt settings), like other terminals do.
    if let Some(terminal) = terminal_in(&page) {
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

/// The folder the local shell in `page` is in now, read from the system
/// (works in any shell, no setup), if that folder still exists.
pub fn working_dir(app: &App, page: &adw::TabPage) -> Option<PathBuf> {
    let terminal = terminal_in(page)?;
    let pid = app
        .local_shells
        .borrow()
        .iter()
        .find(|(shell, _)| shell.upgrade().as_ref() == Some(&terminal))
        .map(|(_, pid)| *pid)?;
    // A folder deleted since reads as "/path (deleted)", which is not a folder.
    let dir = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    dir.is_dir().then_some(dir)
}

/// Whether an ssh tab whose `ssh` ended with wait `status` closes by itself:
/// yes when the remote session ended (`exit`, Ctrl+D, with whatever exit code
/// the remote shell passes back), no when ssh itself failed (255: could not
/// connect, login refused, connection lost) or was killed, so the error
/// stays readable.
fn ssh_session_finished(status: i32) -> bool {
    let exited = status & 0x7f == 0;
    exited && (status >> 8) & 0xff != 255
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
    apply_look(app, &terminal);
    add_shortcuts(&terminal);

    let program = argv[0].clone();
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let local = host.is_empty();
    let weak_app = Rc::downgrade(app);
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
            move |result| match result {
                // Remember a local shell, so Duplicate can start in its folder.
                Ok(pid) if local => {
                    if let Some(app) = weak_app.upgrade() {
                        let mut shells = app.local_shells.borrow_mut();
                        shells.retain(|(shell, _)| shell.upgrade().is_some());
                        shells.push((terminal.downgrade(), pid.0));
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    terminal.feed(format!("Could not start {program}: {e}\r\n").as_bytes());
                }
            }
        ),
    );

    // A rounded black card with a scrollbar, as in sshPilot.
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&terminal)
        .build();
    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
    card.add_css_class("terminal-card");
    card.set_overflow(gtk::Overflow::Hidden);
    card.set_margin_start(6);
    card.set_margin_end(6);
    card.set_margin_bottom(6);
    card.append(&scroller);
    let page = app.tabs.append(&card);
    page.set_title(title);
    // Which host the tab is for (the tab menu duplicates it).
    page.set_keyword(host);
    page.set_icon(Some(&gio::ThemedIcon::new(icon)));
    app.tabs.set_selected_page(&page);
    terminal.grab_focus();

    // When the session ends, close the tab, unless ssh failed: then its
    // error stays on screen.
    let weak_app = Rc::downgrade(app);
    terminal.connect_child_exited(glib::clone!(
        #[weak]
        page,
        move |terminal, status| {
            let Some(app) = weak_app.upgrade() else {
                return;
            };
            app.local_shells
                .borrow_mut()
                .retain(|(shell, _)| shell.upgrade().is_some_and(|shell| &shell != terminal));
            if !local && !ssh_session_finished(status) {
                terminal.feed(
                    b"\r\n[ssh stopped with the error above. Close this tab, or duplicate it to try again.]\r\n",
                );
                return;
            }
            // After the signal, not inside it: closing destroys this terminal.
            glib::idle_add_local_once(move || {
                let open = (0..app.tabs.n_pages()).any(|i| app.tabs.nth_page(i) == page);
                if open {
                    app.tabs.close_page(&page);
                }
            });
        }
    ));
    page
}

/// The terminal shown in `page`, if it is a terminal tab.
pub fn terminal_in(page: &adw::TabPage) -> Option<vte4::Terminal> {
    page.child()
        .first_child()?
        .downcast::<gtk::ScrolledWindow>()
        .ok()?
        .child()?
        .downcast()
        .ok()
}

/// White on black with the Tango palette, as in GNOME Terminal (and the
/// default theme of sshPilot, which this app grew out of).
const FOREGROUND: &str = "#FFFFFF";
const BACKGROUND: &str = "#000000";
const PALETTE: [&str; 16] = [
    "#000000", "#CC0000", "#4E9A06", "#C4A000", "#3465A4", "#75507B", "#06989A", "#D3D7CF",
    "#555753", "#EF2929", "#8AE234", "#FCE94F", "#729FCF", "#AD7FA8", "#34E2E2", "#EEEEEC",
];

fn rgba(hex: &str) -> gdk::RGBA {
    gdk::RGBA::parse(hex).expect("valid color")
}

/// Colors and font. Selection colors stay unset, so VTE shows a selection
/// in reverse video and colored output stays readable while selected.
fn apply_look(app: &App, terminal: &vte4::Terminal) {
    let palette: Vec<gdk::RGBA> = PALETTE.iter().map(|hex| rgba(hex)).collect();
    let palette: Vec<&gdk::RGBA> = palette.iter().collect();
    let foreground = rgba(FOREGROUND);
    terminal.set_colors(Some(&foreground), Some(&rgba(BACKGROUND)), &palette);
    terminal.set_color_cursor(Some(&foreground));
    terminal.set_font(Some(&pango::FontDescription::from_string(&font(app))));
}

/// The chosen terminal font; until one is chosen, the one set in sshPilot
/// (read-only, for people coming from it), else "Monospace 12".
pub fn font(app: &App) -> String {
    if let Some(font) = &app.layout.borrow().terminal_font {
        return font.clone();
    }
    static DEFAULT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DEFAULT
        .get_or_init(|| sshpilot_font().unwrap_or_else(|| "Monospace 12".to_owned()))
        .clone()
}

fn sshpilot_font() -> Option<String> {
    let path = store::home_dir().join(".config/sshpilot/config.json");
    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let font = config.get("terminal")?.get("font")?.as_str()?.trim();
    (!font.is_empty() && !font.chars().any(char::is_control)).then(|| font.to_owned())
}

/// The terminal font picker: a monospace family and a size, applied to every
/// tab as they are picked.
pub fn font_popover(app: &Rc<App>) -> gtk::Popover {
    let current = pango::FontDescription::from_string(&font(app));
    let current_family = current
        .family()
        .map(|f| f.to_string())
        .unwrap_or_else(|| "Monospace".to_owned());
    let current_size = match current.size() {
        0 => 12.0,
        size => f64::from(size) / f64::from(pango::SCALE),
    };
    let mut families: Vec<String> = app
        .window
        .pango_context()
        .list_families()
        .iter()
        .filter(|family| family.is_monospace())
        .map(|family| family.name().to_string())
        .collect();
    if !families.contains(&current_family) {
        families.push(current_family.clone());
    }
    families.sort_by_key(|family| family.to_lowercase());
    families.dedup();

    let names: Vec<&str> = families.iter().map(String::as_str).collect();
    let family = gtk::DropDown::from_strings(&names);
    // Type to search the list.
    family.set_enable_search(true);
    family.set_expression(Some(gtk::PropertyExpression::new(
        gtk::StringObject::static_type(),
        None::<gtk::Expression>,
        "string",
    )));
    if let Some(index) = families.iter().position(|f| *f == current_family) {
        family.set_selected(index as u32);
    }
    let size = gtk::SpinButton::with_range(6.0, 48.0, 1.0);
    size.set_value(current_size.round());

    let size_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    size_row.append(
        &gtk::Label::builder()
            .label("Size")
            .xalign(0.0)
            .hexpand(true)
            .build(),
    );
    size_row.append(&size);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_top(6);
    content.set_margin_bottom(6);
    content.set_margin_start(6);
    content.set_margin_end(6);
    content.append(
        &gtk::Label::builder()
            .label("Terminal Font")
            .xalign(0.0)
            .css_classes(["heading"])
            .build(),
    );
    content.append(&family);
    content.append(&size_row);

    let apply = Rc::new(glib::clone!(
        #[weak]
        app,
        #[weak]
        family,
        #[weak]
        size,
        move || {
            let Some(name) = family
                .selected_item()
                .and_downcast::<gtk::StringObject>()
                .map(|s| s.string())
            else {
                return;
            };
            set_font(&app, &format!("{name} {}", size.value_as_int()));
        }
    ));
    family.connect_selected_notify(glib::clone!(
        #[strong]
        apply,
        move |_| apply()
    ));
    size.connect_value_changed(move |_| apply());

    gtk::Popover::builder().child(&content).build()
}

/// Use `font` ("Hack 12") in every terminal, now and next time.
pub fn set_font(app: &App, font: &str) {
    app.layout.borrow_mut().terminal_font = Some(font.to_owned());
    if let Err(e) = app.layout.borrow().save(&app.path) {
        app.toast(&e.to_string());
    }
    let description = pango::FontDescription::from_string(font);
    for i in 0..app.tabs.n_pages() {
        if let Some(terminal) = terminal_in(&app.tabs.nth_page(i)) {
            terminal.set_font(Some(&description));
        }
    }
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
        if let Some(terminal) = terminal_in(&app.tabs.nth_page(i)) {
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
    fn ssh_tabs_close_unless_ssh_failed() {
        // Wait statuses: exit code in the second byte, signal in the low bits.
        assert!(ssh_session_finished(0), "exit");
        assert!(ssh_session_finished(1 << 8), "exit after a failed command");
        assert!(ssh_session_finished(130 << 8), "remote shell's own code");
        assert!(!ssh_session_finished(255 << 8), "ssh error");
        assert!(!ssh_session_finished(9), "killed");
    }

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
