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
//! mutates engine state) go through the command channel.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::Duration;

use crate::engine::{EngineHandle, EngineStatus, SharedParams, MAX_BANDS, SPECTRUM_BANDS};
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
/// The third element is the `DfxDsp::Effect` value, which is **not** the order
/// of the `.fac` `Main` slots — see `preset::MAIN_SLOT_TO_EFFECT`.
const EFFECTS: [(i32, &str); 5] = [
    (0, "Fidelity"),
    (2, "Surround"),
    (1, "Ambience"),
    (3, "Dynamic Boost"),
    (4, "Bass"),
];

/// Band counts the engine accepts.
const BAND_CHOICES: [usize; 5] = [5, 10, 15, 20, 31];

struct PanelApp {
    shared: PanelShared,
    spectrum: Vec<f32>,
}

impl PanelApp {
    fn new(shared: PanelShared) -> Self {
        Self {
            shared,
            spectrum: vec![0.0; SPECTRUM_BANDS],
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
}

impl eframe::App for PanelApp {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        use eframe::egui;

        // eframe 0.35 hands the app a `Ui` for the root viewport instead of a
        // `Context`. The context is still needed for `request_repaint_after`,
        // and cloning it here is cheap — it is an `Arc` inside.
        let ctx = ui.ctx().clone();

        // Read the meters the audio thread publishes.
        for (index, slot) in self.spectrum.iter_mut().enumerate() {
            *slot = self.shared.status.spectrum(index);
        }

        let params = Arc::clone(&self.shared.params);
        let status = Arc::clone(&self.shared.status);

        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("FxMini");

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut enabled = params.is_enabled();
                    if ui.checkbox(&mut enabled, "Enabled").changed() {
                        params.set_enabled(enabled);
                    }

                    // The dot doubles as a health indicator, so a glance at the
                    // title bar answers "is it actually working?".
                    let (colour, tip) = if status.is_running() {
                        (egui::Color32::from_rgb(0x3d, 0xdc, 0x97), "processing")
                    } else if status.virtual_present() {
                        (egui::Color32::from_rgb(0xf5, 0xa6, 0x23), "idle")
                    } else {
                        (egui::Color32::from_rgb(0xd9, 0x53, 0x4f), "no virtual sound card")
                    };
                    ui.colored_label(colour, "●").on_hover_text(tip);
                });
            });
            ui.add_space(4.0);
        });

        egui::Panel::bottom("footer").show(ui, |ui| {
            ui.add_space(3.0);
            ui.horizontal_wrapped(|ui| {
                ui.small(match status.source_description() {
                    Some(description) => format!("in:  {description}"),
                    None => "in:  —".to_owned(),
                });
            });
            ui.horizontal_wrapped(|ui| {
                ui.small(match status.sink_description() {
                    Some(description) => format!("out: {description}"),
                    None => "out: —".to_owned(),
                });
            });
            if let Some(error) = status.last_error() {
                ui.colored_label(egui::Color32::from_rgb(0xd9, 0x53, 0x4f), format!("⚠ {error}"));
            }
            ui.horizontal(|ui| {
                ui.small(format!("latency {} ms", status.latency_ms()));
                ui.separator();
                ui.small(format!("underruns {}", status.underruns()));
                ui.separator();
                ui.small(format!("drops {}", status.overruns()));
            });
            ui.add_space(3.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.preset_section(ui);
                ui.separator();
                self.effects_section(ui, &params);
                ui.separator();

                egui::CollapsingHeader::new("Equalizer")
                    .default_open(true)
                    .show(ui, |ui| {
                        let mut eq_on = params.eq_on();
                        if ui.checkbox(&mut eq_on, "Enabled").changed() {
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

        // Read the selection from the shared status rather than from a snapshot
        // taken when the window opened, so a preset changed from the tray menu
        // shows up here as well.
        let active = self.shared.status.active_preset().map(PathBuf::from);

        ui.horizontal(|ui| {
            ui.label("Preset");
            let current = active
                .as_ref()
                .and_then(|path| {
                    self.shared
                        .presets
                        .iter()
                        .find(|entry| &entry.path == path)
                        .map(|entry| entry.name.clone())
                })
                .unwrap_or_else(|| "—".to_owned());

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
        ui.heading("Effects");
        ui.add_space(2.0);

        for (id, label) in EFFECTS {
            let mut value = params.effect(id);
            ui.horizontal(|ui| {
                ui.label(format!("{label:<14}"));
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
        let bands = params.num_bands().min(MAX_BANDS);

        ui.horizontal(|ui| {
            ui.label("Bands");
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
        ui.heading("Output");
        ui.add_space(2.0);

        let rows: [(&str, fn(&SharedParams) -> f32, fn(&SharedParams, f32), f32, f32); 4] = [
            ("Balance", SharedParams::balance, SharedParams::set_balance, -20.0, 20.0),
            ("Master gain", SharedParams::master_gain, SharedParams::set_master_gain, -20.0, 20.0),
            ("Normalization", SharedParams::normalization, SharedParams::set_normalization, 0.0, 4.0),
            ("Volume leveling", SharedParams::volume_leveling, SharedParams::set_volume_leveling, 0.0, 4.0),
        ];

        for (label, get, set, min, max) in rows {
            let mut value = get(params);
            ui.horizontal(|ui| {
                ui.label(format!("{label:<15}"));
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
            ui.label(format!("{:<15}", "Filter Q"));
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

        ui.heading("Spectrum");
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
