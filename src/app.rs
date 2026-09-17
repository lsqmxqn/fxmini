//! Application wiring: the tray, the engine, and the state that connects them.
//!
//! Keeping this separate from `main` means the event-loop callback stays a
//! one-liner and the behaviour is testable without a message pump.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::device;
use crate::driver;
use crate::engine::{AudioEngine, EngineHandle, EngineStatus, SharedParams};
use crate::i18n::{self, Lang};
use crate::preset::{self, PresetEntry};
use crate::routing::{EngageOutcome, Routing};
use crate::ui::panel::{self, PanelShared};
use crate::ui::tray::{Tray, TrayAction};

/// How often the tooltip and the driver menu are refreshed.
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// The running application.
pub struct App {
    config: Config,
    engine: AudioEngine,
    handle: EngineHandle,
    tray: Tray,
    presets: Vec<PresetEntry>,
    active_preset: Option<PathBuf>,
    /// Owns the difference between the machine's output routing and the one
    /// FxMini needs. Releasing it on exit is what stops the app from leaving a
    /// silent machine behind.
    routing: Routing,
    /// Whether a takeover has already been attempted for the virtual card's
    /// current appearance. Cleared when the card disappears, so its return is a
    /// fresh reason to try.
    routing_attempted: bool,
    last_refresh: Instant,
    /// The preset-folder revision this app has already reacted to.
    ///
    /// The panel saves presets by asking the engine to write the file, which
    /// happens on the audio thread, so the tray cannot learn about it from its
    /// own call paths. It compares this against
    /// [`EngineStatus::presets_revision`] once a second instead.
    seen_presets_revision: u64,
}

impl App {
    /// Prepares everything and puts the tray icon on screen.
    pub fn start(mut config: Config) -> Result<Self, String> {
        // Before the tray is built, because every one of its labels is drawn
        // from the string table and there is no way to relabel an existing
        // menu item — see `ui::tray`.
        let language = i18n::resolve(&config.language);
        i18n::set(language);
        log::info!(
            "interface language: {} ({})",
            language.endonym(),
            config.language
        );

        // Bundled presets are unpacked before anything reads the folder, so the
        // first run already has a populated menu.
        if let Err(err) = preset::unpack_embedded_presets() {
            log::warn!("could not unpack the bundled presets: {err}");
        }

        let presets = preset::library();
        log::info!(
            "{} preset(s) available ({} embedded)",
            presets.len(),
            preset::embedded_count()
        );

        // Pick the preset to start with: the remembered one, else Music if it
        // survived, else whatever sorted first.
        let active = config
            .active_preset
            .as_ref()
            .map(PathBuf::from)
            .filter(|path| path.is_file())
            .or_else(|| {
                presets
                    .iter()
                    .find(|entry| entry.name.contains("音乐") || entry.name.contains("Music"))
                    .map(|entry| entry.path.clone())
            })
            .or_else(|| presets.first().map(|entry| entry.path.clone()));

        // Routing comes before the engine, in that order, because the engine's
        // choice of render endpoint depends on knowing which physical device
        // the takeover displaced. Doing it the other way round makes the first
        // graph render to an arbitrary device and then rebuild.
        let mut routing = Routing::new();
        Routing::recover(config.previous_default_id.as_deref());
        if config.take_over_default {
            match routing.engage() {
                EngageOutcome::Engaged { .. } | EngageOutcome::AlreadyRouted => {}
                EngageOutcome::NoVirtualCard => log::info!(
                    "not switching the default output: FxSound's virtual sound card is not active"
                ),
                EngageOutcome::Failed(err) => {
                    log::error!("could not route audio through FxMini: {err}");
                }
            }
        } else {
            log::info!("take_over_default is off; leaving the system output alone");
        }
        // Re-recorded every start so a crash always leaves a usable marker, and
        // blanked when there is nothing to restore.
        config.previous_default_id = routing.previous().map(str::to_owned);
        // Written out here rather than left to the next unrelated save.
        // This file is the *only* record of where the output came from, so it
        // has to hit the disk before the process can be killed while holding
        // the default device — which is exactly what a crash is.
        if let Err(err) = config.save() {
            log::warn!("could not save the crash-recovery marker: {err}");
        }

        let engine = match AudioEngine::start(config.clone()) {
            Ok(engine) => engine,
            Err(err) => {
                routing.release();
                return Err(format!("could not start the audio engine: {err}"));
            }
        };
        let handle = engine.handle();

        // Push the remembered on/off state before anything reads it.
        handle.params().set_enabled(config.enabled);

        let driver_status = driver::status();
        log::info!("driver: {}", driver_status.summary());
        if !driver_status.endpoint_active {
            log::warn!(
                "FxSound's virtual sound card was not found. FxMini will pass audio through \
                 unprocessed until it is installed (tray menu: install virtual sound card)."
            );
        }

        let tray = match Tray::new(
            &presets,
            config.enabled,
            crate::autostart::is_enabled(),
            driver_status.endpoint_active,
        ) {
            Ok(tray) => tray,
            Err(err) => {
                routing.release();
                return Err(err);
            }
        };

        let mut app = Self {
            engine,
            handle,
            tray,
            presets,
            active_preset: None,
            routing,
            routing_attempted: config.take_over_default,
            last_refresh: Instant::now() - REFRESH_INTERVAL,
            seen_presets_revision: 0,
            config,
        };

        if let Some(path) = active {
            app.activate_preset(&path);
        }
        app.refresh();

        Ok(app)
    }

