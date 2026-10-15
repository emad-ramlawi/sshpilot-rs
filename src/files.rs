//! A file manager tab: your files on the left, the server's on the right,
//! with Upload under yours and Download under the server's. SFTP is spoken
//! over the system `ssh` (see `ssh::sftp_args`).

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::Poll;
use std::time::{Duration, Instant};

use adw::prelude::*;
use anyhow::{Context, Result, bail};
use gtk::{gdk, gio, glib};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader, ReadBuf};

use crate::{App, dialogs, sidebar, spawn, ssh, store};

#[derive(Clone, Debug)]
struct Entry {
    name: String,
    /// Full path, fixed when listed (the folder shown can change while a
    /// menu or dialog is open).
    path: String,
    dir: bool,
    link: bool,
    size: u64,
    /// Last change, in seconds since 1970.
    modified: Option<i64>,
}

/// One side of the file manager, laid out as in sshPilot: a toolbar card, the
/// listing card (Name, Size, Modified) and an action bar card at the bottom.
struct Pane {
    root: gtk::Box,
    bar: gtk::Box,
    up: gtk::Button,
    refresh: gtk::Button,
    hidden: gtk::ToggleButton,
    path: gtk::Entry,
    list: gtk::ListBox,
    card: gtk::Box,
    actions: gtk::Box,
    /// The folder shown; everything in it, and the entries shown (no dot
    /// files unless asked) in row order.
    cwd: RefCell<String>,
    all: RefCell<Vec<Entry>>,
    entries: RefCell<Vec<Entry>>,
}

type Waiting = Box<dyn FnOnce(&Rc<Browser>, Arc<SftpSession>)>;

struct Browser {
    app: Rc<App>,
    alias: String,
    sftp: RefCell<Option<Arc<SftpSession>>>,
    /// Keeps the `ssh` process alive for as long as the tab is open.
    ssh: RefCell<Option<tokio::process::Child>>,
    connecting: Cell<bool>,
    /// Actions waiting for the connection being made.
    waiting: RefCell<Vec<Waiting>>,
    /// The tab is closed: don't start or keep a session.
    closed: Cell<bool>,
    /// Local: this computer.
    local: Pane,
    /// The server.
    remote: Pane,
    status: gtk::Label,
    progress: Progress,
}

/// Running transfers, shown as one progress bar.
struct Progress {
    bar: gtk::ProgressBar,
    /// Bytes moved so far and expected size, per transfer.
    items: RefCell<Vec<(Arc<AtomicU64>, u64)>>,
    uploads: Cell<usize>,
    downloads: Cell<usize>,
    /// For the speed: bytes done at the last update, and when.
    last: Cell<(Instant, u64)>,
    speed: Cell<f64>,
    timer: RefCell<Option<glib::SourceId>>,
}

/// Counts the bytes read through it, for the progress bar.
struct Counted<R> {
    inner: R,
    count: Arc<AtomicU64>,
}

impl<R: AsyncRead + Unpin> AsyncRead for Counted<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        let read = buf.filled().len() - before;
        self.count.fetch_add(read as u64, Ordering::Relaxed);
        result
    }
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

/// SFTP tuning for speed on slow links and long, idle sessions.
fn sftp_config() -> russh_sftp::client::Config {
    russh_sftp::client::Config {
        // Writes fill OpenSSH's 2 MB channel window like reads already do
        // (the default is 16 x 32 KB), for full speed on slow links.
        max_write_packet_len: 128 * 1024,
        max_concurrent_writes: 32,
        // Busy servers and big folders can take a while; a dead link is
        // caught by ssh's keep-alives instead.
        request_timeout_secs: 60,
        ..Default::default()
    }
}

async fn connect(args: Vec<String>) -> Result<(SftpSession, tokio::process::Child)> {
    connect_with(args, sftp_config()).await
}

async fn connect_with(
    args: Vec<String>,
    config: russh_sftp::client::Config,
) -> Result<(SftpSession, tokio::process::Child)> {
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
    match SftpSession::new_with_config(tokio::io::join(stdout, stdin), config).await {
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
                path: join(&path, &e.file_name()),
                name: e.file_name(),
                dir: kind.is_dir(),
                link: kind.is_symlink(),
                size: e.metadata().len(),
                modified: e.metadata().mtime.map(i64::from),
            }
        })
        .collect();
    sort_entries(&mut entries);
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

const TRANSFER_BUFFER: usize = 1024 * 1024;

/// Copy a remote file to a new local file; a failed copy leaves no file behind.
async fn download(
    sftp: Arc<SftpSession>,
    remote: String,
    local: PathBuf,
    count: Arc<AtomicU64>,
) -> Result<()> {
    // Large buffers: few local writes and full-size SFTP requests.
    let inner = sftp.open(remote).await?;
    let mut source = BufReader::with_capacity(TRANSFER_BUFFER, Counted { inner, count });
    let mut target = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&local)
        .await?;
    let copied = async {
        tokio::io::copy_buf(&mut source, &mut target).await?;
        target.flush().await
    }
    .await;
    if let Err(e) = copied {
        drop(target);
        let _ = tokio::fs::remove_file(&local).await;
        return Err(e.into());
    }
    Ok(())
}

