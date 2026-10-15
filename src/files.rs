//! A file manager tab: SFTP spoken over the system `ssh` (see `ssh::sftp_args`).

use std::cell::RefCell;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use anyhow::{Context, Result, bail};
use gtk::{gdk, gio, glib};
use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{App, dialogs, sidebar, spawn, ssh};

#[derive(Clone, Debug)]
struct Entry {
    name: String,
    dir: bool,
    link: bool,
    size: u64,
}

struct Browser {
    app: Rc<App>,
    alias: String,
    sftp: RefCell<Option<Arc<SftpSession>>>,
    /// Keeps the `ssh` process alive for as long as the tab is open.
    ssh: RefCell<Option<tokio::process::Child>>,
    cwd: RefCell<String>,
    entries: RefCell<Vec<Entry>>,
    list: gtk::ListBox,
    path: gtk::Entry,
    status: gtk::Label,
}

/// A remote name that is safe to show and to use as a local file name.
fn safe_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn parent(dir: &str) -> String {
    match dir.trim_end_matches('/').rsplit_once('/') {
        Some(("", _)) | None => "/".to_owned(),
        Some((parent, _)) => parent.to_owned(),
    }
}

fn human_size(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < units.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", units[unit])
    }
}

/// `dir/name`, or `dir/name (2).ext` etc. when that is taken: never overwrite.
fn free_local_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|path| !path.exists())
        .expect("some free name")
}

async fn connect(args: Vec<String>) -> Result<(SftpSession, tokio::process::Child)> {
    let mut child = tokio::process::Command::new(&args[0])
        .args(&args[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("Could not start ssh")?;
    let stdin = child.stdin.take().context("ssh stdin")?;
    let stdout = child.stdout.take().context("ssh stdout")?;
    let mut stderr = child.stderr.take().context("ssh stderr")?;
    match SftpSession::new(tokio::io::join(stdout, stdin)).await {
        Ok(sftp) => {
            // Keep draining ssh's messages so it never blocks on a full pipe.
            tokio::spawn(async move { tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await });
            Ok((sftp, child))
        }
        Err(error) => {
            let _ = child.start_kill();
            let mut message = String::new();
            let _ = stderr.read_to_string(&mut message).await;
            let message = message.trim();
            if message.is_empty() {
                bail!("{error}")
            } else {
                bail!("{message}")
            }
        }
    }
}

async fn list_dir(sftp: Arc<SftpSession>, path: String) -> Result<(String, Vec<Entry>)> {
    let path = sftp.canonicalize(path).await?;
    let mut entries: Vec<Entry> = sftp
        .read_dir(path.clone())
        .await?
        .filter(|e| safe_name(&e.file_name()))
        .map(|e| {
            let kind = e.file_type();
            Entry {
                name: e.file_name(),
                dir: kind.is_dir(),
                link: kind.is_symlink(),
                size: e.metadata().len(),
            }
        })
        .collect();
    entries.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok((path, entries))
}

fn remove_all(
    sftp: Arc<SftpSession>,
    path: String,
    dir: bool,
) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
    Box::pin(async move {
        if dir {
            // Symlinks are listed as links, never followed, so this stays inside `path`.
            for entry in sftp.read_dir(path.clone()).await? {
                let name = entry.file_name();
                if safe_name(&name) {
                    remove_all(sftp.clone(), join(&path, &name), entry.file_type().is_dir())
                        .await?;
                }
            }
            sftp.remove_dir(path).await?;
        } else {
            sftp.remove_file(path).await?;
        }
        Ok(())
    })
}

async fn download(sftp: Arc<SftpSession>, remote: String, local: PathBuf) -> Result<()> {
    let mut source = sftp.open(remote).await?;
    let mut target = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&local)
        .await?;
    tokio::io::copy(&mut source, &mut target).await?;
    target.flush().await?;
    Ok(())
}

async fn upload(sftp: Arc<SftpSession>, local: PathBuf, remote: String) -> Result<()> {
    if sftp.metadata(remote.clone()).await.is_ok() {
        bail!("{} already exists", remote);
    }
    let mut source = tokio::fs::File::open(&local).await?;
    let mut target = sftp.create(remote).await?;
    tokio::io::copy(&mut source, &mut target).await?;
    target.shutdown().await?;
    Ok(())
}

