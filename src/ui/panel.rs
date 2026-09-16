//! The tuning panel — a window that exists only while it is open.
//!
//! ## Why the window lives on a worker thread
//!
//! The point of FxMini over the FxSound app is the memory ceiling. A windowing
//! toolkit keeps a GL context, a font atlas and a renderer alive for as long as
//! its event loop runs, so the cheapest way to honour "under 20 MB when idle"
//! is to have no window when idle: closing the panel destroys the window and
//! everything hanging off it.
//!
//! The thread placement costs two explicit opt-ins, both forced by winit:
//!
//! * `EventLoopBuilderExtWindows::with_any_thread` — winit refuses to build an
//!   event loop anywhere but the process's main thread, and FxMini's main
//!   thread is busy running the tray loop.
//! * One long-lived panel thread rather than one per window — winit permits
//!   exactly one event loop per *process*, so a fresh thread could never build
//!   its own. See [`panel_thread`].
//!
//! ## How it talks to the engine
//!
//! Parameters are shared atomics — the panel stores into `SharedParams` and the
//! audio thread notices through the generation counter, so slider drags need no
//! message passing. Only things the *engine* has to do (loading a `.fac`, which
//! mutates engine state, and saving one, which only the engine can serialise)
//! go through the command channel.
//!
//! ## Saving a preset
//!
//! The panel does not write the file. `.fac` is a positional format whose
//! numbers have to match what `DfxDsp::loadPreset` reads back, and the engine
//! already owns the serializer that produced every bundled preset — so the
//! panel asks for a save and *waits to be told what happened*, through
//! [`EngineStatus::presets_revision`] and [`EngineStatus::last_save`]. The
//! write lands on another thread; a button that reported success on its own
//! would be guessing.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::Duration;

use crate::engine::{EngineHandle, EngineStatus, SharedParams, MAX_BANDS, SPECTRUM_BANDS};
use crate::i18n::{self, PanelText};
use crate::preset::PresetEntry;

/// Set while a panel window exists, so a second tray click focuses the existing
/// one instead of opening another.
static PANEL_OPEN: AtomicBool = AtomicBool::new(false);

/// Wakes the panel thread, which is started on the first open and then lives
/// for the rest of the process — see [`panel_thread`] for why it cannot be one
/// thread per window.
static PANEL_TX: OnceLock<mpsc::Sender<PanelShared>> = OnceLock::new();

/// Whether a panel is currently on screen.
pub fn is_open() -> bool {
    PANEL_OPEN.load(Ordering::SeqCst)
}

/// Everything the panel needs, cloned in when the window opens.
pub struct PanelShared {
    pub handle: EngineHandle,
    pub params: Arc<SharedParams>,
    pub status: Arc<EngineStatus>,
    pub presets: Vec<PresetEntry>,
}

/// Asks the panel thread to open a window.
///
/// Returns `false` if one is already open — the caller should not treat that as
/// an error, it just means the click was redundant.
pub fn spawn(shared: PanelShared) -> bool {
    if PANEL_OPEN.swap(true, Ordering::SeqCst) {
        return false;
    }

    let Some(tx) = panel_sender() else {
        PANEL_OPEN.store(false, Ordering::SeqCst);
        return false;
    };

    if tx.send(shared).is_err() {
        // The thread only leaves `recv` when the process exits, so a send
        // failure means it died on a panic. Not worth retrying: the cached
        // event loop lives on that thread, so a replacement could not build one.
        log::error!("the panel thread has stopped; the panel cannot be reopened");
        PANEL_OPEN.store(false, Ordering::SeqCst);
        return false;
    }

    true
}

/// The panel thread's inbox, starting the thread the first time it is needed.
fn panel_sender() -> Option<&'static mpsc::Sender<PanelShared>> {
    if let Some(tx) = PANEL_TX.get() {
        return Some(tx);
    }

    let (tx, rx) = mpsc::channel::<PanelShared>();
    match std::thread::Builder::new()
        .name("fxmini-panel".to_owned())
        .spawn(move || panel_thread(rx))
    {
        Ok(_) => {
            // Were two callers to race here, the loser's thread would idle on
            // its receiver forever, which is harmless. In practice `spawn` is
            // only reached from the tray tick on the main thread.
            let _ = PANEL_TX.set(tx);
            PANEL_TX.get()
        }
        Err(err) => {
            log::error!("could not spawn the panel thread: {err}");
            None
        }
    }
}

