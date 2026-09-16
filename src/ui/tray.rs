//! The notification-area icon and its menu.
//!
//! This is the whole user interface for normal use: FxMini has no main window,
//! and the only reason a window ever exists is the tuning panel
//! ([`super::panel`]).
//!
//! ## Message pump requirement
//!
//! `tray-icon` and `muda` deliver events through Win32 messages, so the thread
//! that created the icon must be pumping. [`super::run_message_loop`] does
//! that, and also drains the menu event queue this module reads.
//!
//! ## Why both driver items always exist
//!
//! Showing only the applicable one means removing and re-adding menu entries
//! whenever the driver state changes, which invalidates ids and races with the
//! event queue. Instead both are always present and one is disabled — the same
//! thing the Sound control panel does.
//!
//! ## Language
//!
//! Every label comes from [`crate::i18n`] and is a single language, not the
//! bilingual `启用音效 / Enabled` this used to show. A menu is scanned from the
//! screen edge, where a wide label is clipped rather than wrapped, so doubling
//! its length to serve a reader who only needs half of it was costing the one
//! thing the menu is short of. Changing the language rebuilds the whole tray
//! (`app::App::set_language`) rather than mutating labels in place: `muda` has
//! no text setter for an existing item, so a rebuild is the only option.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{TrayIcon, TrayIconBuilder};

use crate::i18n::{self, Lang};
use crate::preset::PresetEntry;

/// Something the user asked for through the tray.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayAction {
    /// Enable or disable processing.
    ToggleEnabled,
    /// Enable or disable running at logon.
    ToggleAutostart,
    /// Switch to a `.fac` file.
    SelectPreset(PathBuf),
    /// Open the tuning panel.
    OpenPanel,
    /// Install the virtual sound card (triggers an elevation prompt).
    InstallDriver,
    /// Remove the virtual sound card.
    RemoveDriver,
    /// Re-scan the preset folder.
    ReloadPresets,
    /// Point the system's default output at the virtual sound card, so audio
    /// actually flows through the enhancer.
    RouteOutput,
    /// Draw the interface in a different language.
    SetLanguage(Lang),
    /// Leave.
    Quit,
}

/// A preset's menu entry, kept so it can be ticked later.
struct PresetMenuItem {
    id: MenuId,
    path: PathBuf,
    item: CheckMenuItem,
}

/// Owns the tray icon and its menu.
pub struct Tray {
    icon: TrayIcon,
    enabled_item: CheckMenuItem,
    route_item: MenuItem,
    autostart_item: CheckMenuItem,
    install_item: MenuItem,
    remove_item: MenuItem,
    preset_menu: Submenu,
    preset_items: Vec<PresetMenuItem>,
    /// Menu id -> action, for the fixed entries.
    actions: HashMap<MenuId, TrayAction>,
}

impl Tray {
    /// Builds the icon and menu, in the language [`i18n::current`] reports.
    pub fn new(
        presets: &[PresetEntry],
        enabled: bool,
        autostart: bool,
        driver_present: bool,
    ) -> Result<Self, String> {
        let text = &i18n::t().tray;
        let menu = Menu::new();

        let enabled_item = CheckMenuItem::new(text.enabled, true, enabled, None);
        let route_item = MenuItem::new(text.route_through, true, None);
        let open_item = MenuItem::new(text.panel, true, None);
        let preset_menu = Submenu::new(text.presets, true);
        let autostart_item = CheckMenuItem::new(text.autostart, true, autostart, None);
        let install_item = MenuItem::new(text.install_driver, true, None);
        let remove_item = MenuItem::new(text.remove_driver, true, None);
        let reload_item = MenuItem::new(text.rescan, true, None);
        let quit_item = MenuItem::new(text.quit, true, None);

        // The language submenu. Its entries are endonyms — "中文" and
        // "English" — so the way out of a language you cannot read is legible
        // in that language.
        let language_menu = Submenu::new(text.language, true);
        let current = i18n::current();
        let mut language_items = Vec::with_capacity(Lang::ALL.len());
        for lang in Lang::ALL {
            let item = CheckMenuItem::new(lang.endonym(), true, lang == current, None);
            if language_menu.append(&item).is_ok() {
                language_items.push((item.id().clone(), lang));
            }
        }

        let mut actions: HashMap<MenuId, TrayAction> = [
            (enabled_item.id().clone(), TrayAction::ToggleEnabled),
            (route_item.id().clone(), TrayAction::RouteOutput),
            (open_item.id().clone(), TrayAction::OpenPanel),
            (autostart_item.id().clone(), TrayAction::ToggleAutostart),
            (install_item.id().clone(), TrayAction::InstallDriver),
            (remove_item.id().clone(), TrayAction::RemoveDriver),
            (reload_item.id().clone(), TrayAction::ReloadPresets),
            (quit_item.id().clone(), TrayAction::Quit),
        ]
        .into_iter()
        .collect();

        for (id, lang) in language_items {
            actions.insert(id, TrayAction::SetLanguage(lang));
        }

        menu.append(&enabled_item).map_err(stringify)?;
        menu.append(&route_item).map_err(stringify)?;
        menu.append(&open_item).map_err(stringify)?;
        menu.append(&preset_menu).map_err(stringify)?;
        menu.append(&PredefinedMenuItem::separator())
            .map_err(stringify)?;
        menu.append(&autostart_item).map_err(stringify)?;
        menu.append(&language_menu).map_err(stringify)?;
        menu.append(&PredefinedMenuItem::separator())
            .map_err(stringify)?;
        menu.append(&install_item).map_err(stringify)?;
        menu.append(&remove_item).map_err(stringify)?;
        menu.append(&reload_item).map_err(stringify)?;
        menu.append(&PredefinedMenuItem::separator())
            .map_err(stringify)?;
        menu.append(&quit_item).map_err(stringify)?;

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("FxMini")
            .with_icon(super::icon::tray_icon())
            .build()
            .map_err(stringify)?;

        let mut tray = Self {
            icon,
            enabled_item,
            route_item,
            autostart_item,
            install_item,
            remove_item,
            preset_menu,
            preset_items: Vec::new(),
            actions,
        };
        tray.set_presets(presets);
        tray.set_driver_present(driver_present);
        Ok(tray)
    }

