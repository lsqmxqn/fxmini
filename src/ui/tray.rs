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
//! that, and also drains the menu and icon event queues this module reads.
//!
//! ## How the menu is grouped
//!
//! Four groups, in the order a user's questions arrive:
//!
//! 1. **Open the panel.** The one thing people come here for. It is also on
//!    left-click — see [`Tray::poll_icon`] — so the menu entry is for
//!    discoverability rather than for the only route in.
//! 2. **What it is doing.** Processing on/off, whether the output is routed,
//!    and the preset list.
//! 3. **Settings.** Start with Windows, interface language.
//! 4. **Maintenance.** Driver install/remove, then quit.
//!
//! `Rescan presets` lives *inside* the presets submenu, where it belongs: it
//! used to sit between the driver entries, six rows below the list it refreshes.
//!
//! ## Why the route entry states its state instead of greying out
//!
//! Greying out "Route output via FxMini" once it had been done made "already
//! correct" look identical to "not available". The item now reads
//! `Output routed via FxMini` with a tick when it has been done, so the
//! disabled state answers the question the user opened the menu to ask.
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
//! thing the menu is short of.
//!
//! Switching language retitles the existing items in place. It used to rebuild
//! the whole tray, on the belief that `muda` had no setter for an item's text;
//! it does ([`MenuItem::set_text`]), and a rebuild is a strictly worse way to
//! change a word — it can fail, and the failure mode is losing the only UI the
//! process has.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

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

    open_item: MenuItem,
    enabled_item: CheckMenuItem,
    route_item: CheckMenuItem,
    preset_menu: Submenu,
    preset_items: Vec<PresetMenuItem>,
    /// The disabled `（无预设）` placeholder, present only while the list is
    /// empty. Kept so a language switch can retitle it.
    preset_empty: Option<MenuItem>,
    rescan_item: MenuItem,
    autostart_item: CheckMenuItem,
    language_menu: Submenu,
    install_item: MenuItem,
    remove_item: MenuItem,
    quit_item: MenuItem,

    /// Menu id -> action, for the fixed entries.
    actions: HashMap<MenuId, TrayAction>,

    /// The two facts [`Self::refresh_route`] needs, kept because the route
    /// entry's label and enabled state depend on both and they arrive from
    /// different places.
    routed: Cell<bool>,
    driver_present: Cell<bool>,
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

        let open_item = MenuItem::new(text.panel, true, None);
        let enabled_item = CheckMenuItem::new(text.enabled, true, enabled, None);
        let route_item = CheckMenuItem::new(text.route_through, true, false, None);
        let preset_menu = Submenu::new(text.presets, true);
        let rescan_item = MenuItem::new(text.rescan, true, None);
        let autostart_item = CheckMenuItem::new(text.autostart, true, autostart, None);
        let install_item = MenuItem::new(text.install_driver, true, None);
        let remove_item = MenuItem::new(text.remove_driver, true, None);
        let quit_item = MenuItem::new(text.quit, true, None);

        // The language submenu. Its entries are endonyms — "中文" and
        // "English" — so the way out of a language you cannot read is legible
        // in that language. They are also the one pair of labels that never
        // needs retitling, which is why they are not stored.
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
            (open_item.id().clone(), TrayAction::OpenPanel),
            (enabled_item.id().clone(), TrayAction::ToggleEnabled),
            (route_item.id().clone(), TrayAction::RouteOutput),
            (rescan_item.id().clone(), TrayAction::ReloadPresets),
            (autostart_item.id().clone(), TrayAction::ToggleAutostart),
            (install_item.id().clone(), TrayAction::InstallDriver),
            (remove_item.id().clone(), TrayAction::RemoveDriver),
            (quit_item.id().clone(), TrayAction::Quit),
        ]
        .into_iter()
        .collect();

        for (id, lang) in language_items {
            actions.insert(id, TrayAction::SetLanguage(lang));
        }

        let separator = || PredefinedMenuItem::separator();

        // Group 1: the panel.
        menu.append(&open_item).map_err(stringify)?;
        menu.append(&separator()).map_err(stringify)?;
        // Group 2: state.
        menu.append(&enabled_item).map_err(stringify)?;
        menu.append(&route_item).map_err(stringify)?;
        menu.append(&preset_menu).map_err(stringify)?;
        menu.append(&separator()).map_err(stringify)?;
        // Group 3: settings.
        menu.append(&autostart_item).map_err(stringify)?;
        menu.append(&language_menu).map_err(stringify)?;
        menu.append(&separator()).map_err(stringify)?;
        // Group 4: maintenance, then the exit.
        menu.append(&install_item).map_err(stringify)?;
        menu.append(&remove_item).map_err(stringify)?;
        menu.append(&separator()).map_err(stringify)?;
        menu.append(&quit_item).map_err(stringify)?;

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("FxMini")
            .with_icon(super::icon::tray_icon())
            // The menu is on right-click only, freeing left-click for the
            // panel. On Windows the two are independent, so this does not cost
            // the menu.
            .with_menu_on_left_click(false)
            .build()
            .map_err(stringify)?;

        let mut tray = Self {
            icon,
            open_item,
            enabled_item,
            route_item,
            preset_menu,
            preset_items: Vec::new(),
            preset_empty: None,
            rescan_item,
            autostart_item,
            language_menu,
            install_item,
            remove_item,
            quit_item,
            actions,
            routed: Cell::new(false),
            driver_present: Cell::new(driver_present),
        };
        tray.set_presets(presets);
        tray.set_driver_present(driver_present);
        Ok(tray)
    }

    /// Replaces the preset submenu contents.
    ///
    /// The submenu is emptied and rebuilt rather than reconciled: `Submenu` has
    /// no bulk clear and removal takes the item rather than an index, so
    /// tracking which of four kinds of child is stale costs more than rebuilding
    /// a list that is at most a few dozen rows.
    pub fn set_presets(&mut self, presets: &[PresetEntry]) {
        let existing = self.preset_menu.items().len();
        for _ in 0..existing {
            self.preset_menu.remove_at(0);
        }
        self.preset_items.clear();
        self.preset_empty = None;

        if presets.is_empty() {
            let empty = MenuItem::new(i18n::t().tray.no_presets, false, None);
            if self.preset_menu.append(&empty).is_ok() {
                self.preset_empty = Some(empty);
            }
        } else {
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

        // Rescanning is a preset operation, so it lives with the presets —
        // pinned below a separator so it does not read as one of them.
        let _ = self.preset_menu.append(&PredefinedMenuItem::separator());
        let _ = self.preset_menu.append(&self.rescan_item);
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
        self.driver_present.set(present);
        self.install_item.set_enabled(!present);
        self.remove_item.set_enabled(present);
        // The route entry is only meaningful while there is something to route
        // to, so it follows this too.
        self.refresh_route();
    }

    /// States whether audio is currently going through the enhancer.
    ///
    /// Not a toggle: there is no useful "unroute" (quitting FxMini already does
    /// that), so the entry describes the situation and is pressable exactly when
    /// pressing it would change something.
    pub fn set_routed(&self, routed: bool) {
        self.routed.set(routed);
        self.refresh_route();
    }

    /// Keeps the checkbox in sync when the state changed elsewhere (the panel
    /// can toggle processing too).
    pub fn set_enabled_checked(&self, on: bool) {
        self.enabled_item.set_checked(on);
    }

    pub fn set_autostart_checked(&self, on: bool) {
        self.autostart_item.set_checked(on);
    }

    /// Redraws the route entry from the two facts it depends on.
    fn refresh_route(&self) {
        let text = &i18n::t().tray;
        if self.routed.get() {
            // The tick plus the wording carry the meaning; the item is disabled
            // because there is nothing left to press, not because it is
            // unavailable.
            self.route_item.set_text(text.route_through_done);
            self.route_item.set_checked(true);
            self.route_item.set_enabled(false);
        } else {
            self.route_item.set_text(text.route_through);
            self.route_item.set_checked(false);
            self.route_item.set_enabled(self.driver_present.get());
        }
    }

    /// Rewrites every label from the current language table, in place.
    ///
    /// Preset names come from the files rather than the table, so they are left
    /// alone; only the empty-list placeholder needs a new word.
    pub fn retitle(&self) {
        let text = &i18n::t().tray;
        self.open_item.set_text(text.panel);
        self.enabled_item.set_text(text.enabled);
        self.preset_menu.set_text(text.presets);
        self.rescan_item.set_text(text.rescan);
        self.autostart_item.set_text(text.autostart);
        self.language_menu.set_text(text.language);
        self.install_item.set_text(text.install_driver);
        self.remove_item.set_text(text.remove_driver);
        self.quit_item.set_text(text.quit);
        if let Some(empty) = &self.preset_empty {
            empty.set_text(text.no_presets);
        }
        // The route entry's wording is state-dependent, so it is not simply
        // assigned here.
        self.refresh_route();
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

    /// Reads the icon's own mouse events, collapsing them to one request.
    ///
    /// Left-click opens the panel, which is the Windows convention for a tray
    /// app's primary action and turns an icon with no window into a one-click
    /// way into the tuning panel. The menu is on right-click and is unaffected —
    /// `tray-icon` gates the two separately.
    ///
    /// The whole queue is drained before deciding: a double-click arrives as two
    /// clicks plus a double-click, and a request per event would try to open the
    /// panel three times. Wanting it any number of times is wanting it once.
    pub fn poll_icon(&self) -> Option<TrayAction> {
        let receiver = TrayIconEvent::receiver();
        let mut open = false;

        while let Ok(event) = receiver.try_recv() {
            match event {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
                | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } => open = true,
                // Everything else — moves, hovers, right-clicks that are about
                // to open the menu — is not ours to act on.
                _ => {}
            }
        }

        open.then_some(TrayAction::OpenPanel)
    }
}

/// Flattens a `tray_icon` error into a `String` for the caller's log.
fn stringify(error: impl std::fmt::Display) -> String {
    error.to_string()
}