/// Serves panel windows, one at a time, for the life of the process.
///
/// The thread outlives each window, and that is forced by winit: it allows
/// exactly one event loop per *process* — `EventLoopBuilder::build` flips a
/// process-wide flag and returns `RecreationAttempt` ever after, and `any_thread`
/// does not relax that — so no second thread could ever build its own. eframe
/// copes by caching the loop in a thread-local and running it with
/// `run_app_on_demand`, which means every window has to be driven from this same
/// thread.
///
/// Nothing is lost by staying alive: closing a window still drops the GL
/// context, the font atlas and the renderer, which is where the memory actually
/// goes. While the panel is closed this thread sits in `recv` owning none of it.
fn panel_thread(rx: mpsc::Receiver<PanelShared>) {
    while let Ok(shared) = rx.recv() {
        let app = PanelApp::new(shared);
        let options = eframe::NativeOptions {
            viewport: eframe::egui::ViewportBuilder::default()
                .with_title("FxMini")
                .with_inner_size([420.0, 620.0])
                .with_min_inner_size([360.0, 460.0])
                .with_resizable(true),
            // winit builds its loop on the calling thread but rejects any
            // thread other than the process's main thread unless this is set.
            // FxMini's main thread is running the tray loop, so the panel has
            // to opt in.
            event_loop_builder: Some(Box::new(
                |builder: &mut eframe::EventLoopBuilder<eframe::UserEvent>| {
                    use winit::platform::windows::EventLoopBuilderExtWindows as _;
                    let _ = builder.with_any_thread(true);
                },
            )),
            ..Default::default()
        };

        if let Err(err) = eframe::run_native(
            "FxMini",
            options,
            Box::new(move |cc| {
                install_cjk_font(&cc.egui_ctx);
                Ok(Box::new(app))
            }),
        ) {
            log::error!("the tuning panel could not start: {err}");
        }

        // The window is gone, so the next click may open a fresh one. Cleared
        // here rather than in a Drop impl so it also covers the case where
        // `run_native` failed before the window was ever created.
        PANEL_OPEN.store(false, Ordering::SeqCst);
    }

    log::debug!("the panel thread has exited");
}

