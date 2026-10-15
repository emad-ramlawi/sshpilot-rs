mod desktop;
mod dialogs;
mod files;
mod sidebar;
mod ssh;
mod store;
mod terminal;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::OnceLock;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use store::{Host, Layout};

const APP_ID: &str = "io.github.emad_ramlawi.SshPilotRs";

const CSS: &str = "
row.drop-before { box-shadow: inset 0 2px @accent_color; }
row.drop-after { box-shadow: inset 0 -2px @accent_color; }
row.drop-into { background-color: alpha(@accent_color, 0.2); }
";

/// One background runtime for SSH/SFTP I/O; the UI stays on the GTK thread.
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

/// Run `work` in the background, then `done` with its result on the GTK thread.
pub fn spawn<T: Send + 'static>(
    work: impl Future<Output = anyhow::Result<T>> + Send + 'static,
    done: impl FnOnce(anyhow::Result<T>) + 'static,
) {
    let handle = runtime().spawn(work);
    glib::spawn_future_local(async move {
        done(handle.await.unwrap_or_else(|e| Err(anyhow::anyhow!(e))));
    });
}

pub struct App {
    pub layout: RefCell<Layout>,
    /// Hosts from ~/.ssh/config, in file order.
    pub hosts: RefCell<Vec<Host>>,
    pub path: PathBuf,
    pub config_path: PathBuf,
    /// Watched files and folders of the ssh config (it can `Include` others).
    watched: RefCell<(Vec<PathBuf>, Vec<gio::FileMonitor>)>,
    /// Hosts a tab was opened to, whose shared connection to close on quit.
    pub connected: RefCell<BTreeSet<String>>,
    pub window: adw::ApplicationWindow,
    pub list: gtk::ListBox,
    pub rows: RefCell<Vec<sidebar::RowKind>>,
    pub tabs: adw::TabView,
    pub toasts: adw::ToastOverlay,
    /// Terminal text size, shown between the zoom buttons.
    pub zoom_label: gtk::Label,
}

impl App {
    pub fn toast(&self, message: &str) {
        // Plain text: messages can quote file names, which may contain `<` or `&`.
        self.toasts.add_toast(
            adw::Toast::builder()
                .title(message)
                .use_markup(false)
                .build(),
        );
    }

    pub fn host(&self, alias: &str) -> Option<Host> {
        self.hosts
            .borrow()
            .iter()
            .find(|h| h.alias == alias)
            .cloned()
    }

    /// Apply a change to the layout, save it and redraw the sidebar.
    pub fn commit(self: &Rc<Self>, change: impl FnOnce(&mut Layout) -> anyhow::Result<()>) {
        let result = change(&mut self.layout.borrow_mut());
        if let Err(e) = result.and_then(|()| self.layout.borrow().save(&self.path)) {
            self.toast(&e.to_string());
        }
        sidebar::rebuild(self);
    }

    /// Re-read ~/.ssh/config (and what it includes) and fit the layout to it.
    pub fn reload_config(self: &Rc<Self>) {
        let config = store::load_ssh_config(&self.config_path);
        self.watch(config.watch);
        let hosts = config.hosts;
        let changed = self.layout.borrow_mut().reconcile(&hosts);
        *self.hosts.borrow_mut() = hosts;
        if changed {
            self.commit(|_| Ok(()));
        } else {
            sidebar::rebuild(self);
        }
    }

    /// Reload whenever one of `paths` (config files, or folders an `Include`
    /// pattern reads) changes.
    fn watch(self: &Rc<Self>, paths: Vec<PathBuf>) {
        if self.watched.borrow().0 == paths {
            return;
        }
        let monitors = paths
            .iter()
            .filter_map(|path| {
                let monitor = gio::File::for_path(path)
                    .monitor(
                        gio::FileMonitorFlags::WATCH_MOVES,
                        None::<&gio::Cancellable>,
                    )
                    .ok()?;
                let app = Rc::downgrade(self);
                monitor.connect_changed(move |_, _, _, event| {
                    use gio::FileMonitorEvent as E;
                    if matches!(
                        event,
                        E::ChangesDoneHint
                            | E::Created
                            | E::Deleted
                            | E::Renamed
                            | E::MovedIn
                            | E::MovedOut
                    ) && let Some(app) = app.upgrade()
                    {
                        app.reload_config();
                    }
                });
                Some(monitor)
            })
            .collect();
        *self.watched.borrow_mut() = (paths, monitors);
    }