/// Copy a local file to a new remote file; never replaces one that exists.
async fn upload(
    sftp: Arc<SftpSession>,
    local: PathBuf,
    remote: String,
    count: Arc<AtomicU64>,
) -> Result<()> {
    let inner = tokio::fs::File::open(&local).await?;
    let mut source = BufReader::with_capacity(TRANSFER_BUFFER, Counted { inner, count });
    // EXCLUDE makes the server refuse an existing name, with no check-then-create race.
    let opened = sftp
        .open_with_flags(
            remote.clone(),
            OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
        )
        .await;
    let mut target = match opened {
        Ok(file) => file,
        Err(e) => {
            // Servers report a taken name as a generic failure: say which it was.
            if sftp.symlink_metadata(remote.clone()).await.is_ok() {
                bail!("{remote} already exists");
            }
            return Err(e.into());
        }
    };
    let copied = async {
        tokio::io::copy_buf(&mut source, &mut target).await?;
        target.shutdown().await
    }
    .await;
    if let Err(e) = copied {
        drop(target);
        let _ = sftp.remove_file(remote).await;
        return Err(e.into());
    }
    Ok(())
}

/// Folders first, then by name.
fn sort_entries(entries: &mut [Entry]) {
    // Each lowercase name is made once, not twice per comparison. Stable,
    // like the comparison sort: equal names keep their order.
    entries.sort_by_cached_key(|e| (!e.dir, e.name.to_lowercase()));
}

/// A local folder: `~` is the home folder. Links to folders count as folders.
fn list_local(path: String) -> Result<(String, Vec<Entry>)> {
    let path = match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            store::home_dir().join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(path),
    };
    let path = std::fs::canonicalize(&path)?;
    let dir = path
        .to_str()
        .context("This folder's name is not valid UTF-8")?
        .to_owned();
    let mut entries: Vec<Entry> = std::fs::read_dir(&path)?
        .filter_map(|e| {
            let e = e.ok()?;
            let name = e.file_name().into_string().ok().filter(|n| safe_name(n))?;
            let link = e.file_type().ok()?.is_symlink();
            let meta = std::fs::metadata(e.path()).ok();
            Some(Entry {
                path: join(&dir, &name),
                name,
                dir: meta.as_ref().is_some_and(|m| m.is_dir()),
                link,
                modified: meta.as_ref().map(std::os::unix::fs::MetadataExt::mtime),
                size: meta.map_or(0, |m| m.len()),
            })
        })
        .collect();
    sort_entries(&mut entries);
    Ok((dir, entries))
}

/// The file-type icon from the icon theme, in color: the type is guessed from
/// the name alone (shared-mime-info), so listing stays fast.
fn file_icon(entry: &Entry) -> gio::Icon {
    if entry.dir {
        return gio::ThemedIcon::from_names(&["inode-directory", "folder"]).upcast();
    }
    if entry.link {
        return gio::ThemedIcon::from_names(&[
            "inode-symlink",
            "emblem-symbolic-link",
            "text-x-generic",
        ])
        .upcast();
    }
    let (content_type, _) = gio::content_type_guess(Some(&entry.name), None);
    gio::content_type_get_icon(&content_type)
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .build()
}

/// A flat button with an icon and a label.
fn transfer_button(label: &str, icon: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .child(
            &adw::ButtonContent::builder()
                .icon_name(icon)
                .label(label)
                .build(),
        )
        .tooltip_text(tooltip)
        .css_classes(["flat"])
        .build()
}

/// `2026-10-05 18:50`, in local time.
fn format_time(seconds: i64) -> String {
    glib::DateTime::from_unix_local(seconds)
        .and_then(|time| time.format("%Y-%m-%d %H:%M"))
        .map(String::from)
        .unwrap_or_default()
}

fn column(text: &str, width: i32) -> gtk::Label {
    let label = gtk::Label::builder()
        .label(text)
        .xalign(1.0)
        .width_chars(width)
        .build();
    label.add_css_class("dim-label");
    label
}