/// The five effect knobs, in the order a user expects to see them.
///
/// The first element is the `DfxDsp::Effect` value, which is **not** the order
/// of the `.fac` `Main` slots — see `preset::MAIN_SLOT_TO_EFFECT`. The second is
/// a lookup into the string table rather than a label, so the list itself stays
/// language-independent.
const EFFECTS: [(i32, fn(&PanelText) -> &'static str); 5] = [
    (0, |t| t.effect_fidelity),
    (2, |t| t.effect_surround),
    (1, |t| t.effect_ambience),
    (3, |t| t.effect_dynamic_boost),
    (4, |t| t.effect_bass),
];

/// Band counts the engine accepts.
const BAND_CHOICES: [usize; 5] = [5, 10, 15, 20, 31];

/// Width of the label column in the effects and output lists.
///
/// Pixels, not `format!("{:<15}")` padding. Character-count padding only lines
/// up in a fixed-width font, and CJK glyphs are double-width, so padding by
/// characters puts every slider in the Chinese interface at a different x.
const LABEL_WIDTH: f32 = 104.0;

/// What the panel shows after a save attempt.
struct SaveFeedback {
    message: String,
    ok: bool,
}

struct PanelApp {
    shared: PanelShared,
    spectrum: Vec<f32>,
    /// The name typed into the save box.
    save_name: String,
    /// The last save result, shown under the box.
    save_feedback: Option<SaveFeedback>,
    /// The preset-folder revision this panel has already reacted to.
    seen_revision: u64,
}

impl PanelApp {
    fn new(shared: PanelShared) -> Self {
        let seen_revision = shared.status.presets_revision();
        Self {
            shared,
            spectrum: vec![0.0; SPECTRUM_BANDS],
            save_name: String::new(),
            save_feedback: None,
            seen_revision,
        }
    }

    /// Applies a preset: load it in the engine, then publish the choice.
    ///
    /// The choice goes into the shared status rather than a field here, because
    /// the tray tick and the config file are reconciled against that — a preset
    /// picked here has to survive a restart and show up in the tray menu.
    fn load_preset(&mut self, path: &Path) {
        match crate::preset::FacPreset::from_file(path) {
            Ok(preset) => {
                self.shared.handle.send(crate::engine::EngineCommand::LoadPreset {
                    path: path.to_path_buf(),
                    preset,
                });
                self.shared.status.set_active_preset(Some(path));
            }
            Err(err) => {
                log::error!("could not read {}: {err}", path.display());
            }
        }
    }

    /// Asks the engine to write the current settings out as a `.fac`.
    ///
    /// Returns nothing: the answer arrives asynchronously through the status,
    /// because the write happens on the audio thread. See
    /// [`Self::adopt_preset_changes`].
    fn request_save(&mut self) {
        let panel = &i18n::t().panel;

        let name = self.save_name.trim().to_owned();
        if name.is_empty() {
            self.save_feedback = Some(SaveFeedback {
                message: panel.save_name_required.to_owned(),
                ok: false,
            });
            return;
        }
        if !is_a_usable_filename(&name) {
            self.save_feedback = Some(SaveFeedback {
                message: panel.save_name_invalid.to_owned(),
                ok: false,
            });
            return;
        }

        let dir = crate::config::presets_dir();
        if let Err(err) = std::fs::create_dir_all(&dir) {
            log::error!("could not create {}: {err}", dir.display());
            self.save_feedback = Some(SaveFeedback {
                message: panel.save_failed.to_owned(),
                ok: false,
            });
            return;
        }

        // The engine joins this directory with `name + ".fac"` itself, so the
        // directory is what goes over the wire — see `EngineCommand::SavePreset`.
        self.shared
            .handle
            .send(crate::engine::EngineCommand::SavePreset {
                name,
                path: dir,
            });
        // Cleared so the message that is about to arrive is unambiguous, and so
        // a second identical save still produces a visible change.
        self.save_feedback = None;
    }

    /// Picks up a save that finished.
    ///
    /// Driven by the revision counter rather than by callbacks, the same way
    /// the tray follows [`SharedParams`]: the audio thread cannot call into the
    /// UI thread, and polling one integer per frame is free.
    ///
    /// The revision has exactly one setter — `EngineStatus::note_preset_written`
    /// — so a change means a save completed and [`EngineStatus::last_save`] is
    /// describing *that* save, not an older one.
    fn adopt_preset_changes(&mut self) {
        let revision = self.shared.status.presets_revision();
        if revision == self.seen_revision {
            return;
        }
        self.seen_revision = revision;

        let Some((filename, ok)) = self.shared.status.last_save() else {
            return;
        };
        let panel = &i18n::t().panel;

        if !ok {
            self.save_feedback = Some(SaveFeedback {
                message: panel.save_failed.to_owned(),
                ok: false,
            });
            return;
        }

        // Decided against the list as it stands *before* the rescan: afterwards
        // the file we just wrote is in there whether it replaced something or
        // not, and the distinction is the whole reason for saying "overwrote".
        let overwrote = self.shared.presets.iter().any(|entry| {
            entry
                .path
                .file_name()
                .is_some_and(|existing| existing == filename.as_str())
        });

        // Rescan so the new file appears in the combo box on this very frame.
        self.shared.presets = crate::preset::library();

        // Adopt what we wrote, so the header and the tray agree with the file
        // that now exists. Left until here rather than done at request time:
        // until the write is confirmed there may be no such file.
        let path = crate::config::presets_dir().join(&filename);
        if path.is_file() {
            self.shared.status.set_active_preset(Some(&path));
        }

        self.save_feedback = Some(SaveFeedback {
            message: panel.saved(&filename, overwrote),
            ok: true,
        });
    }
}

/// Whether `name` can be used as a Windows filename stem.
///
/// The name becomes a filename verbatim — the engine appends `.fac` and joins
/// it to the preset directory — so anything that changes *where* the file lands
/// has to be refused here: a separator would let a preset be written anywhere
/// on disk, and the reserved device names would make the write fail in a way
/// the user cannot see.
fn is_a_usable_filename(name: &str) -> bool {
    const FORBIDDEN: [char; 9] = ['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

    if name.contains(FORBIDDEN) || name.contains(char::is_control) {
        return false;
    }
    // Windows silently strips a trailing dot or space, which would make the
    // name we report differ from the name on disk.
    if name.ends_with('.') || name.ends_with(' ') {
        return false;
    }

    // `CON.fac` is still the console device: the reservation applies to the
    // stem, whatever the extension.
    let stem = name.to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0');
    !reserved
}

impl eframe::App for PanelApp {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        use eframe::egui;

        // eframe 0.35 hands the app a `Ui` for the root viewport instead of a
        // `Context`. The context is still needed for `request_repaint_after`,
        // and cloning it here is cheap — it is an `Arc` inside.
        let ctx = ui.ctx().clone();

        // A save that finished since the last frame, or a preset folder that
        // changed. Done first so the combo box and the footer below both see
        // the result on this frame rather than the next one.
        self.adopt_preset_changes();

        // Read the meters the audio thread publishes.
        for (index, slot) in self.spectrum.iter_mut().enumerate() {
            *slot = self.shared.status.spectrum(index);
        }

        let params = Arc::clone(&self.shared.params);
        let status = Arc::clone(&self.shared.status);
        let text = &i18n::t().panel;

        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("FxMini");

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut enabled = params.is_enabled();
                    if ui.checkbox(&mut enabled, text.enabled).changed() {
                        params.set_enabled(enabled);
                    }

                    // The dot doubles as a health indicator, so a glance at the
                    // title bar answers "is it actually working?".
                    let (colour, tip) = if status.is_running() {
                        (egui::Color32::from_rgb(0x3d, 0xdc, 0x97), text.status_processing)
                    } else if status.virtual_present() {
                        (egui::Color32::from_rgb(0xf5, 0xa6, 0x23), text.status_idle)
                    } else {
                        (egui::Color32::from_rgb(0xd9, 0x53, 0x4f), text.status_no_card)
                    };
                    ui.colored_label(colour, "●").on_hover_text(tip);
                });
            });
            ui.add_space(4.0);
        });

        // The footer is where a save happens: it is the one part of the window
        // that is always on screen, so "tune, then save" never needs a scroll.
        egui::Panel::bottom("footer").show(ui, |ui| {
            ui.add_space(3.0);

            ui.horizontal(|ui| {
                ui.label(text.save_heading);
                let field = ui.add(
                    egui::TextEdit::singleline(&mut self.save_name)
                        .hint_text(text.save_name_hint)
                        .desired_width(180.0),
                );
                // Enter is the same as clicking Save. After tuning, the name box
                // is where the cursor already is, so it is the shorter path.
                let pressed_enter =
                    field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button(text.save_button).clicked() || pressed_enter {
                    self.request_save();
                }
            });
            match &self.save_feedback {
                Some(feedback) => {
                    let colour = if feedback.ok {
                        egui::Color32::from_rgb(0x3d, 0xdc, 0x97)
                    } else {
                        egui::Color32::from_rgb(0xd9, 0x53, 0x4f)
                    };
                    // `RichText::small`, not `ui.small`: the latter returns a
                    // `Response`, which is not something a label can be made of.
                    ui.colored_label(colour, egui::RichText::new(&feedback.message).small());
                }
                None => {
                    ui.small(text.save_help);
                }
            }

            ui.separator();

            ui.horizontal_wrapped(|ui| {
                ui.small(match status.source_description() {
                    Some(description) => text.input_line(Some(&description)),
                    None => text.input_line(None),
                });
            });
            ui.horizontal_wrapped(|ui| {
                ui.small(match status.sink_description() {
                    Some(description) => text.output_line(Some(&description)),
                    None => text.output_line(None),
                });
            });
            if let Some(error) = status.last_error() {
                ui.colored_label(egui::Color32::from_rgb(0xd9, 0x53, 0x4f), format!("⚠ {error}"));
            }
            ui.horizontal(|ui| {
                ui.small(text.latency(status.latency_ms()));
                ui.separator();
                ui.small(text.underruns(status.underruns()));
                ui.separator();
                ui.small(text.drops(status.overruns()));
            });
            ui.add_space(3.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.preset_section(ui);
                ui.separator();
                self.effects_section(ui, &params);
                ui.separator();

                egui::CollapsingHeader::new(text.equalizer)
                    .default_open(true)
                    .show(ui, |ui| {
                        let mut eq_on = params.eq_on();
                        if ui.checkbox(&mut eq_on, text.enabled).changed() {
                            params.set_eq_on(eq_on);
                        }
                        self.eq_section(ui, &params);
                    });

                ui.separator();
                self.output_section(ui, &params);
                ui.separator();
                self.meter_section(ui);
            });
        });

        // ~30 fps is plenty for meters, and keeps the window from spinning the
        // GPU when nothing is happening.
        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