pub fn open(app: &Rc<App>, alias: &str) {
    let up = gtk::Button::builder()
        .icon_name("go-up-symbolic")
        .tooltip_text("Parent Folder")
        .build();
    let path = gtk::Entry::builder().hexpand(true).build();
    let upload_button = gtk::Button::builder()
        .icon_name("document-send-symbolic")
        .tooltip_text("Upload Files")
        .build();
    let new_folder = gtk::Button::builder()
        .icon_name("folder-new-symbolic")
        .tooltip_text("New Folder")
        .build();
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    bar.set_margin_top(6);
    bar.set_margin_start(6);
    bar.set_margin_end(6);
    bar.append(&up);
    bar.append(&path);
    bar.append(&upload_button);
    bar.append(&new_folder);

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Multiple);
    list.set_activate_on_single_click(false);
    list.add_css_class("navigation-sidebar");
    let scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .build();
    let status = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .margin_start(8)
        .margin_end(8)
        .margin_bottom(6)
        .build();
    status.add_css_class("dim-label");
    status.add_css_class("caption");

    let page_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    page_box.append(&bar);
    page_box.append(&scroller);
    page_box.append(&status);

    let browser = Rc::new(Browser {
        app: app.clone(),
        alias: alias.to_owned(),
        sftp: RefCell::new(None),
        ssh: RefCell::new(None),
        cwd: RefCell::new(".".to_owned()),
        entries: RefCell::new(Vec::new()),
        list: list.clone(),
        path: path.clone(),
        status,
    });

    up.connect_clicked(glib::clone!(
        #[strong]
        browser,
        move |_| {
            let target = parent(&browser.cwd.borrow());
            browser.navigate(target);
        }
    ));
    path.connect_activate(glib::clone!(
        #[strong]
        browser,
        move |entry| browser.navigate(entry.text().to_string())
    ));
    upload_button.connect_clicked(glib::clone!(
        #[strong]
        browser,
        move |_| browser.choose_uploads()
    ));
    new_folder.connect_clicked(glib::clone!(
        #[strong]
        browser,
        move |_| browser.ask_new_folder()
    ));
    list.connect_row_activated(glib::clone!(
        #[strong]
        browser,
        move |_, row| {
            let entry = browser.entries.borrow().get(row.index() as usize).cloned();
            match entry {
                Some(e) if e.dir || e.link => {
                    let target = join(&browser.cwd.borrow(), &e.name);
                    browser.navigate(target);
                }
                Some(e) => browser.choose_download(vec![e]),
                None => {}
            }
        }
    ));

    let menu_click = gtk::GestureClick::builder()
        .button(gdk::BUTTON_SECONDARY)
        .build();
    menu_click.connect_pressed(glib::clone!(
        #[strong]
        browser,
        move |_, _, x, y| browser.context_menu(x, y)
    ));
    list.add_controller(menu_click);

    // Drop files from the desktop to upload them here.
    let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    drop.connect_drop(glib::clone!(
        #[strong]
        browser,
        move |_, value, _, _| {
            let Ok(files) = value.get::<gdk::FileList>() else {
                return false;
            };
            browser.upload_files(files.files().iter().filter_map(|f| f.path()).collect());
            true
        }
    ));
    scroller.add_controller(drop);

    let page = app.tabs.append(&page_box);
    page.set_title(&format!("Files — {alias}"));
    page.set_icon(Some(&gio::ThemedIcon::new("folder-remote-symbolic")));
    app.tabs.set_selected_page(&page);

    // The tab's handlers keep `browser` alive, so end the session explicitly
    // when the tab closes; otherwise its ssh would run until the app quits.
    let handler: Rc<RefCell<Option<glib::SignalHandlerId>>> = Rc::default();
    let closing = Rc::downgrade(&browser);
    let id = app.tabs.connect_close_page(glib::clone!(
        #[strong]
        handler,
        move |tabs, closed| {
            if closed == &page {
                if let Some(browser) = closing.upgrade() {
                    browser.disconnect();
                }
                if let Some(id) = handler.take() {
                    tabs.disconnect(id);
                }
            }
            glib::Propagation::Proceed
        }
    ));
    *handler.borrow_mut() = Some(id);

    browser.navigate(".".to_owned());
}