impl Pane {
    /// `title` with an optional icon: "Local", or the host.
    fn new(title: &str, icon: Option<&str>) -> Pane {
        let heading = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        heading.set_margin_start(6);
        heading.set_margin_end(4);
        if let Some(icon) = icon {
            heading.append(&gtk::Image::from_icon_name(icon));
        }
        heading.append(
            &gtk::Label::builder()
                .label(title)
                .css_classes(["heading"])
                .max_width_chars(18)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build(),
        );
        let up = icon_button("go-up-symbolic", "Parent Folder");
        let refresh = icon_button("view-refresh-symbolic", "Refresh");
        let hidden = gtk::ToggleButton::builder()
            .icon_name("view-conceal-symbolic")
            .tooltip_text("Show Hidden Files")
            .build();
        for button in [
            up.upcast_ref::<gtk::Widget>(),
            refresh.upcast_ref(),
            hidden.upcast_ref(),
        ] {
            button.add_css_class("flat");
        }
        let path = gtk::Entry::builder().hexpand(true).build();
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        bar.add_css_class("card");
        bar.add_css_class("pane-bar");
        bar.append(&heading);
        bar.append(&up);
        bar.append(&path);
        bar.append(&refresh);
        bar.append(&hidden);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.add_css_class("file-header");
        let name = gtk::Label::builder()
            .label("Name")
            .xalign(0.0)
            .hexpand(true)
            .build();
        name.add_css_class("dim-label");
        header.append(&name);
        header.append(&column("Size", 9));
        header.append(&column("Modified", 16));
        let mut child = header.first_child();
        while let Some(widget) = child {
            widget.add_css_class("caption-heading");
            child = widget.next_sibling();
        }
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Multiple);
        list.set_activate_on_single_click(false);
        list.add_css_class("navigation-sidebar");
        list.add_css_class("file-list");
        let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        card.add_css_class("card");
        card.set_overflow(gtk::Overflow::Hidden);
        card.append(&header);
        card.append(
            &gtk::ScrolledWindow::builder()
                .child(&list)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .vexpand(true)
                .build(),
        );

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        actions.add_css_class("card");
        actions.add_css_class("pane-bar");

        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.set_hexpand(true);
        root.append(&bar);
        root.append(&card);
        root.append(&actions);
        Pane {
            root,
            bar,
            up,
            refresh,
            hidden,
            path,
            list,
            card,
            actions,
            cwd: RefCell::default(),
            all: RefCell::default(),
            entries: RefCell::default(),
        }
    }

    fn show(&self, cwd: String, entries: Vec<Entry>) {
        self.path.set_text(&cwd);
        *self.cwd.borrow_mut() = cwd;
        *self.all.borrow_mut() = entries;
        self.render();
    }

    /// Fill the list from `all`, leaving out dot files unless shown.
    fn render(&self) {
        let show_hidden = self.hidden.is_active();
        let entries: Vec<Entry> = self
            .all
            .borrow()
            .iter()
            .filter(|e| show_hidden || !e.name.starts_with('.'))
            .cloned()
            .collect();
        self.list.remove_all();
        for e in &entries {
            let name = gtk::Label::builder()
                .label(&e.name)
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .build();
            let size = column(
                &if e.dir {
                    String::new()
                } else {
                    human_size(e.size)
                },
                9,
            );
            let modified = column(&e.modified.map(format_time).unwrap_or_default(), 16);
            let icon = gtk::Image::from_gicon(&file_icon(e));
            icon.set_pixel_size(24);
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            content.append(&icon);
            content.append(&name);
            content.append(&size);
            content.append(&modified);
            self.list.append(&content);
        }
        *self.entries.borrow_mut() = entries;
    }

    /// Show the current folder in the path bar again (after a failed jump).
    fn reset_path(&self) {
        self.path.set_text(&self.cwd.borrow());
    }

    fn entry_at(&self, row: &gtk::ListBoxRow) -> Option<Entry> {
        let index = usize::try_from(row.index()).ok()?;
        self.entries.borrow().get(index).cloned()
    }

    /// The entries an action applies to: the selection if it includes the
    /// clicked row, otherwise just the clicked row.
    fn targets(&self, clicked: Option<gtk::ListBoxRow>) -> Vec<Entry> {
        if let Some(row) = clicked
            && !row.is_selected()
        {
            self.list.unselect_all();
            self.list.select_row(Some(&row));
        }
        self.list
            .selected_rows()
            .iter()
            .filter_map(|row| self.entry_at(row))
            .collect()
    }
}