impl PanelApp {
    fn preset_section(&mut self, ui: &mut eframe::egui::Ui) {
        use eframe::egui;

        let text = &i18n::t().panel;

        // Read the selection from the shared status rather than from a snapshot
        // taken when the window opened, so a preset changed from the tray menu
        // shows up here as well.
        let active = self.shared.status.active_preset().map(PathBuf::from);

        ui.horizontal(|ui| {
            ui.label(text.preset);
            let current = active
                .as_ref()
                .and_then(|path| {
                    self.shared
                        .presets
                        .iter()
                        .find(|entry| &entry.path == path)
                        .map(|entry| entry.name.clone())
                })
                .unwrap_or_else(|| text.no_selection.to_owned());

            egui::ComboBox::from_id_salt("preset-combo")
                .selected_text(current)
                .width(220.0)
                .show_ui(ui, |ui| {
                    let presets: Vec<(String, PathBuf)> = self
                        .shared
                        .presets
                        .iter()
                        .map(|entry| (entry.name.clone(), entry.path.clone()))
                        .collect();

                    for (name, path) in presets {
                        let selected = active.as_ref().is_some_and(|active| active == &path);
                        if ui.selectable_label(selected, name).clicked() {
                            self.load_preset(&path);
                        }
                    }
                });
        });
    }

