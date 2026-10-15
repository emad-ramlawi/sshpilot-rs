//! Small forms: new host, group name, confirmations.

use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::App;
use crate::store::{self, Id, NewHost};

fn entry(title: &str, text: &str) -> adw::EntryRow {
    let row = adw::EntryRow::builder().title(title).text(text).build();
    row.set_activates_default(true);
    row
}

/// Ask for one line of text; `run` gets it when the user confirms.
pub fn ask_text(
    app: &Rc<App>,
    heading: &str,
    label: &str,
    initial: &str,
    confirm: &str,
    run: impl Fn(String) + 'static,
) {
    let dialog = adw::AlertDialog::new(Some(heading), None);
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("ok", confirm);
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    let row = entry(label, initial);
    let group = adw::PreferencesGroup::new();
    group.add(&row);
    dialog.set_extra_child(Some(&group));
    let valid = |text: &str| !text.trim().is_empty();
    dialog.set_response_enabled("ok", valid(initial));
    row.connect_changed(glib::clone!(
        #[weak]
        dialog,
        move |row| dialog.set_response_enabled("ok", valid(&row.text()))
    ));
    dialog.connect_response(Some("ok"), move |_, _| run(row.text().trim().to_owned()));
    dialog.present(Some(&app.window));
}

/// Ask before something destructive.
pub fn confirm(app: &Rc<App>, heading: &str, body: &str, confirm: &str, run: impl Fn() + 'static) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("ok", confirm);
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
    dialog.set_close_response("cancel");
    dialog.connect_response(Some("ok"), move |_, _| run());
    dialog.present(Some(&app.window));
}

/// Add a `Host` block to ~/.ssh/config. The file watcher then shows it.
pub fn new_host(app: &Rc<App>) {
    let dialog = adw::AlertDialog::new(
        Some("New Connection"),
        Some("This adds a Host entry to ~/.ssh/config. For anything more, edit that file."),
    );
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("add", "Add");
    dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("add"));
    dialog.set_close_response("cancel");

    let alias = entry("Name (Host alias)", "");
    let hostname = entry("Address (HostName)", "");
    let user = entry("User (optional)", "");
    let port = entry("Port (optional)", "");
    let form = adw::PreferencesGroup::new();
    for row in [&alias, &hostname, &user, &port] {
        form.add(row);
    }
    dialog.set_extra_child(Some(&form));

    let fields = [alias.clone(), hostname.clone(), user.clone(), port.clone()];
    let read = Rc::new(move || NewHost {
        alias: alias.text().to_string(),
        hostname: hostname.text().to_string(),
        user: user.text().to_string(),
        port: port.text().to_string(),
    });
    let refresh = glib::clone!(
        #[weak]
        dialog,
        #[strong]
        read,
        move || dialog.set_response_enabled("add", read().validate().is_ok())
    );
    refresh();
    for row in fields {
        let refresh = refresh.clone();
        row.connect_changed(move |_| refresh());
    }

    let app2 = app.clone();
    dialog.connect_response(Some("add"), move |_, _| {
        if let Err(e) = store::append_host(&app2.config_path, &read()) {
            app2.toast(&e.to_string());
        }
        // The file watcher would catch it too; reloading now feels immediate.
        app2.reload_config();
    });
    dialog.present(Some(&app.window));
}

/// Open ~/.ssh/config in the default text editor.
pub fn edit_ssh_config(app: &Rc<App>) {
    let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&app.config_path)));
    let app = app.clone();
    launcher.launch(
        Some(&app.window.clone()),
        None::<&gio::Cancellable>,
        move |result| {
            if let Err(e) = result {
                app.toast(&format!("Could not open ~/.ssh/config: {e}"));
            }
        },
    );
}

pub fn new_group(app: &Rc<App>) {
    let app2 = app.clone();
    ask_text(app, "New Group", "Name", "", "Add", move |name| {
        app2.commit(|layout| layout.add_group(&name).map(|_| ()));
    });
}

pub fn rename_group(app: &Rc<App>, id: Id) {
    let Some(current) = app.layout.borrow().group(id).map(|g| g.name.clone()) else {
        return;
    };
    let app2 = app.clone();
    ask_text(
        app,
        "Rename Group",
        "Name",
        &current,
        "Rename",
        move |name| {
            app2.commit(|layout| layout.rename_group(id, &name));
        },
    );
}

pub fn delete_group(app: &Rc<App>, id: Id) {
    let Some(name) = app.layout.borrow().group(id).map(|g| g.name.clone()) else {
        return;
    };
    let app2 = app.clone();
    confirm(
        app,
        &format!("Delete “{name}”?"),
        "Its connections are kept and move to Ungrouped.",
        "Delete",
        move || {
            app2.commit(|layout| {
                layout.remove_group(id);
                Ok(())
            })
        },
    );
}