pub fn open(app: &Rc<App>, alias: &str) {
    let local = Pane::new("Local", None);
    let remote = Pane::new(alias, Some("computer-symbolic"));
    let new_folder = icon_button("folder-new-symbolic", "New Folder");
    new_folder.add_css_class("flat");
    remote
        .bar
        .insert_child_after(&new_folder, Some(&remote.refresh));

    // Upload under your files, Download under the server's, as in sshPilot.
    let upload = transfer_button(
        "Upload",
        "document-send-symbolic",
        "Copy the selected files to the server's folder",
    );
    let download = transfer_button(
        "Download",
        "folder-download-symbolic",
        "Copy the selected server files to your folder",
    );
    local.actions.append(&upload);
    remote.actions.append(&download);

    let panes = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    panes.set_homogeneous(true);
    panes.set_vexpand(true);
    panes.set_margin_top(6);
    panes.set_margin_start(6);
    panes.set_margin_end(6);
    panes.append(&local.root);
    panes.append(&remote.root);

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
    let bar = gtk::ProgressBar::builder()
        .show_text(true)
        .visible(false)
        .margin_start(8)
        .margin_end(8)
        .build();
    page_box.append(&panes);
    page_box.append(&bar);
    page_box.append(&status);

    let browser = Rc::new(Browser {
        app: app.clone(),
        alias: alias.to_owned(),
        sftp: RefCell::new(None),
        ssh: RefCell::new(None),
        connecting: Cell::new(false),
        waiting: RefCell::default(),
        closed: Cell::new(false),
        local,
        remote,
        status,
        progress: Progress {
            bar,
            items: RefCell::default(),
            uploads: Cell::new(0),
            downloads: Cell::new(0),
            last: Cell::new((Instant::now(), 0)),
            speed: Cell::new(0.0),
            timer: RefCell::default(),
        },
    });
    let (local, remote) = (&browser.local, &browser.remote);

    // Local side.
    local.up.connect_clicked(glib::clone!(
        #[strong]
        browser,
        move |_| {
            let target = parent(&browser.local.cwd.borrow());
            browser.navigate_local(target);
        }
    ));
    local.path.connect_activate(glib::clone!(
        #[strong]
        browser,
        move |entry| browser.navigate_local(entry.text().to_string())
    ));
    local.refresh.connect_clicked(glib::clone!(
        #[strong]
        browser,
        move |_| browser.refresh_local()
    ));
    local.list.connect_row_activated(glib::clone!(
        #[strong]
        browser,
        move |_, row| match browser.local.entry_at(row) {
            Some(e) if e.dir => browser.navigate_local(e.path),
            Some(e) => browser.upload_files(vec![PathBuf::from(e.path)]),
            None => {}
        }
    ));

    for server in [false, true] {
        let pane = if server { remote } else { local };
        pane.hidden.connect_toggled(glib::clone!(
            #[weak(rename_to = this)]
            browser,
            move |_| {
                let pane = if server { &this.remote } else { &this.local };
                pane.render();
            }
        ));
    }

    // Server side.
    remote.up.connect_clicked(glib::clone!(
        #[strong]
        browser,
        move |_| {
            let target = parent(&browser.remote.cwd.borrow());
            browser.navigate(target);
        }
    ));
    remote.path.connect_activate(glib::clone!(
        #[strong]
        browser,
        move |entry| browser.navigate(entry.text().to_string())
    ));
    remote.refresh.connect_clicked(glib::clone!(
        #[strong]
        browser,
        move |_| browser.refresh()
    ));
    new_folder.connect_clicked(glib::clone!(
        #[strong]
        browser,
        move |_| browser.ask_new_folder()
    ));
    remote.list.connect_row_activated(glib::clone!(
        #[strong]
        browser,
        move |_, row| match browser.remote.entry_at(row) {
            Some(e) if e.dir => browser.navigate(e.path),
            Some(e) if e.link => browser.open_link(e),
            Some(e) => browser.download(vec![e]),
            None => {}
        }
    ));

    upload.connect_clicked(glib::clone!(
        #[strong]
        browser,
        move |_| {
            let selected = browser.local.targets(None);
            browser.upload_entries(selected);
        }
    ));
    download.connect_clicked(glib::clone!(
        #[strong]
        browser,
        move |_| {
            let selected = browser.remote.targets(None);
            browser.download(selected);
        }
    ));

    for (pane, server) in [(local, false), (remote, true)] {
        let menu_click = gtk::GestureClick::builder()
            .button(gdk::BUTTON_SECONDARY)
            .build();
        menu_click.connect_pressed(glib::clone!(
            #[strong]
            browser,
            move |_, _, x, y| browser.context_menu(server, x, y)
        ));
        pane.list.add_controller(menu_click);
    }

    // Drop files from the desktop on the server side to upload them.
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
    remote.card.add_controller(drop);

    app.connected_to(alias);
    let page = app.tabs.append(&page_box);
    page.set_title(&format!("Files — {alias}"));
    page.set_keyword(alias);
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

    browser.navigate_local("~".to_owned());
    browser.navigate(".".to_owned());
}