    fn effects_section(&mut self, ui: &mut eframe::egui::Ui, params: &Arc<SharedParams>) {
        let text = &i18n::t().panel;

        ui.heading(text.effects);
        ui.add_space(2.0);

        for (id, label) in EFFECTS {
            let mut value = params.effect(id);
            ui.horizontal(|ui| {
                label_column(ui, label(text));
                if ui
                    .add(
                        eframe::egui::Slider::new(&mut value, 0.0..=10.0)
                            .show_value(false)
                            .fixed_decimals(1),
                    )
                    .changed()
                {
                    params.set_effect(id, value);
                }
                ui.small(format!("{value:.1}"));
            });
        }
    }

    fn eq_section(&mut self, ui: &mut eframe::egui::Ui, params: &Arc<SharedParams>) {
        let text = &i18n::t().panel;
        let bands = params.num_bands().min(MAX_BANDS);

        ui.horizontal(|ui| {
            ui.label(text.bands);
            let mut chosen = bands;
            eframe::egui::ComboBox::from_id_salt("band-count")
                .selected_text(bands.to_string())
                .show_ui(ui, |ui| {
                    for option in BAND_CHOICES {
                        if ui.selectable_label(option == bands, option.to_string()).clicked() {
                            chosen = option;
                        }
                    }
                });
            if chosen != bands {
                params.set_num_bands(chosen);
            }
        });

        ui.add_space(2.0);

        // A real curve editor would be nicer, but sliders make the current
        // value legible and are unambiguous to drag on a trackpad.
        eframe::egui::Grid::new("eq-grid")
            .num_columns(3)
            .spacing([8.0, 2.0])
            .show(ui, |ui| {
                for band in 0..bands {
                    let freq = params.band_freq(band);
                    let label = if freq >= 1000.0 {
                        format!("{:.1} kHz", freq / 1000.0)
                    } else {
                        format!("{freq:.0} Hz")
                    };
                    ui.label(label);

                    let mut gain = params.band_gain(band);
                    if ui
                        .add(
                            eframe::egui::Slider::new(&mut gain, -12.0..=12.0)
                                .show_value(false),
                        )
                        .changed()
                    {
                        params.set_band_gain(band, gain);
                    }
                    ui.small(format!("{gain:+.1} dB"));
                    ui.end_row();
                }
            });
    }

    fn output_section(&mut self, ui: &mut eframe::egui::Ui, params: &Arc<SharedParams>) {
        let text = &i18n::t().panel;

        ui.heading(text.output);
        ui.add_space(2.0);

        let rows: [(&str, fn(&SharedParams) -> f32, fn(&SharedParams, f32), f32, f32); 4] = [
            (text.balance, SharedParams::balance, SharedParams::set_balance, -20.0, 20.0),
            (text.master_gain, SharedParams::master_gain, SharedParams::set_master_gain, -20.0, 20.0),
            (text.normalization, SharedParams::normalization, SharedParams::set_normalization, 0.0, 4.0),
            (text.volume_leveling, SharedParams::volume_leveling, SharedParams::set_volume_leveling, 0.0, 4.0),
        ];

        for (label, get, set, min, max) in rows {
            let mut value = get(params);
            ui.horizontal(|ui| {
                label_column(ui, label);
                if ui
                    .add(
                        eframe::egui::Slider::new(&mut value, min..=max)
                            .show_value(false),
                    )
                    .changed()
                {
                    set(params, value);
                }
                ui.small(format!("{value:+.2}"));
            });
        }

        let mut q = params.filter_q();
        ui.horizontal(|ui| {
            label_column(ui, text.filter_q);
            if ui
                .add(eframe::egui::Slider::new(&mut q, 1.0..=3.0).show_value(false))
                .changed()
            {
                params.set_filter_q(q);
            }
            ui.small(format!("{q:.2}"));
        });
    }