    /// Note that a tab was opened to `alias`.
    pub fn connected_to(&self, alias: &str) {
        self.connected.borrow_mut().insert(alias.to_owned());
    }

    /// Close the shared connections the tabs used, so no ssh outlives the app.
    fn close_connections(&self) {
        let children: Vec<_> = self
            .connected
            .take()
            .iter()
            .filter_map(|alias| ssh::exit_args(alias))
            .filter_map(|args| {
                std::process::Command::new(&args[0])
                    .args(&args[1..])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .ok()
            })
            .collect();
        for mut child in children {
            let _ = child.wait();
        }
    }
}

fn build(application: &adw::Application) {
    // One window: a second launch only raises it (two would overwrite each
    // other's layout file).
    if let Some(window) = application.active_window() {
        window.present();
        return;
    }
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().expect("display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let path = Layout::default_path();
    let (layout, load_error) = match Layout::load(&path) {
        Ok(layout) => (layout, None),
        Err(e) => {
            // Keep the unreadable file: the next save would overwrite it.
            let backup = path.with_extension("json.broken");
            let note = match std::fs::rename(&path, &backup) {
                Ok(()) => format!("{e}. It was kept as {}", backup.display()),
                Err(_) => e.to_string(),
            };
            (Layout::default(), Some(note))
        }
    };

    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title("SSH Pilot RS")
        .default_width(1100)
        .default_height(700)
        .build();

    // Sidebar: connection list with a single "+" menu.
    let list = gtk::ListBox::new();
    list.add_css_class("navigation-sidebar");
    list.set_activate_on_single_click(false);
    let add_menu = gio::Menu::new();
    add_menu.append(Some("New Connection"), Some("win.new-connection"));
    add_menu.append(Some("New Group"), Some("win.new-group"));
    add_menu.append(Some("New Local Terminal"), Some("win.local-terminal"));
    let add_button = gtk::MenuButton::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Add")
        .menu_model(&add_menu)
        .build();
    let sidebar_header = adw::HeaderBar::new();
    sidebar_header.set_title_widget(Some(&adw::WindowTitle::new("Connections", "")));
    sidebar_header.pack_start(&add_button);
    let sidebar_view = adw::ToolbarView::new();
    sidebar_view.add_top_bar(&sidebar_header);
    sidebar_view.set_content(Some(
        &gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&list)
            .vexpand(true)
            .build(),
    ));

    // Content: tabs, or a hint when nothing is open.
    let tabs = adw::TabView::new();
    let tab_bar = adw::TabBar::new();
    tab_bar.set_view(Some(&tabs));
    // Show the tab even when only one connection is open.
    tab_bar.set_autohide(false);
    let empty = adw::StatusPage::builder()
        .icon_name("network-server-symbolic")
        .title("No Open Connections")
        .description("Hosts come from ~/.ssh/config.\nDouble-click one to open a terminal; right-click it for files.")
        .build();
    let stack = gtk::Stack::new();
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&tabs, Some("tabs"));
    // Terminal text size: [-] [100%] [+]
    let zoom_label = gtk::Label::new(None);
    zoom_label.set_width_chars(5);
    let zoom_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    zoom_box.add_css_class("linked");
    for (icon, tooltip, action) in [
        ("zoom-out-symbolic", "Smaller Text (Ctrl+−)", "win.zoom-out"),
        ("", "Default Text Size (Ctrl+0)", "win.zoom-reset"),
        ("zoom-in-symbolic", "Larger Text (Ctrl++)", "win.zoom-in"),
    ] {
        let button = gtk::Button::builder()
            .tooltip_text(tooltip)
            .action_name(action)
            .build();
        if icon.is_empty() {
            button.set_child(Some(&zoom_label));
        } else {
            button.set_icon_name(icon);
        }
        zoom_box.append(&button);
    }
    let content_header = adw::HeaderBar::new();
    content_header.pack_end(&zoom_box);
    let content_view = adw::ToolbarView::new();
    content_view.add_top_bar(&content_header);
    content_view.add_top_bar(&tab_bar);
    content_view.set_content(Some(&stack));
    tabs.connect_n_pages_notify(glib::clone!(
        #[weak]
        stack,
        #[weak]
        tab_bar,
        move |tabs| {
            let open = tabs.n_pages() > 0;
            stack.set_visible_child_name(if open { "tabs" } else { "empty" });
            tab_bar.set_visible(open);
        }
    ));
    tab_bar.set_visible(false);

    let split = adw::OverlaySplitView::new();
    split.set_sidebar(Some(&sidebar_view));
    split.set_content(Some(&content_view));
    split.set_min_sidebar_width(240.0);

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&split));
    window.set_content(Some(&toasts));

    let app = Rc::new(App {
        layout: RefCell::new(layout),
        hosts: RefCell::new(Vec::new()),
        path,
        config_path: store::ssh_config_path(),
        watched: RefCell::default(),
        connected: RefCell::default(),
        window: window.clone(),
        list,
        rows: RefCell::new(Vec::new()),
        tabs,
        toasts,
        zoom_label,
    });
    terminal::show_zoom(&app);

    let new_connection = gio::SimpleAction::new("new-connection", None);
    new_connection.connect_activate(glib::clone!(
        #[strong]
        app,
        move |_, _| dialogs::new_host(&app)
    ));
    let new_group = gio::SimpleAction::new("new-group", None);
    new_group.connect_activate(glib::clone!(
        #[strong]
        app,
        move |_, _| dialogs::new_group(&app)
    ));
    let local_terminal = gio::SimpleAction::new("local-terminal", None);
    local_terminal.connect_activate(glib::clone!(
        #[strong]
        app,
        move |_, _| terminal::open_local(&app)
    ));
    window.add_action(&new_connection);
    window.add_action(&new_group);
    window.add_action(&local_terminal);
    for (name, larger) in [
        ("zoom-in", Some(true)),
        ("zoom-out", Some(false)),
        ("zoom-reset", None),
    ] {
        let zoom = gio::SimpleAction::new(name, None);
        zoom.connect_activate(glib::clone!(
            #[strong]
            app,
            move |_, _| terminal::zoom(&app, larger)
        ));
        window.add_action(&zoom);
    }
    application.set_accels_for_action(
        "win.zoom-in",
        &["<Ctrl>plus", "<Ctrl>equal", "<Ctrl>KP_Add"],
    );
    application.set_accels_for_action("win.zoom-out", &["<Ctrl>minus", "<Ctrl>KP_Subtract"]);
    application.set_accels_for_action("win.zoom-reset", &["<Ctrl>0", "<Ctrl>KP_0"]);
    application.set_accels_for_action("win.new-connection", &["<Ctrl>n"]);
    application.set_accels_for_action("win.local-terminal", &["<Ctrl><Shift>t"]);

    sidebar::setup(&app);
    app.reload_config();
    application.connect_shutdown(glib::clone!(
        #[strong]
        app,
        move |_| app.close_connections()
    ));
    if let Some(error) = load_error {
        app.toast(&error);
    }
    window.present();
}