impl Browser {
    /// End the SFTP session: let ssh exit on its own, kill it if it hangs.
    fn disconnect(&self) {
        self.sftp.borrow_mut().take();
        if let Some(mut child) = self.ssh.borrow_mut().take() {
            crate::runtime().spawn(async move {
                drop(child.stdin.take());
                let exited =
                    tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
                if exited.is_err() {
                    let _ = child.kill().await;
                }
            });
        }
    }

    fn say(&self, message: &str) {
        self.status.set_text(message);
    }

    /// Show `target`, connecting first if needed.
    fn navigate(self: &Rc<Self>, target: String) {
        let Some(sftp) = self.sftp.borrow().clone() else {
            self.say(&format!("Connecting to {}…", self.alias));
            let this = self.clone();
            spawn(connect(ssh::sftp_args(&self.alias)), move |result| {
                match result {
                Ok((sftp, child)) => {
                    *this.sftp.borrow_mut() = Some(Arc::new(sftp));
                    *this.ssh.borrow_mut() = Some(child);
                    this.navigate(target);
                }
                Err(e) => this.say(&format!(
                    "Could not connect: {e}\nIf this host asks for a password, open a terminal to it first, \
                     then press Enter in the path field to retry."
                )),
            }
            });
            return;
        };
        let this = self.clone();
        spawn(list_dir(sftp, target), move |result| match result {
            Ok((path, entries)) => {
                this.path.set_text(&path);
                *this.cwd.borrow_mut() = path;
                this.show(entries);
            }
            Err(e) => {
                this.path.set_text(&this.cwd.borrow());
                this.say(&format!("Could not open that folder: {e}"));
            }
        });
    }

    fn refresh(self: &Rc<Self>) {
        let cwd = self.cwd.borrow().clone();
        self.navigate(cwd);
    }

    fn show(&self, entries: Vec<Entry>) {
        self.list.remove_all();
        for e in &entries {
            let icon = if e.dir {
                "folder-symbolic"
            } else if e.link {
                "insert-link-symbolic"
            } else {
                "text-x-generic-symbolic"
            };
            let name = gtk::Label::builder()
                .label(&e.name)
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .build();
            let size = gtk::Label::new(Some(&if e.dir {
                String::new()
            } else {
                human_size(e.size)
            }));
            size.add_css_class("dim-label");
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            content.append(&gtk::Image::from_icon_name(icon));
            content.append(&name);
            content.append(&size);
            self.list.append(&content);
        }
        self.say(&format!("{} items", entries.len()));
        *self.entries.borrow_mut() = entries;
    }

    /// The entries an action applies to: the selection if it includes the
    /// clicked row, otherwise just the clicked row.
    fn targets(&self, clicked: Option<gtk::ListBoxRow>) -> Vec<Entry> {
        let entries = self.entries.borrow();
        if let Some(row) = clicked
            && !row.is_selected()
        {
            self.list.unselect_all();
            self.list.select_row(Some(&row));
        }
        self.list
            .selected_rows()
            .iter()
            .filter_map(|row| entries.get(row.index() as usize).cloned())
            .collect()
    }

    fn context_menu(self: &Rc<Self>, x: f64, y: f64) {
        let row = self.list.row_at_y(y as i32);
        let item = |f: fn(&Rc<Browser>, Vec<Entry>), targets: Vec<Entry>| -> Box<dyn Fn()> {
            let this = self.clone();
            Box::new(move || f(&this, targets.clone()))
        };
        let items: sidebar::MenuItems = if row.is_some() {
            let targets = self.targets(row);
            let mut items = vec![("Download…", item(Browser::choose_download, targets.clone()))];
            if targets.len() == 1 {
                items.push(("Rename…", item(Browser::ask_rename, targets.clone())));
            }
            items.push(("Delete…", item(Browser::ask_delete, targets)));
            items
        } else {
            vec![
                ("Upload Files…", item(|b, _| b.choose_uploads(), vec![])),
                ("New Folder…", item(|b, _| b.ask_new_folder(), vec![])),
                ("Refresh", item(|b, _| b.refresh(), vec![])),
            ]
        };
        sidebar::popup_menu(self.list.upcast_ref(), x, y, items);
    }