    fn meter_section(&mut self, ui: &mut eframe::egui::Ui) {
        use eframe::egui;

        ui.heading(i18n::t().panel.spectrum);
        ui.add_space(2.0);

        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 72.0), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 3.0, egui::Color32::from_rgb(0x16, 0x1c, 0x26));

        let count = self.spectrum.len().max(1);
        let slot = rect.width() / count as f32;
        for (index, &value) in self.spectrum.iter().enumerate() {
            let height = (value.clamp(0.0, 1.0) * rect.height()).max(1.0);
            let x = rect.left() + index as f32 * slot;
            let bar = egui::Rect::from_min_size(
                egui::pos2(x + slot * 0.15, rect.bottom() - height),
                egui::vec2(slot * 0.7, height),
            );
            // Green→amber as the band approaches full scale, which is the
            // convention users read without a legend.
            let colour = if value < 0.7 {
                egui::Color32::from_rgb(0x3d, 0xdc, 0x97)
            } else if value < 0.9 {
                egui::Color32::from_rgb(0xf5, 0xa6, 0x23)
            } else {
                egui::Color32::from_rgb(0xd9, 0x53, 0x4f)
            };
            painter.rect_filled(bar, 1.0, colour);
        }
    }
}

/// Draws a label in the fixed-width column the sliders line up against.
///
/// `add_sized` rather than `format!("{label:<14}")`: character padding only
/// aligns in a monospace face, and a CJK glyph is twice as wide as a Latin one,
/// so counting characters would give each row a different label width in the
/// Chinese interface and the sliders would start at a different x every line.
fn label_column(ui: &mut eframe::egui::Ui, text: &str) {
    ui.add_sized(
        [LABEL_WIDTH, ui.spacing().interact_size.y],
        eframe::egui::Label::new(text),
    );
}

