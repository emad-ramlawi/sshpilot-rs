mod dialogs;
mod files;
mod sidebar;
mod ssh;
mod store;
mod terminal;

use std::cell::RefCell;
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
    config_monitor: RefCell<Option<gio::FileMonitor>>,
    pub window: adw::ApplicationWindow,
    pub list: gtk::ListBox,
    pub rows: RefCell<Vec<sidebar::RowKind>>,
    pub tabs: adw::TabView,
    pub toasts: adw::ToastOverlay,
}

impl App {
    pub fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
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

    /// Re-read ~/.ssh/config and fit the layout to it.
    pub fn reload_config(self: &Rc<Self>) {
        let text = std::fs::read_to_string(&self.config_path).unwrap_or_default();
        let hosts = store::parse_ssh_config(&text);
        let changed = self.layout.borrow_mut().reconcile(&hosts);
        *self.hosts.borrow_mut() = hosts;
        if changed {
            self.commit(|_| Ok(()));
        } else {
            sidebar::rebuild(self);
        }
    }

    /// Reload whenever ~/.ssh/config is saved.
    fn watch_config(self: &Rc<Self>) {
        let file = gio::File::for_path(&self.config_path);
        let Ok(monitor) = file.monitor_file(
            gio::FileMonitorFlags::WATCH_MOVES,
            None::<&gio::Cancellable>,
        ) else {
            return;
        };
        let app = Rc::downgrade(self);
        monitor.connect_changed(move |_, _, _, event| {
            use gio::FileMonitorEvent as E;
            if matches!(
                event,
                E::ChangesDoneHint | E::Created | E::Deleted | E::Renamed | E::MovedIn
            ) && let Some(app) = app.upgrade()
            {
                app.reload_config();
            }
        });
        *self.config_monitor.borrow_mut() = Some(monitor);
    }
}

fn build(application: &adw::Application) {
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
        Err(e) => (Layout::default(), Some(e.to_string())),
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
    let empty = adw::StatusPage::builder()
        .icon_name("network-server-symbolic")
        .title("No Open Connections")
        .description("Hosts come from ~/.ssh/config.\nDouble-click one to open a terminal; right-click it for files.")
        .build();
    let stack = gtk::Stack::new();
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&tabs, Some("tabs"));
    let content_view = adw::ToolbarView::new();
    content_view.add_top_bar(&adw::HeaderBar::new());
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
        config_monitor: RefCell::new(None),
        window: window.clone(),
        list,
        rows: RefCell::new(Vec::new()),
        tabs,
        toasts,
    });

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
    window.add_action(&new_connection);
    window.add_action(&new_group);
    application.set_accels_for_action("win.new-connection", &["<Ctrl>n"]);

    sidebar::setup(&app);
    app.reload_config();
    app.watch_config();
    if let Some(error) = load_error {
        app.toast(&error);
    }
    window.present();
}

fn main() -> glib::ExitCode {
    let application = adw::Application::builder().application_id(APP_ID).build();
    application.connect_activate(build);
    application.run()
}