    /// Runs one pass of the event loop. Returns `true` when the app should exit.
    pub fn tick(&mut self) -> bool {
        if self.last_refresh.elapsed() >= REFRESH_INTERVAL {
            self.refresh();
        }

        // The menu is asked first, then the icon's own clicks. A right-click
        // emits a click event as well as opening the menu, and the menu event is
        // the one carrying meaning; asking in this order means a stray click
        // event cannot shadow a menu choice.
        let action = self.tray.poll().or_else(|| self.tray.poll_icon());

        match action {
            Some(TrayAction::Quit) => return true,
            Some(TrayAction::ToggleEnabled) => self.toggle_enabled(),
            Some(TrayAction::ToggleAutostart) => self.toggle_autostart(),
            Some(TrayAction::SelectPreset(path)) => self.activate_preset(&path),
            Some(TrayAction::OpenPanel) => self.open_panel(),
            Some(TrayAction::InstallDriver) => self.request_driver_install(),
            Some(TrayAction::RemoveDriver) => self.request_driver_removal(),
            Some(TrayAction::ReloadPresets) => self.reload_presets(),
            Some(TrayAction::RouteOutput) => self.route_output(),
            Some(TrayAction::SetLanguage(language)) => self.set_language(language),
            None => {}
        }
        false
    }

    /// Shuts the engine down cleanly and hands the output device back.
    ///
    /// Order matters. The engine stops first, so the moment the default output
    /// returns to the physical card there is no second copy of the audio still
    /// rendering into it — restoring first would put the unenhanced stream and
    /// the enhanced one on the same speaker for a moment, which is audible as
    /// an echo.
    pub fn shutdown(mut self) {
        self.engine.shutdown();

        if self.routing.release() {
            // Cleared only after a successful restore, so the marker keeps
            // pointing at the displaced device for as long as one is owed.
            self.config.previous_default_id = None;
        }
        if let Err(err) = self.config.save() {
            log::warn!("could not save the config: {err}");
        }
    }

    /// Refreshes the tooltip and the driver-dependent menu state.
    ///
    /// Polled rather than event-driven because the interesting facts
    /// ("is the endpoint there?", "is the card the default?") change through
    /// paths that do not notify us — the user installing FxSound itself, a
    /// driver update, Device Manager actions.
    fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        self.reconcile_selection();

        // An owned handle rather than a borrow: the late takeover below needs
        // `&mut self`, and a reference into `self` would still be live here.
        let status = std::sync::Arc::clone(self.handle.status());

        // A preset was written by the panel (the engine does the writing, so
        // nothing on this thread saw it happen). Rescanning here is what puts
        // the new preset into the tray menu; the panel rescans its own copy off
        // the same counter.
        let revision = status.presets_revision();
        if revision != self.seen_presets_revision {
            self.seen_presets_revision = revision;
            self.reload_presets();
        }

        let driver_present = status.virtual_present() || device::virtual_device_present();
        self.tray.set_driver_present(driver_present);

        // A late takeover. The card can appear after start — the user installs
        // the driver from this very menu, for instance — and until it does,
        // there is nothing to route to.
        //
        // Guarded by `routing_attempted` so a card that is present but unusable
        // (disabled in Device Manager, say) is not retried once a second for
        // the life of the process. The flag is cleared whenever the card goes
        // away, so its reappearance is a fresh reason to try.
        //
        // Attempted only while `engaged` is false, which is also what stops
        // this from fighting a user who deliberately moved the output
        // elsewhere: the switch happens once, and a later change by hand is
        // theirs to keep.
        if !driver_present {
            self.routing_attempted = false;
        } else if self.config.take_over_default
            && self.handle.params().is_enabled()
            && !self.routing.engaged()
            && !self.routing_attempted
        {
            self.routing_attempted = true;
            self.engage_routing();
        }