/// Registers a CJK-capable font from the system, so preset names like
/// 「音乐」 render instead of tofu boxes.
///
/// egui ships Latin-only fonts, and embedding a CJK face would add ~10 MB to
/// the binary — more than the entire memory budget is trying to save. Loading
/// from `C:\Windows\Fonts` costs nothing and every Chinese Windows install has
/// at least one of these.
///
/// `.ttc` collections are avoided where possible: the font stack egui uses
/// parses single faces, and a collection's first face is not always the one
/// wanted.
fn install_cjk_font(ctx: &eframe::egui::Context) {
    const CANDIDATES: [&str; 6] = [
        r"C:\Windows\Fonts\Deng.ttf",
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\simkai.ttf",
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\simsun.ttc",
        r"C:\Windows\Fonts\msjh.ttc",
    ];

    for path in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };

        let mut fonts = eframe::egui::FontDefinitions::default();
        fonts.font_data.insert(
            "cjk".to_owned(),
            Arc::new(eframe::egui::FontData::from_owned(bytes)),
        );
        // Appended, not prepended: Latin text keeps the sharper default face,
        // and the CJK font only fills in the glyphs the default lacks.
        for family in [
            eframe::egui::FontFamily::Proportional,
            eframe::egui::FontFamily::Monospace,
        ] {
            fonts.families.entry(family).or_default().push("cjk".to_owned());
        }
        ctx.set_fonts(fonts);
        log::debug!("loaded CJK font from {path}");
        return;
    }

    log::warn!("no CJK font found in C:\\Windows\\Fonts; Chinese preset names will show as boxes");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowW, GetWindowThreadProcessId, PostMessageW, WM_CLOSE,
    };

    #[test]
    fn a_preset_name_cannot_escape_the_preset_folder() {
        // The name becomes a filename verbatim, so a separator would let a
        // preset be written anywhere the process can write.
        for bad in [
            r"..\..\evil",
            "a/b",
            r"C:\Windows\System32\x",
            "trailing.",
            "trailing ",
            "with:colon",
            "star*",
            "quote\"",
            "pipe|",
            "less<more",
            "question?",
            "new\nline",
            "nul\u{0}byte",
        ] {
            assert!(!is_a_usable_filename(bad), "{bad:?} should be refused");
        }
    }

    #[test]
    fn reserved_device_names_are_refused_whatever_the_extension() {
        // `CON.fac` is still the console: the reservation is on the stem.
        for bad in ["CON", "con", "PRN", "AUX", "NUL", "COM1", "lpt9"] {
            assert!(!is_a_usable_filename(bad), "{bad:?} should be refused");
        }
        // …but only the exact stem, and only digits 1-9.
        for good in ["CON2", "COM0", "LPT", "COM10", "CONSOLE", "My Preset 1"] {
            assert!(is_a_usable_filename(good), "{good:?} should be allowed");
        }
    }

    #[test]
    fn ordinary_names_are_usable() {
        for good in ["音乐", "Bass boost", "My Tune (v2)", "低音_2"] {
            assert!(is_a_usable_filename(good), "{good:?} should be allowed");
        }
    }

    /// This process's panel window, if one is up.
    ///
    /// The title alone is not enough — a second copy of FxMini running on the
    /// same desktop has a window with the same name, and a test that closed
    /// *that* one would hang instead of failing.
    fn panel_window() -> Option<windows::Win32::Foundation::HWND> {
        // SAFETY: a null class name is documented as "match any class", and the
        // title is a NUL-terminated literal.
        let window = unsafe { FindWindowW(PCWSTR::null(), w!("FxMini")) }.ok()?;

        // SAFETY: the handle came from FindWindowW and only the pid is written.
        let mut owner = 0u32;
        unsafe { GetWindowThreadProcessId(window, Some(&mut owner)) };
        (owner == std::process::id()).then_some(window)
    }

    /// A panel's inputs, with nothing real behind them.
    fn shared() -> PanelShared {
        let handle = crate::engine::detached_handle();
        PanelShared {
            params: Arc::clone(handle.params()),
            status: Arc::clone(handle.status()),
            handle,
            presets: Vec::new(),
        }
    }

    fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if ready() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for {what}");
    }

    /// Pauses before the first window is opened, when asked to.
    ///
    /// A window that has never been opened leaves no trace in the process's
    /// private bytes, and after one closes the process settles back to a floor.
    /// Whether that floor equals the pre-open baseline is the difference between
    /// "window teardown releases everything" and "something outlives the
    /// window", and answering it needs a moment where the process exists but no
    /// window ever has. Diagnostic only; zero by default.
    fn settle_before_first_open() {
        let millis = std::env::var("FXMINI_PANEL_TEST_SETTLE_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        if millis > 0 {
            std::thread::sleep(Duration::from_millis(millis));
        }
    }

    /// A panel has to be openable more than once.
    ///
    /// winit allows exactly one event loop per process, and `any_thread` does
    /// not relax that, so the obvious "one thread per window" design opens the
    /// first window and then fails every later one with `RecreationAttempt`.
    /// This is the regression guard for that, and the reason [`panel_thread`]
    /// is long-lived.
    ///
    /// `FXMINI_PANEL_TEST_ROUNDS` raises the cycle count, which is how the
    /// window's memory footprint is watched from outside the process — the
    /// interesting question is whether each cycle returns the memory the last
    /// one used.
    #[test]
    #[ignore = "opens real windows; needs a desktop session — run with `-- --ignored`"]
    fn the_panel_reopens_after_being_closed() {
        let rounds: usize = std::env::var("FXMINI_PANEL_TEST_ROUNDS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(2);

        settle_before_first_open();

        for round in 1..=rounds {
            assert!(
                spawn(shared()),
                "round {round}: the open request was refused"
            );
            wait_for("the panel window to appear", || panel_window().is_some());
            assert!(is_open(), "round {round}: is_open() should be true");

            // Close it the way the title bar's X does.
            let window = panel_window().expect("the window was just found");
            // SAFETY: the handle came from FindWindowW a moment ago.
            unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0)) }
                .expect("post WM_CLOSE");

            wait_for("the panel to report itself closed", || !is_open());
            wait_for("the window to disappear", || panel_window().is_none());
        }
    }
}
