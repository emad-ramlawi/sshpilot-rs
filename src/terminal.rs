//! A terminal tab running the system `ssh`.

use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use vte4::prelude::*;

use crate::{App, ssh};

pub fn open(app: &Rc<App>, alias: &str) {
    let terminal = vte4::Terminal::new();
    terminal.set_vexpand(true);
    terminal.set_hexpand(true);
    terminal.set_scrollback_lines(10_000);

    let args = ssh::terminal_args(alias);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    terminal.spawn_async(
        vte4::PtyFlags::DEFAULT,
        None,
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
                    terminal.feed(format!("Could not start ssh: {e}\r\n").as_bytes());
                }
            }
        ),
    );
    terminal.connect_child_exited(|terminal, _status| {
        terminal.feed(b"\r\n[Connection closed]\r\n");
    });

    let page = app.tabs.append(&terminal);
    page.set_title(alias);
    page.set_icon(Some(&gio::ThemedIcon::new("utilities-terminal-symbolic")));
    app.tabs.set_selected_page(&page);
    terminal.grab_focus();
}
