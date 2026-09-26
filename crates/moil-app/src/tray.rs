//! The tray icon and its menu, made real from a [`TrayView`].

use std::collections::HashMap;

use tray_icon::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu, TextStyle,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::icon::{self, TRAY_SIZE};
use crate::platform;
use crate::view::{Action, Entry, IconState, Label, MenuModel, TrayView};

/// The tray icon, showing the latest [`TrayView`].
pub struct Tray {
    icon: TrayIcon,
    menu: RealMenu,
    shown: TrayView,
}

impl std::fmt::Debug for Tray {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tray")
            .field("shown", &self.shown)
            .finish_non_exhaustive()
    }
}

impl Tray {
    /// Puts the icon in the menu bar. Must be called on the main thread,
    /// once the event loop runs.
    pub fn new(view: TrayView) -> Result<Self, String> {
        let menu = RealMenu::new(&view.menu);
        let icon = TrayIconBuilder::new()
            .with_icon(tray_image(view.icon)?)
            .with_icon_as_template(true)
            .with_tooltip(&view.tooltip)
            .with_menu(Box::new(menu.menu.clone()))
            .build()
            .map_err(|e| format!("couldn't create the tray icon: {e}"))?;
        platform::use_tabular_digits(&icon);
        icon.set_title(view.title.as_deref());
        Ok(Self {
            icon,
            menu,
            shown: view,
        })
    }

    /// Shows `view`, changing only what differs from what is shown.
    pub fn show(&mut self, view: TrayView) {
        if view.icon != self.shown.icon {
            match tray_image(view.icon) {
                Ok(image) => {
                    let _ = self.icon.set_icon_with_as_template(Some(image), true);
                }
                Err(e) => tracing::warn!("{e}"),
            }
        }
        if view.title != self.shown.title {
            self.icon.set_title(view.title.as_deref());
        }
        if view.tooltip != self.shown.tooltip {
            let _ = self.icon.set_tooltip(Some(&view.tooltip));
        }
        if view.menu != self.shown.menu {
            if self.shown.menu.same_shape(&view.menu) {
                // Relabel in place, so an open menu stays open as a job progresses.
                self.menu.relabel(&self.shown.menu, &view.menu);
            } else {
                self.menu = RealMenu::new(&view.menu);
                self.icon.set_menu(Some(Box::new(self.menu.menu.clone())));
            }
        }
        self.shown = view;
    }

    /// What the menu item `id` does.
    pub fn action(&self, id: &MenuId) -> Option<&Action> {
        self.menu.actions.get(id)
    }
}

fn tray_image(state: IconState) -> Result<Icon, String> {
    let image = icon::draw(state, TRAY_SIZE);
    Icon::from_rgba(image.rgba, image.width, image.height)
        .map_err(|e| format!("couldn't draw the tray icon: {e}"))
}

/// A muda menu built from a [`MenuModel`], with its items in the order of
/// [`MenuModel::flatten`].
struct RealMenu {
    menu: Menu,
    items: Vec<RealItem>,
    actions: HashMap<MenuId, Action>,
}

enum RealItem {
    Text(MenuItem),
    Item(MenuItem),
    Check(CheckMenuItem),
    Submenu(Submenu),
    Separator,
}

impl RealMenu {
    fn new(model: &MenuModel) -> Self {
        let mut real = Self {
            menu: Menu::new(),
            items: Vec::new(),
            actions: HashMap::new(),
        };
        let menu = real.menu.clone();
        real.add(&model.entries, &|item| {
            let _ = menu.append(item);
        });
        real
    }

    fn add(&mut self, entries: &[Entry], append: &dyn Fn(&dyn IsMenuItem)) {
        for entry in entries {
            match entry {
                Entry::Text(label) => {
                    let item = MenuItem::new("", false, None);
                    set_label(&item, label);
                    append(&item);
                    self.items.push(RealItem::Text(item));
                }
                Entry::Item {
                    label,
                    action,
                    enabled,
                } => {
                    let item = MenuItem::new("", *enabled, None);
                    set_label(&item, label);
                    append(&item);
                    self.actions.insert(item.id().clone(), action.clone());
                    self.items.push(RealItem::Item(item));
                }
                Entry::Check {
                    label,
                    checked,
                    enabled,
                    action,
                } => {
                    let item = CheckMenuItem::new("", *enabled, *checked, None);
                    item.set_styled_text(parts(label));
                    append(&item);
                    self.actions.insert(item.id().clone(), action.clone());
                    self.items.push(RealItem::Check(item));
                }
                Entry::Submenu { label, entries } => {
                    let submenu = Submenu::new("", true);
                    submenu.set_styled_text(parts(label));
                    append(&submenu);
                    self.items.push(RealItem::Submenu(submenu.clone()));
                    self.add(entries, &|item| {
                        let _ = submenu.append(item);
                    });
                }
                Entry::Separator => {
                    append(&PredefinedMenuItem::separator());
                    self.items.push(RealItem::Separator);
                }
            }
        }
    }

    /// Turns the menu built for `old` into `new`, which has the same shape.
    fn relabel(&self, old: &MenuModel, new: &MenuModel) {
        for ((item, old), new) in self.items.iter().zip(old.flatten()).zip(new.flatten()) {
            if old == new {
                continue;
            }
            match (item, new) {
                (RealItem::Text(item), Entry::Text(label)) => set_label(item, label),
                (RealItem::Item(item), Entry::Item { label, enabled, .. }) => {
                    set_label(item, label);
                    item.set_enabled(*enabled);
                }
                (
                    RealItem::Check(item),
                    Entry::Check {
                        label,
                        checked,
                        enabled,
                        ..
                    },
                ) => {
                    item.set_styled_text(parts(label));
                    item.set_checked(*checked);
                    item.set_enabled(*enabled);
                }
                (RealItem::Submenu(submenu), Entry::Submenu { label, .. }) => {
                    submenu.set_styled_text(parts(label));
                }
                _ => {}
            }
        }
    }
}

fn set_label(item: &MenuItem, label: &Label) {
    item.set_styled_text(parts(label));
}

/// A label as muda's styled parts: the text, then the detail de-emphasized.
fn parts(label: &Label) -> Vec<(String, TextStyle)> {
    match (&*label.text, &label.detail) {
        (text, None) => vec![(text.to_owned(), TextStyle::Default)],
        ("", Some(detail)) => vec![(detail.clone(), TextStyle::Secondary)],
        (text, Some(detail)) => vec![
            (text.to_owned(), TextStyle::Default),
            (format!(" — {detail}"), TextStyle::Secondary),
        ],
    }
}