impl Browser {
    /// End the SFTP session: let ssh exit on its own, kill it if it hangs.
    fn disconnect(&self) {
        self.closed.set(true);
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

    /// Forget the session if its ssh has exited (dropped connection, or the
    /// shared connection was closed), so the next action reconnects.
    fn drop_if_dead(&self) {
        let _runtime = crate::runtime().enter();
        let mut ssh = self.ssh.borrow_mut();
        if let Some(child) = ssh.as_mut()
            && !matches!(child.try_wait(), Ok(None))
        {
            *ssh = None;
            self.sftp.borrow_mut().take();
        }
    }

    /// Run `then` with a live session, connecting (again) first if needed:
    /// after a long idle or a dropped link, the next action just reconnects.
    /// Actions asked for while connecting wait for that connection.
    fn with_session(self: &Rc<Self>, then: impl FnOnce(&Rc<Browser>, Arc<SftpSession>) + 'static) {
        if self.closed.get() {
            return;
        }
        self.drop_if_dead();
        let live = self.sftp.borrow().clone();
        if let Some(sftp) = live {
            then(self, sftp);
            return;
        }
        self.waiting.borrow_mut().push(Box::new(then));
        if self.connecting.replace(true) {
            return;
        }
        self.say(&format!("Connecting to {}…", self.alias));
        let this = self.clone();
        spawn(connect(ssh::sftp_args(&self.alias)), move |result| {
            this.connecting.set(false);
            let waiting = this.waiting.take();
            if this.closed.get() {
                // The tab closed meanwhile; dropping the session ends its ssh.
                return;
            }
            match result {
                Ok((sftp, child)) => {
                    let sftp = Arc::new(sftp);
                    *this.sftp.borrow_mut() = Some(sftp.clone());
                    *this.ssh.borrow_mut() = Some(child);
                    this.say(&format!("Connected to {}", this.alias));
                    for then in waiting {
                        then(&this, sftp.clone());
                    }
                }
                Err(e) => this.say(&format!(
                    "Could not connect: {e}\nIf this host asks for a password, open a terminal to it first, \
                     then press Enter in the server path field to retry."
                )),
            }
        });
    }

    /// Show what went wrong, noting when it was the connection that dropped.
    fn report(&self, message: String) {
        self.drop_if_dead();
        if self.sftp.borrow().is_none() && !self.connecting.get() {
            self.say(&format!(
                "{message}\nThe connection was lost; your next action reconnects."
            ));
        } else {
            self.say(&message);
        }
    }

    /// Show `target` on the local side.
    fn navigate_local(self: &Rc<Self>, target: String) {
        let this = self.clone();
        spawn(
            async move { tokio::task::spawn_blocking(move || list_local(target)).await? },
            move |result| match result {
                Ok((path, entries)) => this.local.show(path, entries),
                Err(e) => {
                    this.local.reset_path();
                    this.say(&format!("Could not open that folder: {e}"));
                }
            },
        );
    }

    fn refresh_local(self: &Rc<Self>) {
        let cwd = self.local.cwd.borrow().clone();
        self.navigate_local(cwd);
    }

    /// Show `target` on the server side, connecting first if needed.
    fn navigate(self: &Rc<Self>, target: String) {
        self.list_remote(target, true);
    }

    fn list_remote(self: &Rc<Self>, target: String, retry: bool) {
        self.with_session(move |this, sftp| {
            let this = this.clone();
            spawn(list_dir(sftp, target.clone()), move |result| match result {
                Ok((path, entries)) => this.remote.show(path, entries),
                Err(e) => {
                    this.drop_if_dead();
                    if retry && this.sftp.borrow().is_none() {
                        // The connection died while idle: reconnect once, quietly.
                        this.list_remote(target, false);
                        return;
                    }
                    this.remote.reset_path();
                    this.report(format!("Could not open that folder: {e}"));
                }
            });
        });
    }

    fn refresh(self: &Rc<Self>) {
        let cwd = self.remote.cwd.borrow().clone();
        self.navigate(if cwd.is_empty() { ".".to_owned() } else { cwd });
    }

    /// A server symlink: open it if it leads to a folder, otherwise download it.
    fn open_link(self: &Rc<Self>, entry: Entry) {
        self.with_session(move |this, sftp| {
            let this = this.clone();
            let path = entry.path.clone();
            spawn(
                async move { Ok(sftp.metadata(path).await?.is_dir()) },
                move |result| match result {
                    Ok(true) => this.navigate(entry.path),
                    Ok(false) => this.download(vec![entry]),
                    Err(e) => this.report(format!("Could not open {}: {e}", entry.name)),
                },
            );
        });
    }

    fn context_menu(self: &Rc<Self>, server: bool, x: f64, y: f64) {
        let pane = if server { &self.remote } else { &self.local };
        let row = pane.list.row_at_y(y as i32);
        let item = |f: fn(&Rc<Browser>, Vec<Entry>), targets: Vec<Entry>| -> Box<dyn Fn()> {
            let this = self.clone();
            Box::new(move || f(&this, targets.clone()))
        };
        let items: sidebar::MenuItems = match (server, row.is_some()) {
            (true, true) => {
                let targets = pane.targets(row);
                let mut items = vec![("Download", item(Browser::download, targets.clone()))];
                if targets.len() == 1 {
                    items.push(("Rename…", item(Browser::ask_rename, targets.clone())));
                }
                items.push(("Delete…", item(Browser::ask_delete, targets)));
                items
            }
            (true, false) => vec![
                ("New Folder…", item(|b, _| b.ask_new_folder(), vec![])),
                ("Refresh", item(|b, _| b.refresh(), vec![])),
            ],
            (false, true) => vec![("Upload", item(Browser::upload_entries, pane.targets(row)))],
            (false, false) => vec![("Refresh", item(|b, _| b.refresh_local(), vec![]))],
        };
        sidebar::popup_menu(pane.list.upcast_ref(), x, y, items);
    }

    /// Copy server files into the folder shown on the local side.
    fn download(self: &Rc<Self>, targets: Vec<Entry>) {
        if targets.is_empty() {
            self.say("Select files on the server side first.");
            return;
        }
        let files: Vec<Entry> = targets.into_iter().filter(|e| !e.dir).collect();
        if files.is_empty() {
            self.say("Folders can't be downloaded yet; open one and pick its files.");
            return;
        }
        let folder = PathBuf::from(self.local.cwd.borrow().clone());
        self.with_session(move |this, sftp| {
            for e in files {
                let local = free_local_path(&folder, &e.name);
                this.say(&format!("Downloading {}…", e.name));
                let count = this.transfer_started(false, e.size);
                let this = this.clone();
                let shown = local.display().to_string();
                let work = download(sftp.clone(), e.path, local, count.clone());
                spawn(work, move |result| {
                    this.transfer_finished(false, &count);
                    match result {
                        Ok(()) => this.say(&format!("Saved {shown}")),
                        Err(err) => this.report(format!("Download of {} failed: {err}", e.name)),
                    }
                    this.refresh_local();
                });
            }
        });
    }

    /// Copy files selected on the local side to the folder shown on the server side.
    fn upload_entries(self: &Rc<Self>, targets: Vec<Entry>) {
        if targets.is_empty() {
            self.say("Select files on the local side first.");
            return;
        }
        if targets.iter().all(|e| e.dir) {
            self.say("Folders can't be uploaded yet; open one and pick its files.");
            return;
        }
        let paths = targets
            .into_iter()
            .filter(|e| !e.dir)
            .map(|e| PathBuf::from(e.path))
            .collect();
        self.upload_files(paths);
    }

    fn upload_files(self: &Rc<Self>, paths: Vec<PathBuf>) {
        let mut files = Vec::new();
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
            files.push((local, name));
        }
        if files.is_empty() {
            return;
        }
        let folder = self.remote.cwd.borrow().clone();
        self.with_session(move |this, sftp| {
            for (local, name) in files {
                let remote = join(&folder, &name);
                this.say(&format!("Uploading {name}…"));
                let size = std::fs::metadata(&local).map_or(0, |m| m.len());
                let count = this.transfer_started(true, size);
                let this = this.clone();
                let work = upload(sftp.clone(), local, remote, count.clone());
                spawn(work, move |result| {
                    this.transfer_finished(true, &count);
                    match result {
                        Ok(()) => {
                            this.refresh();
                            this.say(&format!("Uploaded {name}"));
                        }
                        Err(e) => this.report(format!("Upload of {name} failed: {e}")),
                    }
                });
            }
        });
    }

