//! The connection list: groups, manual order by drag and drop, right-click menus.

use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib};

use crate::store::Id;
use crate::{App, dialogs, files, terminal};

/// What each list row (by index) shows.
#[derive(Clone, Debug, PartialEq)]
pub enum RowKind {
    Group {
        id: Id,
        index: usize,
    },
    Host {
        alias: String,
        group: Option<Id>,
        index: usize,
    },
    UngroupedHeader,
}

/// What is being dragged, as carried in the drag payload.
#[derive(Clone, Debug, PartialEq)]
enum Dragged {
    Host(String),
    Group(Id),
}

impl Dragged {
    fn encode(&self) -> String {
        match self {
            Dragged::Host(alias) => format!("host:{alias}"),
            Dragged::Group(id) => format!("group:{id}"),
        }
    }

    fn decode(text: &str) -> Option<Self> {
        let (kind, value) = text.split_once(':')?;
        match kind {
            "host" if !value.is_empty() => Some(Dragged::Host(value.to_owned())),
            "group" => Some(Dragged::Group(value.parse().ok()?)),
            _ => None,
        }
    }
}

/// Where a drop lands: the move to make, and the row + style that shows it.
struct Drop {
    apply: Move,
    highlight: Option<(usize, &'static str)>,
}

enum Move {
    Host {
        alias: String,
        group: Option<Id>,
        index: usize,
    },
    Group {
        id: Id,
        index: usize,
    },
}

pub fn rebuild(app: &Rc<App>) {
    app.list.remove_all();
    let mut rows = Vec::new();
    let layout = app.layout.borrow();
    for (index, group) in layout.groups.iter().enumerate() {
        app.list.append(&group_row(
            group.id,
            &group.name,
            group.hosts.len(),
            group.collapsed,
        ));
        rows.push(RowKind::Group {
            id: group.id,
            index,
        });
        if !group.collapsed {
            for (index, alias) in group.hosts.iter().enumerate() {
                app.list.append(&host_row(app, alias, true));
                rows.push(RowKind::Host {
                    alias: alias.clone(),
                    group: Some(group.id),
                    index,
                });
            }
        }
    }
    if !layout.groups.is_empty() {
        // Also the drop target for taking a connection out of its group.
        let label = gtk::Label::builder()
            .label("Ungrouped")
            .xalign(0.0)
            .css_classes(["caption-heading", "dim-label"])
            .margin_top(6)
            .build();
        let row = gtk::ListBoxRow::builder()
            .child(&label)
            .selectable(false)
            .activatable(false)
            .build();
        app.list.append(&row);
        rows.push(RowKind::UngroupedHeader);
    }
    for (index, alias) in layout.ungrouped.iter().enumerate() {
        app.list.append(&host_row(app, alias, false));
        rows.push(RowKind::Host {
            alias: alias.clone(),
            group: None,
            index,
        });
    }
    *app.rows.borrow_mut() = rows;
}

fn group_row(id: Id, name: &str, count: usize, collapsed: bool) -> gtk::ListBoxRow {
    let arrow = gtk::Image::from_icon_name(if collapsed {
        "pan-end-symbolic"
    } else {
        "pan-down-symbolic"
    });
    let label = gtk::Label::builder()
        .label(name)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    label.add_css_class("heading");
    let count = gtk::Label::new(Some(&count.to_string()));
    count.add_css_class("dim-label");
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    content.append(&arrow);
    content.append(&label);
    content.append(&count);
    let row = gtk::ListBoxRow::builder()
        .child(&content)
        .selectable(false)
        .css_classes(["group"])
        .build();
    add_drag_source(&row, Dragged::Group(id));
    row
}

fn host_row(app: &Rc<App>, alias: &str, nested: bool) -> gtk::ListBoxRow {
    let target = app.host(alias).map(|h| h.target()).unwrap_or_default();
    let title = gtk::Label::builder()
        .label(alias)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let subtitle = gtk::Label::builder()
        .label(&target)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    title.add_css_class("host-title");
    subtitle.add_css_class("caption");
    subtitle.add_css_class("dim-label");
    let text = gtk::Box::new(gtk::Orientation::Vertical, 3);
    text.append(&title);
    text.append(&subtitle);
    let icon = gtk::Image::from_icon_name("utilities-terminal-symbolic");
    icon.set_pixel_size(20);
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    content.append(&icon);
    content.append(&text);
    let row = gtk::ListBoxRow::builder()
        .child(&content)
        .css_classes(["host"])
        .build();
    if nested {
        row.set_margin_start(16);
    }
    add_drag_source(&row, Dragged::Host(alias.to_owned()));
    row
}

fn add_drag_source(row: &gtk::ListBoxRow, dragged: Dragged) {
    let source = gtk::DragSource::new();
    source.set_actions(gdk::DragAction::MOVE);
    source.connect_prepare(move |_, _, _| {
        Some(gdk::ContentProvider::for_value(
            &dragged.encode().to_value(),
        ))
    });
    source.connect_drag_begin(glib::clone!(
        #[weak]
        row,
        move |source, _| source.set_icon(Some(&gtk::WidgetPaintable::new(Some(&row))), 0, 0)
    ));
    row.add_controller(source);
}

/// Work out where `dragged` lands for the pointer at `y`.
fn drop_at(app: &App, dragged: Dragged, y: f64) -> Option<Drop> {
    let rows = app.rows.borrow();
    let layout = app.layout.borrow();
    let row = app.list.row_at_y(y as i32);
    let hit = row.as_ref().and_then(|row| {
        let index = usize::try_from(row.index()).ok()?;
        let bounds = row.compute_bounds(&app.list)?;
        let upper = y < f64::from(bounds.y() + bounds.height() / 2.0);
        Some((index, rows.get(index)?.clone(), upper))
    });

    match dragged {
        Dragged::Host(alias) => {
            let (group, index, highlight) = match hit {
                // Blank space below the list: end of the ungrouped list.
                None => (None, layout.ungrouped.len(), None),
                Some((row, RowKind::Host { group, index, .. }, upper)) => {
                    if upper {
                        (group, index, Some((row, "drop-before")))
                    } else {
                        (group, index + 1, Some((row, "drop-after")))
                    }
                }
                Some((row, RowKind::Group { id: gid, .. }, _)) => {
                    (Some(gid), 0, Some((row, "drop-into")))
                }
                Some((row, RowKind::UngroupedHeader, _)) => (None, 0, Some((row, "drop-into"))),
            };
            Some(Drop {
                apply: Move::Host {
                    alias,
                    group,
                    index,
                },
                highlight,
            })
        }
        Dragged::Group(id) => {
            let groups = layout.groups.len();
            // The group under the pointer, whether on its header or a member.
            let owner = |gid: Id| layout.groups.iter().position(|g| g.id == gid);
            let (index, highlight) = match hit {
                Some((row, RowKind::Group { index, .. }, true)) => {
                    (index, Some((row, "drop-before")))
                }
                Some((_, RowKind::Group { id: gid, .. }, false))
                | Some((
                    _,
                    RowKind::Host {
                        group: Some(gid), ..
                    },
                    _,
                )) => {
                    let index = owner(gid)?;
                    let last = rows.iter().rposition(|kind| match *kind {
                        RowKind::Group { id, .. } => id == gid,
                        RowKind::Host { group, .. } => group == Some(gid),
                        RowKind::UngroupedHeader => false,
                    })?;
                    (index + 1, Some((last, "drop-after")))
                }
                _ => {
                    let header = rows
                        .iter()
                        .position(|kind| *kind == RowKind::UngroupedHeader);
                    (groups, header.map(|row| (row, "drop-before")))
                }
            };
            Some(Drop {
                apply: Move::Group { id, index },
                highlight,
            })
        }
    }
}

fn clear_highlight(app: &App) {
    let mut index = 0;
    while let Some(row) = app.list.row_at_index(index) {
        for class in ["drop-before", "drop-after", "drop-into"] {
            row.remove_css_class(class);
        }
        index += 1;
    }
}

fn dragged_of(target: &gtk::DropTarget) -> Option<Dragged> {
    // Only drags started in this app carry a local Gdk.Drag.
    let drop = target.current_drop()?;
    drop.drag()?;
    let value = target.value()?;
    Dragged::decode(&value.get::<String>().ok()?)
}

pub fn setup(app: &Rc<App>) {
    let list = &app.list;

    // Drag and drop: manual order, moving between groups, reordering groups.
    let target = gtk::DropTarget::new(glib::Type::STRING, gdk::DragAction::MOVE);
    target.set_preload(true);
    target.connect_motion(glib::clone!(
        #[strong]
        app,
        move |target, _x, y| {
            clear_highlight(&app);
            let Some(dragged) = dragged_of(target) else {
                return gdk::DragAction::empty();
            };
            if let Some(Drop {
                highlight: Some((row, class)),
                ..
            }) = drop_at(&app, dragged, y)
                && let Some(row) = app.list.row_at_index(row as i32)
            {
                row.add_css_class(class);
            }
            gdk::DragAction::MOVE
        }
    ));
    target.connect_leave(glib::clone!(
        #[strong]
        app,
        move |_| clear_highlight(&app)
    ));
    target.connect_drop(glib::clone!(
        #[strong]
        app,
        move |_, value, _x, y| {
            clear_highlight(&app);
            let Some(dragged) = value
                .get::<String>()
                .ok()
                .and_then(|text| Dragged::decode(&text))
            else {
                return false;
            };
            let Some(drop) = drop_at(&app, dragged, y) else {
                return false;
            };
            // Rebuilding destroys the dragged row; let the drag finish first.
            let app = app.clone();
            glib::idle_add_local_once(move || {
                app.commit(|layout| match drop.apply {
                    Move::Host {
                        alias,
                        group,
                        index,
                    } => layout.move_host(&alias, group, index),
                    Move::Group { id, index } => layout.move_group(id, index),
                });
            });
            true
        }
    ));
    list.add_controller(target);

    // Double-click / Enter on a connection opens a terminal.
    list.connect_row_activated(glib::clone!(
        #[strong]
        app,
        move |_, row| {
            let kind = app.rows.borrow().get(row.index() as usize).cloned();
            if let Some(RowKind::Host { alias, .. }) = kind {
                terminal::open(&app, &alias);
            }
        }
    ));

    // A single click on a group header folds it.
    let click = gtk::GestureClick::builder()
        .button(gdk::BUTTON_PRIMARY)
        .build();
    click.connect_released(glib::clone!(
        #[strong]
        app,
        move |_, presses, _x, y| {
            if presses != 1 {
                return;
            }
            let Some(row) = app.list.row_at_y(y as i32) else {
                return;
            };
            let kind = app.rows.borrow().get(row.index() as usize).cloned();
            if let Some(RowKind::Group { id, .. }) = kind {
                app.commit(|layout| {
                    if let Some(group) = layout.group_mut(id) {
                        group.collapsed = !group.collapsed;
                    }
                    Ok(())
                });
            }
        }
    ));
    list.add_controller(click);

    // Right-click menus.
    let menu_click = gtk::GestureClick::builder()
        .button(gdk::BUTTON_SECONDARY)
        .build();
    menu_click.connect_pressed(glib::clone!(
        #[strong]
        app,
        move |_, _, x, y| {
            let kind = app
                .list
                .row_at_y(y as i32)
                .and_then(|row| app.rows.borrow().get(row.index() as usize).cloned());
            let items: MenuItems = match kind {
                Some(RowKind::Host { alias, .. }) => {
                    let (a, b) = (alias.clone(), alias);
                    vec![
                        (
                            "Open Terminal",
                            action(&app, move |app| terminal::open(app, &a)),
                        ),
                        ("Open Files", action(&app, move |app| files::open(app, &b))),
                        ("Edit ~/.ssh/config", action(&app, dialogs::edit_ssh_config)),
                    ]
                }
                Some(RowKind::Group { id, .. }) => vec![
                    (
                        "Rename…",
                        action(&app, move |app| dialogs::rename_group(app, id)),
                    ),
                    (
                        "Delete Group",
                        action(&app, move |app| dialogs::delete_group(app, id)),
                    ),
                ],
                _ => vec![
                    ("New Connection", action(&app, dialogs::new_host)),
                    ("New Group", action(&app, dialogs::new_group)),
                ],
            };
            popup_menu(app.list.upcast_ref(), x, y, items);
        }
    ));
    list.add_controller(menu_click);
}

fn action(app: &Rc<App>, f: impl Fn(&Rc<App>) + 'static) -> Box<dyn Fn()> {
    let app = app.clone();
    Box::new(move || f(&app))
}

/// Menu entries: label and what clicking it does.
pub type MenuItems<'a> = Vec<(&'a str, Box<dyn Fn()>)>;