    fn choose_download(self: &Rc<Self>, targets: Vec<Entry>) {
        let files: Vec<Entry> = targets.into_iter().filter(|e| !e.dir).collect();
        if files.is_empty() {
            self.say("Folders can't be downloaded yet; open one and pick its files.");
            return;
        }
        let this = self.clone();
        let dialog = gtk::FileDialog::builder()
            .title("Download To")
            .modal(true)
            .build();
        glib::spawn_future_local(async move {
            let Ok(folder) = dialog.select_folder_future(Some(&this.app.window)).await else {
                return;
            };
            let Some(folder) = folder.path() else { return };
            let Some(sftp) = this.sftp.borrow().clone() else {
                return;
            };
            for e in files {
                let remote = join(&this.cwd.borrow(), &e.name);
                let local = free_local_path(&folder, &e.name);
                this.say(&format!("Downloading {}…", e.name));
                let this2 = this.clone();
                let shown = local.display().to_string();
                spawn(
                    download(sftp.clone(), remote, local),
                    move |result| match result {
                        Ok(()) => this2.say(&format!("Saved {shown}")),
                        Err(err) => this2.say(&format!("Download of {} failed: {err}", e.name)),
                    },
                );
            }
        });
    }

    fn choose_uploads(self: &Rc<Self>) {
        let this = self.clone();
        let dialog = gtk::FileDialog::builder()
            .title("Upload Files")
            .modal(true)
            .build();
        glib::spawn_future_local(async move {
            let Ok(model) = dialog.open_multiple_future(Some(&this.app.window)).await else {
                return;
            };
            let paths = (0..model.n_items())
                .filter_map(|i| model.item(i).and_downcast::<gio::File>())
                .filter_map(|f| f.path())
                .collect();
            this.upload_files(paths);
        });
    }