    /// Replaces the preset submenu contents.
    ///
    /// `Submenu` has no bulk clear, so children are removed one at a time.
    /// Removal takes the item itself rather than its id, which is why the
    /// handles are retained.
    pub fn set_presets(&mut self, presets: &[PresetEntry]) {
        for existing in self.preset_items.drain(..) {
            let _ = self.preset_menu.remove(&existing.item);
        }

        if presets.is_empty() {
            let empty = MenuItem::new(i18n::t().tray.no_presets, false, None);
            let _ = self.preset_menu.append(&empty);
            return;
        }

        for entry in presets {
            // The display name comes from inside the file; fall back to the
            // filename when a preset has an empty name line.
            let label = if entry.name.trim().is_empty() {
                entry
                    .path
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "?".to_owned())
            } else {
                entry.name.clone()
            };

            let item = CheckMenuItem::new(&label, true, false, None);
            let id = item.id().clone();
            if self.preset_menu.append(&item).is_ok() {
                self.preset_items.push(PresetMenuItem {
                    id,
                    path: entry.path.clone(),
                    item,
                });
            }
        }
    }

    /// Ticks the given preset and unticks the rest.
    pub fn set_active_preset(&self, path: Option<&Path>) {
        for entry in &self.preset_items {
            entry
                .item
                .set_checked(path.is_some_and(|wanted| wanted == entry.path));
        }
    }

    /// Disables whichever driver action does not apply.
    pub fn set_driver_present(&self, present: bool) {
        self.install_item.set_enabled(!present);
        self.remove_item.set_enabled(present);
    }

    /// Greys out "route output through FxMini" once audio is already going
    /// through it.
    ///
    /// The entry is a one-shot repair, not a toggle: there is no useful
    /// "unroute" (quitting FxMini already does that), so the item is enabled
    /// exactly when pressing it would change something. That doubles as the
    /// answer to "is my audio actually being enhanced?" — visible by
    /// right-clicking the tray instead of reading the log.
    pub fn set_routed(&self, routed: bool) {
        self.route_item.set_enabled(!routed);
    }

    /// Keeps the checkbox in sync when the state changed elsewhere (the panel
    /// can toggle processing too).
    pub fn set_enabled_checked(&self, on: bool) {
        self.enabled_item.set_checked(on);
    }

    pub fn set_autostart_checked(&self, on: bool) {
        self.autostart_item.set_checked(on);
    }

    /// Updates the hover tooltip.
    pub fn set_tooltip(&self, text: &str) {
        let _ = self.icon.set_tooltip(Some(text));
    }

    /// Reads one pending menu event.
    ///
    /// Non-blocking: the caller is a message loop that has to stay responsive.
    /// Events for ids we do not know are discarded rather than forwarded, so a
    /// stale queue entry cannot trigger a surprise action.
    pub fn poll(&self) -> Option<TrayAction> {
        let receiver = MenuEvent::receiver();
        while let Ok(event) = receiver.try_recv() {
            if let Some(action) = self.actions.get(&event.id) {
                return Some(action.clone());
            }
            if let Some(entry) = self
                .preset_items
                .iter()
                .find(|entry| entry.id == event.id)
            {
                return Some(TrayAction::SelectPreset(entry.path.clone()));
            }
        }
        None
    }
}

/// Flattens a `tray_icon` error into a `String` for the caller's log.
fn stringify(error: impl std::fmt::Display) -> String {
    error.to_string()
}