/// A small popover of plain menu entries at (x, y) of `parent`.
pub fn popup_menu(parent: &gtk::Widget, x: f64, y: f64, items: MenuItems) {
    let popover = gtk::Popover::new();
    popover.set_has_arrow(false);
    popover.set_halign(gtk::Align::Start);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    for (label, run) in items {
        let button = gtk::Button::builder()
            .child(&gtk::Label::builder().label(label).xalign(0.0).build())
            .css_classes(["flat"])
            .build();
        button.connect_clicked(glib::clone!(
            #[weak]
            popover,
            move |_| {
                popover.popdown();
                run();
            }
        ));
        content.append(&button);
    }
    popover.set_child(Some(&content));
    popover.set_parent(parent);
    popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    popover.connect_closed(|popover| {
        let popover = popover.clone();
        glib::idle_add_local_once(move || popover.unparent());
    });
    popover.popup();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_payload_round_trips_and_rejects_junk() {
        for dragged in [Dragged::Host("web:2".into()), Dragged::Group(3)] {
            assert_eq!(Dragged::decode(&dragged.encode()), Some(dragged));
        }
        assert_eq!(Dragged::decode("file:///etc/passwd"), None);
        assert_eq!(Dragged::decode("group:x"), None);
        assert_eq!(Dragged::decode("host:"), None);
    }
}