    /// Count a new transfer of `size` bytes into the progress bar; returns
    /// the counter the transfer adds its bytes to.
    fn transfer_started(self: &Rc<Self>, upload: bool, size: u64) -> Arc<AtomicU64> {
        let progress = &self.progress;
        let count = Arc::new(AtomicU64::new(0));
        if progress.items.borrow().is_empty() {
            progress.last.set((Instant::now(), 0));
            progress.speed.set(0.0);
        }
        progress.items.borrow_mut().push((count.clone(), size));
        let kind = if upload {
            &progress.uploads
        } else {
            &progress.downloads
        };
        kind.set(kind.get() + 1);
        progress.bar.set_visible(true);
        if progress.timer.borrow().is_none() {
            let this = Rc::downgrade(self);
            let timer =
                glib::timeout_add_local(Duration::from_millis(200), move || match this.upgrade() {
                    Some(this) => {
                        this.show_progress();
                        glib::ControlFlow::Continue
                    }
                    None => glib::ControlFlow::Break,
                });
            *progress.timer.borrow_mut() = Some(timer);
        }
        self.show_progress();
        count
    }

    /// A transfer ended (done or failed): it counts as complete in the bar.
    fn transfer_finished(self: &Rc<Self>, upload: bool, count: &Arc<AtomicU64>) {
        let progress = &self.progress;
        if let Some((count, size)) = progress
            .items
            .borrow()
            .iter()
            .find(|(c, _)| Arc::ptr_eq(c, count))
        {
            count.fetch_max(*size, Ordering::Relaxed);
        }
        let kind = if upload {
            &progress.uploads
        } else {
            &progress.downloads
        };
        kind.set(kind.get().saturating_sub(1));
        self.show_progress();
        if progress.uploads.get() + progress.downloads.get() > 0 {
            return;
        }
        if let Some(timer) = progress.timer.take() {
            timer.remove();
        }
        // Leave the full bar up a moment, unless new transfers start.
        let this = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            if let Some(this) = this.upgrade()
                && this.progress.uploads.get() + this.progress.downloads.get() == 0
            {
                this.progress.items.borrow_mut().clear();
                this.progress.bar.set_visible(false);
            }
        });
    }

    fn show_progress(&self) {
        let progress = &self.progress;
        let (done, total) =
            progress
                .items
                .borrow()
                .iter()
                .fold((0, 0), |(done, total), (count, size)| {
                    let moved = count.load(Ordering::Relaxed);
                    // A file can grow while it is copied.
                    (done + moved, total + moved.max(*size))
                });
        let (then, done_then) = progress.last.get();
        let seconds = then.elapsed().as_secs_f64();
        if seconds >= 0.5 {
            let now = (done - done_then.min(done)) as f64 / seconds;
            // Smooth it, so the number is readable.
            let speed = match progress.speed.get() {
                0.0 => now,
                old => old * 0.6 + now * 0.4,
            };
            progress.speed.set(speed);
            progress.last.set((Instant::now(), done));
        }
        let (up, down) = (progress.uploads.get(), progress.downloads.get());
        let files = |n: usize| {
            if n == 1 {
                "1 file".to_owned()
            } else {
                format!("{n} files")
            }
        };
        let what = match (up, down) {
            (0, 0) => "Done".to_owned(),
            (up, 0) => format!("Uploading {}", files(up)),
            (0, down) => format!("Downloading {}", files(down)),
            (up, down) => format!("Transferring {}", files(up + down)),
        };
        let mut text = format!("{what} · {} of {}", human_size(done), human_size(total));
        if up + down > 0 && progress.speed.get() > 0.0 {
            text += &format!(" · {}/s", human_size(progress.speed.get() as u64));
        }
        progress.bar.set_text(Some(&text));
        progress.bar.set_fraction(if total == 0 {
            1.0
        } else {
            done as f64 / total as f64
        });
    }

    fn ask_new_folder(self: &Rc<Self>) {
        let this = self.clone();
        dialogs::ask_text(&self.app, "New Folder", "Name", "", "Create", move |name| {
            if !safe_name(&name) {
                this.say("That name can't be used.");
                return;
            }
            let path = join(&this.remote.cwd.borrow(), &name);
            this.with_session(move |this, sftp| {
                let this = this.clone();
                spawn(
                    async move { Ok(sftp.create_dir(path).await?) },
                    move |result| match result {
                        Ok(()) => this.refresh(),
                        Err(e) => this.report(format!("Could not create {name}: {e}")),
                    },
                );
            });
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
                let from = entry.path.clone();
                let to = join(&parent(&from), &name);
                this.with_session(move |this, sftp| {
                    let this = this.clone();
                    spawn(
                        async move { Ok(sftp.rename(from, to).await?) },
                        move |result| match result {
                            Ok(()) => this.refresh(),
                            Err(e) => this.report(format!("Rename failed: {e}")),
                        },
                    );
                });
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
            let work = targets
                .iter()
                .map(|e| (e.path.clone(), e.dir && !e.link))
                .collect::<Vec<_>>();
            this.with_session(move |this, sftp| {
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
                            this.report(format!("Delete failed: {e}"));
                        }
                    },
                );
            });
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
    fn lists_local_folders_first_and_follows_links() {
        let dir = std::env::temp_dir().join(format!("sshpilot-rs-local-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Zeta")).unwrap();
        std::fs::write(dir.join("alpha.txt"), "12345").unwrap();
        std::os::unix::fs::symlink(dir.join("Zeta"), dir.join("link-to-dir")).unwrap();
        let (path, entries) = list_local(dir.to_str().unwrap().to_owned()).unwrap();
        let shown: Vec<_> = entries
            .iter()
            .map(|e| (e.name.as_str(), e.dir, e.link))
            .collect();
        assert_eq!(
            shown,
            [
                ("link-to-dir", true, true),
                ("Zeta", true, false),
                ("alpha.txt", false, false)
            ]
        );
        assert_eq!(entries[2].size, 5);
        assert_eq!(entries[2].path, join(&path, "alpha.txt"));
        assert!(list_local("~".to_owned()).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn file_icons_follow_the_type() {
        let names = |name: &str| {
            let entry = Entry {
                name: name.to_owned(),
                path: String::new(),
                dir: false,
                link: false,
                size: 0,
                modified: None,
            };
            let icon = file_icon(&entry);
            let names = icon.downcast_ref::<gio::ThemedIcon>().unwrap().names();
            names.iter().map(|n| n.to_string()).collect::<Vec<_>>()
        };
        assert!(names("photo.png").contains(&"image-png".to_owned()));
        assert!(names("backup.tar.gz").iter().any(|n| n.contains("tar")
            || n.contains("package")
            || n.contains("zip")
            || n.contains("archive")));
        assert!(names("notes.txt").contains(&"text-plain".to_owned()));
    }

    #[test]
    fn sorts_like_the_plain_comparison() {
        let names = [
            "b", "B", "a.txt", "École", "ecole", "Zeta", "ß", "SS", "b", "_x", "10", "9",
        ];
        let entries: Vec<Entry> = names
            .iter()
            .enumerate()
            .map(|(i, name)| Entry {
                name: (*name).to_owned(),
                // Tells equal names apart, to check their order is kept.
                path: i.to_string(),
                dir: i % 3 == 0,
                link: false,
                size: 0,
                modified: None,
            })
            .collect();
        let mut expected = entries.clone();
        expected.sort_by(|a, b| {
            b.dir
                .cmp(&a.dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        let mut sorted = entries;
        sort_entries(&mut sorted);
        let paths = |v: &[Entry]| v.iter().map(|e| e.path.clone()).collect::<Vec<_>>();
        assert_eq!(paths(&sorted), paths(&expected));
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

            upload(
                sftp.clone(),
                local.join("up.bin"),
                join(&dir, "a.bin"),
                Arc::default(),
            )
            .await
            .unwrap();
            assert!(
                upload(
                    sftp.clone(),
                    local.join("up.bin"),
                    join(&dir, "a.bin"),
                    Arc::default()
                )
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

            download(
                sftp.clone(),
                join(&dir, "b.bin"),
                local.join("down.bin"),
                Arc::default(),
            )
            .await
            .unwrap();
            assert_eq!(std::fs::read(local.join("down.bin")).unwrap(), payload);

            remove_all(sftp.clone(), dir.clone(), true).await.unwrap();
            assert!(sftp.metadata(dir).await.is_err(), "removed");
        });
        std::fs::remove_dir_all(local).unwrap();
    }
}

/// Transfer speed, tuned vs. library defaults with 8 KB copies (as before).
/// `SSHPILOT_RS_TEST_HOST=alias SSHPILOT_RS_TEST_SSH_CONFIG=config cargo test --release -- --ignored --nocapture sftp_speed`
#[cfg(test)]
mod speed_tests {
    use super::*;
    use std::time::Instant;

    #[test]
    #[ignore = "needs an SSH server"]
    fn sftp_speed() {
        let host = std::env::var("SSHPILOT_RS_TEST_HOST").expect("SSHPILOT_RS_TEST_HOST");
        let mut args = ssh::sftp_args(&host);
        if let Ok(config) = std::env::var("SSHPILOT_RS_TEST_SSH_CONFIG") {
            args.splice(1..1, ["-F".to_owned(), config]);
        }
        let local = std::env::temp_dir().join(format!("sshpilot-rs-speed-{}", std::process::id()));
        std::fs::create_dir_all(&local).unwrap();
        let size = 32 * 1024 * 1024;
        let payload: Vec<u8> = (0..size as u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(local.join("up.bin"), &payload).unwrap();
        let mb = size as f64 / 1e6;

        crate::runtime().block_on(async {
            let (tuned, _a) = connect(args.clone()).await.unwrap();
            let tuned = Arc::new(tuned);
            let (old, _b) = connect_with(args.clone(), Default::default())
                .await
                .unwrap();
            let (home, _) = list_dir(tuned.clone(), ".".into()).await.unwrap();
            let remote = join(&home, "sshpilot-rs-speed.bin");
            // Alternate which goes first, so caches and warm-up favour neither.
            for round in 0..4 {
                for tuned_turn in [round % 2 == 0, round % 2 != 0] {
                    let _ = tuned.remove_file(remote.clone()).await;
                    let _ = std::fs::remove_file(local.join("down.bin"));
                    let t = Instant::now();
                    if tuned_turn {
                        upload(
                            tuned.clone(),
                            local.join("up.bin"),
                            remote.clone(),
                            Arc::default(),
                        )
                        .await
                        .unwrap();
                    } else {
                        let mut source = tokio::fs::File::open(local.join("up.bin")).await.unwrap();
                        let mut target = old.create(remote.clone()).await.unwrap();
                        tokio::io::copy(&mut source, &mut target).await.unwrap();
                        target.shutdown().await.unwrap();
                    }
                    let up = t.elapsed().as_secs_f64();
                    let t = Instant::now();
                    if tuned_turn {
                        download(
                            tuned.clone(),
                            remote.clone(),
                            local.join("down.bin"),
                            Arc::default(),
                        )
                        .await
                        .unwrap();
                    } else {
                        let mut source = old.open(remote.clone()).await.unwrap();
                        let mut target = tokio::fs::File::create(local.join("down.bin"))
                            .await
                            .unwrap();
                        tokio::io::copy(&mut source, &mut target).await.unwrap();
                        target.flush().await.unwrap();
                    }
                    let down = t.elapsed().as_secs_f64();
                    assert_eq!(std::fs::read(local.join("down.bin")).unwrap(), payload);
                    eprintln!(
                        "round {round} {}: upload {:6.1} MB/s, download {:6.1} MB/s",
                        if tuned_turn { "tuned " } else { "before" },
                        mb / up,
                        mb / down
                    );
                }
            }
            tuned.remove_file(remote).await.unwrap();
        });
        std::fs::remove_dir_all(local).unwrap();
    }
}