    fn upload_files(self: &Rc<Self>, paths: Vec<PathBuf>) {
        let Some(sftp) = self.sftp.borrow().clone() else {
            self.say("Not connected.");
            return;
        };
        for local in paths {
            let Some(name) = local
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|n| safe_name(n))
                .map(str::to_owned)
            else {
                continue;
            };
            if local.is_dir() {
                self.say(&format!(
                    "{name} is a folder; only files can be uploaded for now."
                ));
                continue;
            }
            let remote = join(&self.cwd.borrow(), &name);
            self.say(&format!("Uploading {name}…"));
            let this = self.clone();
            spawn(
                upload(sftp.clone(), local, remote),
                move |result| match result {
                    Ok(()) => {
                        this.refresh();
                        this.say(&format!("Uploaded {name}"));
                    }
                    Err(e) => this.say(&format!("Upload of {name} failed: {e}")),
                },
            );
        }
    }

    fn ask_new_folder(self: &Rc<Self>) {
        let this = self.clone();
        dialogs::ask_text(&self.app, "New Folder", "Name", "", "Create", move |name| {
            if !safe_name(&name) {
                this.say("That name can't be used.");
                return;
            }
            let Some(sftp) = this.sftp.borrow().clone() else {
                return;
            };
            let path = join(&this.cwd.borrow(), &name);
            let this = this.clone();
            spawn(
                async move { Ok(sftp.create_dir(path).await?) },
                move |result| match result {
                    Ok(()) => this.refresh(),
                    Err(e) => this.say(&format!("Could not create {name}: {e}")),
                },
            );
        });
    }

    fn ask_rename(self: &Rc<Self>, targets: Vec<Entry>) {
        let Some(entry) = targets.into_iter().next() else {
            return;
        };
        let this = self.clone();
        dialogs::ask_text(
            &self.app,
            "Rename",
            "New name",
            &entry.name.clone(),
            "Rename",
            move |name| {
                if !safe_name(&name) {
                    this.say("That name can't be used.");
                    return;
                }
                let Some(sftp) = this.sftp.borrow().clone() else {
                    return;
                };
                let cwd = this.cwd.borrow().clone();
                let (from, to) = (join(&cwd, &entry.name), join(&cwd, &name));
                let this = this.clone();
                spawn(
                    async move { Ok(sftp.rename(from, to).await?) },
                    move |result| match result {
                        Ok(()) => this.refresh(),
                        Err(e) => this.say(&format!("Rename failed: {e}")),
                    },
                );
            },
        );
    }

    fn ask_delete(self: &Rc<Self>, targets: Vec<Entry>) {
        if targets.is_empty() {
            return;
        }
        let heading = match targets.as_slice() {
            [one] => format!("Delete “{}”?", one.name),
            many => format!("Delete {} items?", many.len()),
        };
        let body = if targets.iter().any(|e| e.dir) {
            "Folders are deleted with everything in them. This can't be undone."
        } else {
            "This can't be undone."
        };
        let this = self.clone();
        dialogs::confirm(&self.app, &heading, body, "Delete", move || {
            let Some(sftp) = this.sftp.borrow().clone() else {
                return;
            };
            let cwd = this.cwd.borrow().clone();
            let work = targets
                .iter()
                .map(|e| (join(&cwd, &e.name), e.dir && !e.link))
                .collect::<Vec<_>>();
            this.say("Deleting…");
            let this = this.clone();
            spawn(
                async move {
                    for (path, dir) in work {
                        remove_all(sftp.clone(), path, dir).await?;
                    }
                    Ok(())
                },
                move |result| {
                    this.refresh();
                    if let Err(e) = result {
                        this.say(&format!("Delete failed: {e}"));
                    }
                },
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_paths() {
        assert_eq!(join("/", "etc"), "/etc");
        assert_eq!(join("/home/me", "a.txt"), "/home/me/a.txt");
        assert_eq!(parent("/home/me"), "/home");
        assert_eq!(parent("/home"), "/");
        assert_eq!(parent("/"), "/");
    }

    #[test]
    fn server_names_cannot_escape_the_download_folder() {
        for bad in ["", ".", "..", "../x", "a/b", "nul\0"] {
            assert!(!safe_name(bad), "{bad:?}");
        }
        assert!(safe_name(".bashrc"));
    }

    #[test]
    fn downloads_never_overwrite() {
        let dir = std::env::temp_dir().join(format!("sshpilot-rs-dl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        assert_eq!(free_local_path(&dir, "a.txt"), dir.join("a (2).txt"));
        assert_eq!(free_local_path(&dir, "b.txt"), dir.join("b.txt"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(10), "10 B");
        assert_eq!(human_size(1536), "1.5 KB");
    }
}

/// Against a real server. Run with:
/// `SSHPILOT_RS_TEST_HOST=alias SSHPILOT_RS_TEST_SSH_CONFIG=/path/to/config cargo test -- --ignored`
#[cfg(test)]
mod live_tests {
    use super::*;

    #[test]
    #[ignore = "needs an SSH server"]
    fn sftp_round_trip() {
        let host = std::env::var("SSHPILOT_RS_TEST_HOST").expect("SSHPILOT_RS_TEST_HOST");
        let mut args = ssh::sftp_args(&host);
        if let Ok(config) = std::env::var("SSHPILOT_RS_TEST_SSH_CONFIG") {
            args.splice(1..1, ["-F".to_owned(), config]);
        }
        let local = std::env::temp_dir().join(format!("sshpilot-rs-live-{}", std::process::id()));
        std::fs::create_dir_all(&local).unwrap();
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(local.join("up.bin"), &payload).unwrap();

        crate::runtime().block_on(async {
            let (sftp, _child) = connect(args).await.expect("connect");
            let sftp = Arc::new(sftp);
            let (home, _) = list_dir(sftp.clone(), ".".into()).await.unwrap();
            let dir = join(&home, "sshpilot-rs-test");
            sftp.create_dir(dir.clone()).await.unwrap();
            sftp.create_dir(join(&dir, "nested")).await.unwrap();

            upload(sftp.clone(), local.join("up.bin"), join(&dir, "a.bin"))
                .await
                .unwrap();
            assert!(
                upload(sftp.clone(), local.join("up.bin"), join(&dir, "a.bin"))
                    .await
                    .is_err(),
                "no overwrite"
            );
            sftp.rename(join(&dir, "a.bin"), join(&dir, "b.bin"))
                .await
                .unwrap();

            let (_, entries) = list_dir(sftp.clone(), dir.clone()).await.unwrap();
            let names: Vec<_> = entries.iter().map(|e| (e.name.as_str(), e.dir)).collect();
            assert_eq!(names, [("nested", true), ("b.bin", false)]);
            assert_eq!(entries[1].size, payload.len() as u64);

            download(sftp.clone(), join(&dir, "b.bin"), local.join("down.bin"))
                .await
                .unwrap();
            assert_eq!(std::fs::read(local.join("down.bin")).unwrap(), payload);

            remove_all(sftp.clone(), dir.clone(), true).await.unwrap();
            assert!(sftp.metadata(dir).await.is_err(), "removed");
        });
        std::fs::remove_dir_all(local).unwrap();
    }
}