const USAGE: &str = "\
Usage: sshpilot-rs [OPTION]

  (none)        Open the app
  --install     Add the app menu entry and icon for this user
                (~/.local/share), starting this binary
  --uninstall   Remove them
  --version     Show the version
";

fn main() -> glib::ExitCode {
    let done = |result: anyhow::Result<()>| match result {
        Ok(()) => glib::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sshpilot-rs: {e:#}");
            glib::ExitCode::FAILURE
        }
    };
    match std::env::args().nth(1).as_deref() {
        Some("--install") => return done(desktop::install()),
        Some("--uninstall") => return done(desktop::uninstall()),
        Some("--version" | "-V") => {
            println!("sshpilot-rs {}", env!("CARGO_PKG_VERSION"));
            return glib::ExitCode::SUCCESS;
        }
        Some("--help" | "-h") => {
            print!("{USAGE}");
            return glib::ExitCode::SUCCESS;
        }
        Some(other) => {
            eprint!("sshpilot-rs: unknown option {other}\n\n{USAGE}");
            return glib::ExitCode::FAILURE;
        }
        None => {}
    }
    let application = adw::Application::builder().application_id(APP_ID).build();
    // The installed icon (see scripts/install.sh), for X11 window lists.
    application.connect_startup(|_| gtk::Window::set_default_icon_name(APP_ID));
    application.connect_activate(build);
    application.run()
}