        // Whether the output is actually flowing through the enhancer. Read
        // from the hardware, so it is also wrong when the user routed audio
        // past FxMini by hand, not just when something failed.
        let routed = crate::routing::default_is_routed_through_card();
        self.tray.set_routed(routed);

        let text = &i18n::t().tray;
        let tooltip = if !driver_present {
            text.tooltip_no_card.to_owned()
        } else if !self.handle.params().is_enabled() {
            text.tooltip_disabled.to_owned()
        } else if status.is_running() {
            // The distinction that matters to a user reporting "nothing
            // changed": audio is being processed, or the enhancer is being
            // bypassed by the system's own routing.
            let route_lost = self.config.take_over_default && !routed;
            text.tooltip_playing(
                self.active_name().as_deref(),
                status.last_error().as_deref(),
                route_lost,
            )
        } else {
            text.tooltip_idle.to_owned()
        };
        self.tray.set_tooltip(&tooltip);
    }

    /// Copies selection state that the panel may have changed into the config
    /// file and the tray menu.
    ///
    /// The panel writes straight to the shared atomics — that is what keeps
    /// slider drags free of message passing — so anything the tray *displays*
    /// has to be reconciled here instead of assumed to be in sync. Polled from
    /// [`Self::refresh`], which [`Self::tick`] runs once a second.
    ///
    /// Tray-originated changes never reach the bodies below: the tray handlers
    /// update both sides, so there is nothing left to reconcile and the log
    /// lines stay truthful about where the change came from.
    fn reconcile_selection(&mut self) {
        let enabled = self.handle.params().is_enabled();
        if enabled != self.config.enabled {
            self.config.enabled = enabled;
            self.tray.set_enabled_checked(enabled);
            log::info!(
                "processing {} (changed in the panel)",
                if enabled { "enabled" } else { "disabled" }
            );
            self.save_config();
        }

        let selected = self.handle.status().active_preset().map(PathBuf::from);
        if self.active_preset != selected {
            self.active_preset = selected;
            self.tray.set_active_preset(self.active_preset.as_deref());
            self.config.active_preset = self
                .active_preset
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned());
            log::info!(
                "preset changed in the panel: {}",
                self.config.active_preset.as_deref().unwrap_or("none")
            );
            self.save_config();
        }
    }

    /// Persists the config, warning rather than failing when the file is not
    /// writable.
    fn save_config(&mut self) {
        if let Err(err) = self.config.save() {
            log::warn!("could not save the config: {err}");
        }
    }

    /// The active preset's display name.
    fn active_name(&self) -> Option<String> {
        let active = self.active_preset.as_ref()?;
        self.presets
            .iter()
            .find(|entry| &entry.path == active)
            .map(|entry| entry.name.clone())
    }

    fn toggle_enabled(&mut self) {
        self.config.enabled = !self.config.enabled;
        self.handle.params().set_enabled(self.config.enabled);
        self.tray.set_enabled_checked(self.config.enabled);

        log::info!("processing {}", if self.config.enabled { "enabled" } else { "disabled" });
        self.save_config();
        self.refresh();
    }

    fn toggle_autostart(&mut self) {
        let wanted = !crate::autostart::is_enabled();
        // Reconciled rather than written blindly: `apply` reads the registry
        // back afterwards, so a policy that blocked the write cannot leave the
        // config claiming something the machine is not going to do, and
        // switching it back on also clears Task Manager's disabled marker.
        self.config.autostart = crate::autostart::apply(wanted);
        self.save_config();
        self.tray.set_autostart_checked(self.config.autostart);
    }

    /// Loads a preset into the engine and records it.
    fn activate_preset(&mut self, path: &std::path::Path) {
        let preset = match preset::FacPreset::from_file(path) {
            Ok(preset) => preset,
            Err(err) => {
                log::error!("could not read {}: {err}", path.display());
                return;
            }
        };

        self.handle.send(crate::engine::EngineCommand::LoadPreset {
            path: path.to_path_buf(),
            preset,
        });

        // Published so an already-open panel follows the tray's choice, the
        // same way [`Self::reconcile_selection`] lets the tray follow the
        // panel's.
        self.handle.status().set_active_preset(Some(path));

        self.active_preset = Some(path.to_path_buf());
        self.tray.set_active_preset(self.active_preset.as_deref());

        self.config.active_preset = Some(path.to_string_lossy().into_owned());
        self.save_config();
    }

    fn reload_presets(&mut self) {
        self.presets = preset::library();
        self.tray.set_presets(&self.presets);
        self.tray.set_active_preset(self.active_preset.as_deref());
        log::info!("preset list refreshed: {} entries", self.presets.len());
    }

    /// Switches the interface language and relabels the tray menu in place.
    ///
    /// The tray is relabelled, not rebuilt. It used to be destroyed and
    /// recreated around a belief that `muda` had no text setter; it has, and a
    /// rebuild is a strictly worse way to change a word — it can fail, it
    /// blinks the icon out of the notification area, and the failure mode of
    /// the error path is a process with no user interface at all.
    ///
    /// The panel needs none of this: it re-reads the string table each frame,
    /// so an open panel changes language on its next repaint.
    fn set_language(&mut self, language: Lang) {
        if language == i18n::current() {
            return;
        }

        i18n::set(language);
        // See `Tray::retitle` for what has to be rewritten and why the route
        // entry is not simply assigned.
        self.tray.retitle();

        // Recorded as an explicit choice, replacing "auto": the user has now
        // said which language they want, and a later change to the system
        // language should not undo it.
        self.config.language = language.code().to_owned();
        self.save_config();
        log::info!("interface language switched to {}", language.endonym());

        self.refresh();
    }

    /// Points the system's default output at the virtual card, once.
    ///
    /// Shared by the late-takeover path in [`Self::refresh`] and the tray item,
    /// so both report identically.
    fn engage_routing(&mut self) {
        match self.routing.engage() {
            EngageOutcome::Engaged { .. } => {
                self.config.previous_default_id = self.routing.previous().map(str::to_owned);
                self.save_config();
            }
            EngageOutcome::AlreadyRouted => {}
            EngageOutcome::NoVirtualCard => {
                log::warn!("cannot route audio through FxMini: no active virtual sound card");
            }
            EngageOutcome::Failed(err) => {
                log::error!("could not route audio through FxMini: {err}");
            }
        }
    }

    /// The tray's "route output through FxMini" item.
    ///
    /// The manual retry, for when the automatic switch was declined (the card
    /// appeared while disabled) or when the user moved the output away and has
    /// changed their mind.
    fn route_output(&mut self) {
        self.config.take_over_default = true;
        self.routing_attempted = true;
        self.engage_routing();
        self.refresh();
    }

    /// Opens the tuning panel, as if the tray item had been clicked.
    ///
    /// Public so `main` can honour `--panel`: a GUI window cannot be opened
    /// from a test harness otherwise, and it makes the panel smoke-testable
    /// without clicking through the tray.
    pub fn open_panel_now(&mut self) {
        self.open_panel();
    }

    fn open_panel(&mut self) {
        if panel::is_open() {
            return;
        }
        let shared = PanelShared {
            handle: self.handle.clone(),
            params: std::sync::Arc::clone(self.handle.params()),
            status: std::sync::Arc::clone(self.handle.status()),
            presets: self.presets.clone(),
        };
        if !panel::spawn(shared) {
            log::warn!("the tuning panel is already open");
        }
    }

    /// Asks for elevation and re-runs this executable to install the driver.
    fn request_driver_install(&mut self) {
        if driver::is_elevated() {
            match driver::install_with_default_guard() {
                Ok(outcome) => log::info!("driver install: {outcome:?}"),
                Err(err) => log::error!("driver install failed: {err}"),
            }
            self.refresh();
            return;
        }

        log::info!("requesting elevation to install the virtual sound card");
        if let Err(err) = driver::relaunch_elevated(&[driver::ARG_INSTALL_DRIVER]) {
            log::error!("{err}");
        }
    }

    fn request_driver_removal(&mut self) {
        if driver::is_elevated() {
            match driver::uninstall() {
                Ok(()) => log::info!("driver removed"),
                Err(err) => log::error!("driver removal failed: {err}"),
            }
            self.refresh();
            return;
        }

        log::info!("requesting elevation to remove the virtual sound card");
        if let Err(err) = driver::relaunch_elevated(&[driver::ARG_REMOVE_DRIVER]) {
            log::error!("{err}");
        }
    }

    /// Shared parameters, exposed for the binaries in `src/bin`.
    pub fn params(&self) -> &std::sync::Arc<SharedParams> {
        self.handle.params()
    }

    /// Live status, exposed for the binaries in `src/bin`.
    pub fn status(&self) -> &std::sync::Arc<EngineStatus> {
        self.handle.status()
    }

    /// The control handle, exposed for the binaries in `src/bin`.
    pub fn handle(&self) -> &EngineHandle {
        &self.handle
    }
}
