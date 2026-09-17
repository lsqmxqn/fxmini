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
//!
//! ## Layout
//!
//! Fixed header, a middle that scrolls only when it has to, fixed footer. The
//! header carries the two things worth knowing without scrolling — is it
//! working, and is it on — and the footer carries the one action worth reaching
//! without scrolling, which is saving what you just tuned.
//!
//! The middle is the equalizer across the full width, then five cards of
//! controls in **two columns**. Stacked in one column they came to about a
//! thousand points, which is a whole screen's height for a tray applet; side by
//! side the window fits on any desktop with nothing hidden. Below
//! [`TWO_COLUMN_MIN_WIDTH`] the panel goes back to one column rather than
//! squeeze the sliders, and the scroll area earns its keep.
//!
//! The window is not a fixed size either. [`PanelApp::fit_to_content`] measures
//! what the cards actually laid out and asks for exactly that much height on
//! the first frame — the honest answer to "how tall should this be", which
//! depends on the card set, the font metrics and the width, and which no
//! constant gets right.
//!
//! Colours and metrics come from [`super::theme`]; nothing below names a hex
//! literal, so the panel follows the system theme and stays readable in both.
//!
//! ## The equalizer is a curve, not a list
//!
//! A slider per band meant 31 rows of scrolling for a control whose whole
//! meaning is its shape. It is now one frequency-response curve with draggable
//! points. Two consequences worth knowing:
//!
//! * The dots are the settings and the smooth line between them is a drawing of
//!   them, not a computed transfer function. See [`band_curve`] for why the
//!   interpolation is monotone rather than the usual spline.
//! * Arrow keys move the selected band, because a curve with no keyboard path
//!   would be a regression on the sliders it replaced.

use std::f32::consts::SQRT_2;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::Duration;

use eframe::egui::{
    self, Align, CornerRadius, FontId, Layout, RichText, Sense, Stroke, StrokeKind, Vec2,
};

use crate::engine::{EngineHandle, EngineStatus, SharedParams, MAX_BANDS, SPECTRUM_BANDS};
use crate::i18n::{self, PanelText};
use crate::preset::PresetEntry;
use crate::ui::theme::{self, radius, space, Palette};

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
                // Opening size, not the final one: a provisional height the
                // panel corrects against its own content on the first frame
                // (see `PanelApp::fit_to_content`). Width is real — it is
                // chosen for the equalizer's 31 points, which crowd together
                // past the point of being separately grabbable if it shrinks.
                .with_inner_size([DEFAULT_PANEL_WIDTH, OPENING_PANEL_HEIGHT])
                .with_min_inner_size([392.0, MIN_PANEL_HEIGHT])
                .with_resizable(true)
                // A window taller than the screen would put its own footer —
                // the save box — somewhere the user cannot reach.
                .with_clamp_size_to_monitor_size(true),
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
                // Both, and in this order: the palette has to be in place before
                // the first frame, and the CJK face before any text is laid out.
                theme::apply(&cc.egui_ctx);
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

/// One effect row: the `DfxDsp::Effect` value and the label to look up for it.
type EffectSlot = (i32, fn(&PanelText) -> &'static str);

/// The five effect knobs, in the order a user expects to see them.
///
/// The first element is the `DfxDsp::Effect` value, which is **not** the order
/// of the `.fac` `Main` slots — see `preset::MAIN_SLOT_TO_EFFECT`. The second is
/// a lookup into the string table rather than a label, so the list itself stays
/// language-independent.
const EFFECTS: [EffectSlot; 5] = [
    (0, |t| t.effect_fidelity),
    (2, |t| t.effect_surround),
    (1, |t| t.effect_ambience),
    (3, |t| t.effect_dynamic_boost),
    (4, |t| t.effect_bass),
];

/// Band counts the engine accepts.
const BAND_CHOICES: [usize; 5] = [5, 10, 15, 20, 31];

/// Width of the label column in the effect and output rows.
///
/// Pixels, not `format!("{:<15}")` padding. Character-count padding only lines
/// up in a fixed-width font, and a CJK glyph is double-width, so padding by
/// characters put every slider in the Chinese interface at a different x.
///
/// Wide enough for the longest label in either language — "Volume leveling" —
/// so the truncation below is a guard rather than something a user sees. The
/// rows are labels of known length, not user data, which is why this can be a
/// fixed number at all.
const LABEL_WIDTH: f32 = 100.0;

/// Width of the value column beside a slider.
///
/// Fixed so the sliders themselves are all the same width: the longest value
/// this column ever holds is `-12.0 dB`, and letting the column size itself
/// would make every row's slider start somewhere new.
const VALUE_WIDTH: f32 = 62.0;

/// Height of the equalizer plot, in points.
const EQ_HEIGHT: f32 = 156.0;

/// How wide the panel opens, in points.
///
/// Wide enough for two columns of labelled sliders beside each other. At one
/// column the panel is nearly a screen tall — the equalizer alone is 156
/// points, and there are five cards of sliders under it — so the width is
/// buying height, not luxury.
const DEFAULT_PANEL_WIDTH: f32 = 660.0;

/// Below this the two columns would each be narrower than a labelled slider
/// can be, so the panel goes back to one column and scrolls instead of
/// squeezing five cards into four hundred points of width.
const TWO_COLUMN_MIN_WIDTH: f32 = 560.0;

/// The height the window is *created* at, before the content has been measured.
///
/// Only ever seen for one frame — [`PanelApp::fit_to_content`] replaces it as
/// soon as the panel knows how tall it really is. Close enough to the usual
/// answer that the correction is not a visible jump.
const OPENING_PANEL_HEIGHT: f32 = 620.0;

/// The shortest the panel can be squeezed to, and the floor
/// [`PanelApp::fit_to_content`] will not go below.
///
/// The header, the footer, and the equalizer card are what has to be reachable
/// at this size; everything else is one scroll away.
const MIN_PANEL_HEIGHT: f32 = 480.0;

/// How much of the monitor the fitted window leaves alone, in points.
///
/// A window exactly as tall as the monitor cannot be dragged and hides its own
/// bottom edge behind the taskbar.
const FIT_MARGIN: f32 = 72.0;

/// The half-range of the equalizer's vertical axis, in dB. Bands are ±12 dB, so
/// the axis is too — anything more would waste height on values no band can
/// reach.
const EQ_DB_RANGE: f32 = 12.0;

/// How far from a point, in points, a press counts as grabbing it.
///
/// Generous on purpose: with 31 bands the points sit ~11 px apart, so this
/// overlaps its neighbours, and "the nearest one" is what a user means anyway.
const EQ_GRAB_RADIUS: f32 = 20.0;

/// Gain step for one arrow-key press, and with shift held.
const EQ_KEY_STEP: f32 = 0.5;
const EQ_KEY_STEP_FINE: f32 = 0.1;

/// The equalizer's vertical axis unit.
///
/// A const rather than a string-table entry: dB is an SI symbol, not a word,
/// and translating it would be wrong in both languages.
const DB_UNIT: &str = "dB";

/// Monotone cubic (Fritsch–Carlson) interpolation through the band gains.
///
/// `xs` must be strictly increasing; `ys` are the gains. Returns the curve's
/// value at `at`, clamped to the data at both ends — a curve that shot off the
/// edge of the plot would be worse than a flat one.
///
/// ## Why monotone and not a plain spline
///
/// A natural cubic spline through `+12, −12, +12` overshoots to roughly ±17 dB
/// between the points. On this one control the picture *is* the setting, so
/// drawing a boost the user never asked for — past an axis that only goes to
/// 12 — is a lie about the state of the EQ. The Fritsch–Carlson limiter below
/// is the standard fix: it caps each tangent so the interpolant cannot leave
/// the interval between its own endpoints.
fn band_curve(xs: &[f32], ys: &[f32], at: f32) -> f32 {
    debug_assert_eq!(xs.len(), ys.len(), "xs and ys must be the same length");
    let n = xs.len().min(ys.len());

    match n {
        0 => return 0.0,
        1 => return ys[0],
        _ => {}
    }
    if at <= xs[0] {
        return ys[0];
    }
    if at >= xs[n - 1] {
        return ys[n - 1];
    }

    // Secant slopes between adjacent points.
    let secants: Vec<f32> = (0..n - 1)
        .map(|i| (ys[i + 1] - ys[i]) / (xs[i + 1] - xs[i]))
        .collect();

    // Start from the average of the neighbouring secants.
    let mut tangents = vec![0.0f32; n];
    tangents[0] = secants[0];
    tangents[n - 1] = secants[n - 2];
    for i in 1..n - 1 {
        tangents[i] = (secants[i - 1] + secants[i]) * 0.5;
    }

    // The limiter has two halves and needs both.
    //
    // First, the sign rule. At an interior point where the slope changes sign
    // the curve is at a local extreme, and the tangent *must* be zero there —
    // the average above can easily come out pointing the wrong way. With
    // `-24` on the left and `+12` on the right the average is `-6`, and a
    // tangent of `-6` entering a rising segment dips below the point it starts
    // from. That is the exact undershoot the test catches: `-12.21` on an axis
    // that stops at `-12`.
    for i in 1..n - 1 {
        if secants[i - 1] * secants[i] <= 0.0 {
            tangents[i] = 0.0;
        }
    }

    // Second, the circle of three. With both tangents now on the same side of
    // zero as the secant they belong to, `a² + b² ≤ 9` is Fritsch–Carlson's
    // sufficient condition for the cubic to be monotone across the interval,
    // and a monotone cubic is trapped between its own endpoints.
    for i in 0..n - 1 {
        let d = secants[i];
        if d == 0.0 {
            // A flat run has to be entered and left flat, or the curve bulges
            // off the plateau on the way in.
            tangents[i] = 0.0;
            tangents[i + 1] = 0.0;
        } else {
            let a = tangents[i] / d;
            let b = tangents[i + 1] / d;
            let sum = a * a + b * b;
            if sum > 9.0 {
                let scale = 3.0 / sum.sqrt();
                tangents[i] = scale * a * d;
                tangents[i + 1] = scale * b * d;
            }
        }
    }

    // Locate the interval and evaluate the Hermite basis on it.
    let mut i = 0;
    while i + 2 < n && at > xs[i + 1] {
        i += 1;
    }
    let h = xs[i + 1] - xs[i];
    let t = (at - xs[i]) / h;
    let (t2, t3) = (t * t, t * t * t);

    (2.0 * t3 - 3.0 * t2 + 1.0) * ys[i]
        + (t3 - 2.0 * t2 + t) * h * tangents[i]
        + (-2.0 * t3 + 3.0 * t2) * ys[i + 1]
        + (t3 - t2) * h * tangents[i + 1]
}

/// Rounds a gain to the resolution the panel displays, snapping a hair either
/// side of zero to exactly zero.
///
/// Without the snap a band dragged back to the middle lands on 0.04 dB, which
/// prints as `0.0` and is a value the user did not ask for; it also means the
/// "is this band flat?" question has no exact answer.
fn snap_gain(db: f32) -> f32 {
    let rounded = (db.clamp(-EQ_DB_RANGE, EQ_DB_RANGE) * 10.0).round() / 10.0;
    if rounded.abs() < 0.15 {
        0.0
    } else {
        rounded
    }
}

/// One row of the output card: label, reader, writer, and the range the slider
/// spans.
///
/// Function pointers rather than a list of closures because these are all
/// separate methods on `SharedParams` — each one an atomic store shared with
/// the audio thread — and there is no trait to reach them through. The table
/// is what lets the five rows be one loop instead of five near-identical
/// blocks.
type OutputRow = (
    &'static str,
    fn(&SharedParams) -> f32,
    fn(&SharedParams, f32),
    f32,
    f32,
);

/// What the panel shows after a save attempt.
struct SaveFeedback {
    message: String,
    ok: bool,
}

/// The window height a panel of this content wants.
///
/// Split out from the window itself so the arithmetic can be tested: the cap
/// and the floor are the two places a wrong answer is unrecoverable — one puts
/// the save box below the taskbar, the other above the top of the screen.
///
/// `monitor` is `None` when egui has not learned the size yet, which happens on
/// very early frames on some platforms. Unclamped is the right answer there:
/// the provisional height is already safe, and the window cannot be fitted
/// twice.
fn fitted_height(content: f32, chrome: f32, monitor: Option<f32>) -> f32 {
    let wanted = (content + chrome).ceil();
    let capped = monitor.map_or(wanted, |height| wanted.min(height - FIT_MARGIN));
    // The floor wins over the cap. A monitor short enough for the two to
    // disagree cannot show the header and the footer at once at any height, and
    // a window the OS then clamps is better than one this code made too small
    // to hold its own controls.
    capped.max(MIN_PANEL_HEIGHT)
}

struct PanelApp {
    shared: PanelShared,
    spectrum: Vec<f32>,
    /// Decaying peak markers, so a transient stays visible for a moment instead
    /// of being gone before the eye arrives.
    peaks: Vec<f32>,
    /// The name typed into the save box.
    save_name: String,
    /// The last save result, shown under the box.
    save_feedback: Option<SaveFeedback>,
    /// The preset-folder revision this panel has already reacted to.
    seen_revision: u64,
    /// The band currently held by the pointer, if any.
    eq_dragging: Option<usize>,
    /// The band last clicked, which is the one the arrow keys move.
    eq_selected: Option<usize>,
    /// Whether the window has already been resized to its content.
    ///
    /// Once only. After that the size belongs to whoever dragged the frame, and
    /// re-imposing a computed one would undo their choice.
    fitted: bool,
}

impl PanelApp {
    fn new(shared: PanelShared) -> Self {
        let seen_revision = shared.status.presets_revision();
        Self {
            shared,
            spectrum: vec![0.0; SPECTRUM_BANDS],
            peaks: vec![0.0; SPECTRUM_BANDS],
            save_name: String::new(),
            save_feedback: None,
            seen_revision,
            eq_dragging: None,
            eq_selected: None,
            fitted: false,
        }
    }

    /// Grows the window to its content, once, on the frame the content is first
    /// measurable.
    ///
    /// The height cannot be a constant. It depends on the card set, on the font
    /// metrics (the CJK face is taller than the Latin one), and on the width —
    /// a narrower window wraps the device names and adds a line. Any number
    /// written into `with_inner_size` is wrong for somebody. So the window
    /// opens at a provisional size, and this measures what the panel actually
    /// laid out and asks for exactly that.
    ///
    /// Capped by the monitor, which is the one thing that genuinely cannot be
    /// exceeded — on a short screen the window stays inside the work area and
    /// the scroll area inside it earns its keep.
    fn fit_to_content(&mut self, ctx: &egui::Context, content: Vec2, chrome: f32) {
        if self.fitted || content.y < 1.0 {
            return;
        }
        self.fitted = true;

        let (width, monitor) = ctx.input(|input| {
            (
                input.viewport().inner_rect.map(|rect| rect.width()),
                input.viewport().monitor_size,
            )
        });

        let wanted = fitted_height(content.y, chrome, monitor.map(|size| size.y));
        let size = Vec2::new(width.unwrap_or(DEFAULT_PANEL_WIDTH), wanted);
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
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

    /// Flattens every band back to 0 dB.
    fn flatten_eq(&self, params: &Arc<SharedParams>) {
        for band in 0..params.num_bands().min(MAX_BANDS) {
            params.set_band_gain(band, 0.0);
        }
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
        let ctx = ui.ctx().clone();

        // A save that finished since the last frame, or a preset folder that
        // changed. Done first so the combo box and the footer below both see
        // the result on this frame rather than the next one.
        self.adopt_preset_changes();

        // Read the meters the audio thread publishes, and decay the peak
        // markers. 0.94 per frame at ~30 fps is a little under a second to
        // fall back to the signal, which is long enough to see and short enough
        // not to lie about the current level.
        for (index, slot) in self.spectrum.iter_mut().enumerate() {
            let value = self.shared.status.spectrum(index);
            *slot = value;
            self.peaks[index] = (self.peaks[index] * 0.94).max(value);
        }

        let params = Arc::clone(&self.shared.params);
        let status = Arc::clone(&self.shared.status);
        let palette = theme::palette(&ctx);
        let text = &i18n::t().panel;

        // Both panels are measured as they are built, because how tall the
        // window has to be is "the cards, plus whatever these two took" — and
        // only egui knows that.
        let header_height = egui::Panel::top("header")
            .frame(
                egui::Frame::default()
                    .fill(palette.surface)
                    .inner_margin(egui::Margin::symmetric(space::M as i8, space::S as i8)),
            )
            .show(ui, |ui| {
                self.header(ui, &params, &status, palette, text);
            })
            .response
            .rect
            .height();

        // The footer is where a save happens: it is the one part of the window
        // that is always on screen, so "tune, then save" never needs a scroll.
        let footer_height = egui::Panel::bottom("footer")
            .frame(
                egui::Frame::default()
                    .fill(palette.surface)
                    .inner_margin(egui::Margin::symmetric(space::M as i8, space::S as i8)),
            )
            .show(ui, |ui| {
                self.footer(ui, &status, palette, text);
            })
            .response
            .rect
            .height();

        let content = egui::CentralPanel::default()
            // Painted explicitly. egui's default central frame is transparent,
            // so the cards would sit on whatever the viewport was cleared to —
            // which is the *card* colour, leaving every card invisible except
            // for its one-pixel border. The whole light palette is built around
            // white cards on a grey field; this is what makes the field grey.
            .frame(
                egui::Frame::default()
                    .fill(palette.bg)
                    .inner_margin(egui::Margin::same(space::M as i8)),
            )
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // The equalizer takes the full width: it is the one
                        // control where a wider plot is a better control, and
                        // at 31 bands the points are only ~19 points apart even
                        // here.
                        self.eq_card(ui, &params, palette, text);
                        ui.add_space(space::S);

                        if ui.available_width() >= TWO_COLUMN_MIN_WIDTH {
                            // Four cards side by side in two columns. Stacked,
                            // they are what makes the panel want a whole
                            // screen's height; beside each other the window
                            // fits on any desktop without a scroll bar.
                            //
                            // The pairing is by priority, not height: a preset
                            // is chosen before effects are tuned, and the
                            // output is set before the meter that shows what
                            // came of it. The columns come out uneven by about
                            // one slider row, which costs nothing.
                            ui.columns(2, |columns| {
                                self.preset_card(&mut columns[0], palette, text);
                                columns[0].add_space(space::S);
                                self.effects_card(&mut columns[0], &params, palette, text);

                                self.output_card(&mut columns[1], &params, palette, text);
                                columns[1].add_space(space::S);
                                self.meter_card(&mut columns[1], palette, text);
                            });
                        } else {
                            // One column, in the same order. Everything is
                            // still reachable; it just needs a scroll.
                            self.preset_card(ui, palette, text);
                            ui.add_space(space::S);
                            self.effects_card(ui, &params, palette, text);
                            ui.add_space(space::S);
                            self.output_card(ui, &params, palette, text);
                            ui.add_space(space::S);
                            self.meter_card(ui, palette, text);
                        }
                    })
            })
            .inner;

        self.fit_to_content(&ctx, content.content_size, header_height + footer_height);

        // ~30 fps is plenty for meters, and keeps the window from spinning the
        // GPU when nothing is happening.
        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

impl PanelApp {
    /// Status on the left, the master switch on the right.
    ///
    /// The window title bar already says "FxMini", so this row does not repeat
    /// it: what a glance should answer is whether audio is being processed and
    /// whether the thing is on at all.
    fn header(
        &mut self,
        ui: &mut egui::Ui,
        params: &Arc<SharedParams>,
        status: &Arc<EngineStatus>,
        palette: &Palette,
        text: &PanelText,
    ) {
        ui.horizontal(|ui| {
            // The dot carries the hue, the word carries the meaning: colour
            // alone would be unreadable for a user who cannot tell red from
            // green, and the word is the same colour as all other text.
            let (colour, label) = if status.is_running() {
                (palette.success, text.status_processing)
            } else if status.virtual_present() {
                (palette.warning, text.status_idle)
            } else {
                (palette.danger, text.status_no_card)
            };
            theme::status_pill(ui, palette, colour, label);

            // Laid out from the right so the switch stays flush against the
            // edge however long the status word is.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut on = params.is_enabled();
                let response = theme::toggle(
                    ui,
                    palette,
                    ui.id().with("master-enable"),
                    &mut on,
                    text.enabled,
                );
                if response.changed() {
                    params.set_enabled(on);
                }
                ui.add_space(space::S);
                ui.label(RichText::new(text.enabled).color(palette.text));
            });
        });
    }

    fn preset_card(&mut self, ui: &mut egui::Ui, palette: &Palette, text: &PanelText) {
        // Read the selection from the shared status rather than from a snapshot
        // taken when the window opened, so a preset changed from the tray menu
        // shows up here as well.
        let active = self.shared.status.active_preset().map(PathBuf::from);
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

        let mut chosen: Option<PathBuf> = None;

        theme::card(ui, palette, |ui| {
            theme::card_title(ui, palette, text.preset);

            // Full width, unlike the old fixed 220 px box: the preset name is
            // the longest text in the panel and half the card was sitting empty
            // beside it.
            egui::ComboBox::from_id_salt("preset-combo")
                .selected_text(current)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for entry in &self.shared.presets {
                        let selected = active.as_ref().is_some_and(|path| path == &entry.path);
                        if ui.selectable_label(selected, &entry.name).clicked() {
                            chosen = Some(entry.path.clone());
                        }
                    }
                });
        });

        if let Some(path) = chosen {
            self.load_preset(&path);
        }
    }

    fn effects_card(
        &mut self,
        ui: &mut egui::Ui,
        params: &Arc<SharedParams>,
        palette: &Palette,
        text: &PanelText,
    ) {
        theme::card(ui, palette, |ui| {
            theme::card_title(ui, palette, text.effects);
            for (id, label) in EFFECTS {
                // The effect knobs are a 0–10 feel, not a decibel figure, so the
                // unit is left off and the decimal kept: "+3.5" and "7.0" are
                // what the presets are written in.
                let mut value = params.effect(id);
                if slider_row(
                    ui,
                    palette,
                    label(text),
                    &mut value,
                    0.0..=10.0,
                    |v| format!("{v:.1}"),
                ) {
                    params.set_effect(id, value);
                }
            }
        });
    }

    fn eq_card(
        &mut self,
        ui: &mut egui::Ui,
        params: &Arc<SharedParams>,
        palette: &Palette,
        text: &PanelText,
    ) {
        let bands = params.num_bands().min(MAX_BANDS);
        let mut chosen_bands = bands;
        let mut flatten = false;
        let mut eq_on = params.eq_on();
        let mut eq_toggled = false;

        theme::card(ui, palette, |ui| {
            // Title on the left, controls laid out from the right so the
            // heading keeps its position whatever the band count's digit count
            // is. `right_to_left` fills towards the left edge, so the widgets
            // below are added in the reverse of how they read:
            //
            //   added first ──► [归零] [10 ▾] 频段数 [●—] 启用
            //
            ui.horizontal(|ui| {
                ui.label(RichText::new(text.equalizer).strong().color(palette.text));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button(text.eq_reset).clicked() {
                        flatten = true;
                    }
                    ui.add_space(space::S);
                    egui::ComboBox::from_id_salt("band-count")
                        .selected_text(bands.to_string())
                        .width(56.0)
                        .show_ui(ui, |ui| {
                            for option in BAND_CHOICES {
                                if ui
                                    .selectable_label(option == bands, option.to_string())
                                    .clicked()
                                {
                                    chosen_bands = option;
                                }
                            }
                        });
                    ui.label(RichText::new(text.bands).color(palette.text_weak));
                    ui.add_space(space::S);
                    // Labelled, not a bare switch: an unlabelled toggle in a
                    // row with three other controls is a guess.
                    if theme::toggle(
                        ui,
                        palette,
                        ui.id().with("eq-on"),
                        &mut eq_on,
                        text.equalizer,
                    )
                    .changed()
                    {
                        eq_toggled = true;
                    }
                    ui.label(RichText::new(text.enabled).color(palette.text));
                });
            });

            ui.add_space(space::S);
            self.eq_curve(ui, params, palette, text, bands);

            ui.add_space(space::XS);
            ui.label(
                RichText::new(text.eq_hint)
                    .small()
                    .color(palette.text_weak),
            );
        });

        if eq_toggled {
            params.set_eq_on(eq_on);
        }
        if chosen_bands != bands {
            params.set_num_bands(chosen_bands);
        }
        if flatten {
            self.flatten_eq(params);
        }
    }

    /// The frequency-response curve and its draggable points.
    fn eq_curve(
        &mut self,
        ui: &mut egui::Ui,
        params: &Arc<SharedParams>,
        palette: &Palette,
        text: &PanelText,
        bands: usize,
    ) {
        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), EQ_HEIGHT),
            Sense::hover(),
        );
        // A stable id: `allocate_exact_size` would derive one from the rect,
        // and the rect changes as the window is resized, which would drop any
        // focus and restart the widget's animation.
        let response = ui.interact(rect, ui.id().with("eq-curve"), Sense::click_and_drag());

        if bands == 0 || !ui.is_rect_visible(rect) {
            return;
        }

        // The band frequencies come from the engine, not from a table here.
        // They are the grid the filters are actually on, and a 10-band preset
        // is spread over a different one than a 31-band preset; assuming
        // "31 Hz … 16 kHz" would put the dots where the filters are not.
        let freqs: Vec<f32> = (0..bands).map(|band| params.band_freq(band)).collect();
        let mut gains: Vec<f32> = (0..bands).map(|band| params.band_gain(band)).collect();
        if freqs.iter().any(|hz| *hz <= 0.0) {
            // The engine has not published the grid yet — it does that when a
            // preset loads. A curve over an invented axis would be worse than
            // an empty plot.
            return;
        }

        // The plot leaves a gutter for the dB labels and a strip under it for
        // the frequency ticks.
        let plot = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 30.0, rect.top() + 6.0),
            egui::pos2(rect.right() - 6.0, rect.bottom() - 16.0),
        );
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::same(radius::CONTROL), palette.sunken);

        // Half an octave of margin at each end, so the outermost points are not
        // sitting on the frame where they are hardest to grab.
        let f_lo = freqs[0] / SQRT_2;
        let f_hi = freqs[bands - 1] * SQRT_2;
        let log_lo = f_lo.log2();
        let log_span = (f_hi.log2() - log_lo).max(f32::EPSILON);

        let x_of = |hz: f32| plot.left() + (hz.max(1.0).log2() - log_lo) / log_span * plot.width();
        let half = plot.height() * 0.5;
        let y_of = |db: f32| plot.center().y - db / EQ_DB_RANGE * half;
        let db_of = |y: f32| (plot.center().y - y) / half * EQ_DB_RANGE;

        self.draw_graticule(&painter, plot, palette, f_lo, f_hi, &x_of, &y_of);

        // --- the curve -------------------------------------------------------

        // The x axis is logarithmic and the points are evenly spaced on it, so
        // sampling in x rather than in frequency is both correct and cheaper.
        let steps = (plot.width() / 2.0).clamp(32.0, 240.0) as usize;
        let sampled: Vec<(f32, f32)> = (0..=steps)
            .map(|step| {
                let x = plot.left() + plot.width() * step as f32 / steps as f32;
                // Invert the axis mapping to get the frequency at this x.
                let frac = (x - plot.left()) / plot.width();
                let hz = (log_lo + frac * log_span).exp2();
                (x, band_curve(&freqs, &gains, hz))
            })
            .collect();

        let zero_y = y_of(0.0);
        let fill = palette.accent.gamma_multiply(0.20);

        // The shaded area between the curve and 0 dB. Built as a triangle strip
        // rather than a polygon: the region is not convex, and egui only has a
        // convex-polygon fill, which would draw the crossing points inside out.
        if sampled.len() > 1 {
            let mut mesh = egui::Mesh::default();
            mesh.reserve_triangles(sampled.len() * 2);
            for (x, db) in &sampled {
                mesh.colored_vertex(egui::pos2(*x, y_of(*db)), fill);
                mesh.colored_vertex(egui::pos2(*x, zero_y), fill);
            }
            for step in 0..sampled.len() - 1 {
                let base = (step * 2) as u32;
                mesh.add_triangle(base, base + 1, base + 2);
                mesh.add_triangle(base + 1, base + 3, base + 2);
            }
            painter.add(egui::Shape::mesh(mesh));
        }

        let polyline: Vec<egui::Pos2> = sampled
            .iter()
            .map(|(x, db)| egui::pos2(*x, y_of(*db)))
            .collect();
        painter.add(egui::Shape::line(
            polyline,
            Stroke::new(2.0, palette.accent),
        ));

        // --- interaction -----------------------------------------------------

        let nodes: Vec<egui::Pos2> = freqs
            .iter()
            .zip(&gains)
            .map(|(hz, db)| egui::pos2(x_of(*hz), y_of(*db)))
            .collect();
        let nearest = |pos: egui::Pos2| -> Option<usize> {
            nodes
                .iter()
                .enumerate()
                .map(|(index, node)| (index, node.distance(pos)))
                .filter(|(_, distance)| *distance <= EQ_GRAB_RADIUS)
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(index, _)| index)
        };

        let hovered = response.hover_pos().and_then(nearest);

        // Double-click is checked before the drag below, because a double-click
        // is also a press and the press would otherwise set the band to where
        // the pointer is — the opposite of resetting it.
        if response.double_clicked() {
            if let Some(index) = response.interact_pointer_pos().and_then(nearest) {
                gains[index] = 0.0;
                params.set_band_gain(index, 0.0);
                self.eq_selected = Some(index);
            }
            self.eq_dragging = None;
        } else if response.is_pointer_button_down_on() {
            let pointer = response.interact_pointer_pos();
            // The grab is decided once, on the first frame of the press, so
            // that dragging past a neighbour keeps hold of the band the user
            // actually caught.
            if self.eq_dragging.is_none() {
                self.eq_dragging = pointer.and_then(nearest);
                if let Some(index) = self.eq_dragging {
                    self.eq_selected = Some(index);
                }
            }
            if let (Some(index), Some(pos)) = (self.eq_dragging, pointer) {
                let gain = snap_gain(db_of(pos.y));
                if gain != gains[index] {
                    gains[index] = gain;
                    params.set_band_gain(index, gain);
                }
            }
        } else {
            self.eq_dragging = None;
        }

        // Arrow keys, so the curve is not a keyboard dead end. The step is a
        // tenth of a decibel with shift held, which is the panel's own display
        // resolution.
        if response.has_focus() {
            if let Some(index) = self.eq_selected.filter(|index| *index < bands) {
                let step = ui.input(|i| {
                    if i.modifiers.shift {
                        EQ_KEY_STEP_FINE
                    } else {
                        EQ_KEY_STEP
                    }
                });
                let delta = ui.input(|i| {
                    let up = i.key_pressed(egui::Key::ArrowUp) as i32;
                    let down = i.key_pressed(egui::Key::ArrowDown) as i32;
                    (up - down) as f32 * step
                });
                if delta != 0.0 {
                    gains[index] = snap_gain(gains[index] + delta);
                    params.set_band_gain(index, gains[index]);
                }
            }
        }

        // --- the points ------------------------------------------------------

        let active = self.eq_dragging.or(hovered);
        for (index, node) in nodes.iter().enumerate() {
            let is_active = active == Some(index);
            let is_selected = self.eq_selected == Some(index);
            let radius = if is_active { 6.5 } else { 4.5 };
            if is_selected && !is_active {
                painter.circle_stroke(*node, radius + 3.0, Stroke::new(1.0, palette.accent));
            }
            // Filled in the accent with a surface-coloured ring, so the point
            // stays separate from the curve it sits on instead of merging.
            painter.circle_filled(*node, radius, palette.accent);
            painter.circle_stroke(
                *node,
                radius,
                Stroke::new(2.0, palette.surface),
            );
        }

        // --- the readout -----------------------------------------------------

        // Shown for whatever the pointer is nearest, so the exact figure for a
        // band is available before committing to a drag.
        if let Some(index) = active.or(self.eq_selected) {
            if index < bands {
                let label = format!(
                    "{}  {}",
                    text.frequency(freqs[index]),
                    text.gain_db(gains[index])
                );
                let size = painter.layout_no_wrap(
                    label.clone(),
                    FontId::proportional(11.5),
                    palette.text,
                );
                let pill = egui::Rect::from_min_size(
                    egui::pos2(
                        plot.right() - size.size().x - 14.0,
                        plot.top() + 4.0,
                    ),
                    size.size() + Vec2::new(12.0, 6.0),
                );
                painter.rect_filled(pill, CornerRadius::same(4), palette.surface);
                painter.rect_stroke(
                    pill,
                    CornerRadius::same(4),
                    Stroke::new(1.0, palette.border),
                    StrokeKind::Inside,
                );
                painter.text(
                    pill.center(),
                    egui::Align2::CENTER_CENTER,
                    label,
                    FontId::proportional(11.5),
                    palette.text,
                );

                // Crosshairs, so a drag is precise rather than approximate.
                painter.line_segment(
                    [
                        egui::pos2(nodes[index].x, plot.top()),
                        egui::pos2(nodes[index].x, plot.bottom()),
                    ],
                    Stroke::new(1.0, palette.border_strong),
                );
                painter.line_segment(
                    [
                        egui::pos2(plot.left(), nodes[index].y),
                        egui::pos2(plot.right(), nodes[index].y),
                    ],
                    Stroke::new(1.0, palette.border_strong),
                );
            }
        }
    }

    /// The dB labels, the frequency ticks and the lines that connect them.
    #[allow(clippy::too_many_arguments)]
    fn draw_graticule(
        &self,
        painter: &egui::Painter,
        plot: egui::Rect,
        palette: &Palette,
        f_lo: f32,
        f_hi: f32,
        x_of: &dyn Fn(f32) -> f32,
        y_of: &dyn Fn(f32) -> f32,
    ) {
        const DB_LINES: [f32; 5] = [12.0, 6.0, 0.0, -6.0, -12.0];
        for db in DB_LINES {
            let y = y_of(db);
            // 0 dB is the reference every other line is read against, so it is
            // the one that gets to be solid.
            let stroke = if db == 0.0 {
                Stroke::new(1.0, palette.border_strong)
            } else {
                Stroke::new(1.0, palette.border)
            };
            painter.line_segment(
                [egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)],
                stroke,
            );
            painter.text(
                egui::pos2(plot.left() - 5.0, y),
                egui::Align2::RIGHT_CENTER,
                format!("{}", db as i32),
                FontId::proportional(10.0),
                palette.text_faint,
            );
        }

        // Octave landmarks, thinned to at most four labels per decade so the
        // axis stays readable at 31 bands and at 5.
        const TICKS: [f32; 13] = [
            20.0, 31.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
            20000.0, 24000.0,
        ];
        let visible: Vec<f32> = TICKS
            .into_iter()
            .filter(|hz| *hz >= f_lo && *hz <= f_hi)
            .collect();
        let stride = (visible.len() / 5).max(1);
        for hz in visible.iter().step_by(stride) {
            let x = x_of(*hz);
            painter.line_segment(
                [egui::pos2(x, plot.top()), egui::pos2(x, plot.bottom())],
                Stroke::new(1.0, palette.border),
            );
            // SI prefixes are not translated: "1k" reads the same in both
            // languages and a full `1.0 kHz` under every tick would collide
            // with its neighbours.
            let label = if *hz >= 1000.0 {
                format!("{}k", hz / 1000.0)
            } else {
                format!("{hz}")
            };
            painter.text(
                egui::pos2(x, plot.bottom() + 2.0),
                egui::Align2::CENTER_TOP,
                label,
                FontId::proportional(10.0),
                palette.text_faint,
            );
        }

        // The unit for the axis, said once rather than on every label.
        painter.text(
            egui::pos2(plot.left() - 5.0, plot.top() - 2.0),
            egui::Align2::RIGHT_BOTTOM,
            DB_UNIT,
            FontId::proportional(10.0),
            palette.text_faint,
        );
    }

    fn output_card(
        &mut self,
        ui: &mut egui::Ui,
        params: &Arc<SharedParams>,
        palette: &Palette,
        text: &PanelText,
    ) {
        let gain = |v: f32| format!("{} dB", format!("{v:+.1}").replace("-0.0", "0.0"));

        theme::card(ui, palette, |ui| {
            theme::card_title(ui, palette, text.output);

            let rows: [OutputRow; 4] = [
                (text.balance, SharedParams::balance, SharedParams::set_balance, -20.0, 20.0),
                (text.master_gain, SharedParams::master_gain, SharedParams::set_master_gain, -20.0, 20.0),
                (text.normalization, SharedParams::normalization, SharedParams::set_normalization, 0.0, 4.0),
                (text.volume_leveling, SharedParams::volume_leveling, SharedParams::set_volume_leveling, 0.0, 4.0),
            ];

            for (label, get, set, min, max) in rows {
                let mut value = get(params);
                if slider_row(ui, palette, label, &mut value, min..=max, gain) {
                    set(params, value);
                }
            }

            let mut q = params.filter_q();
            if slider_row(ui, palette, text.filter_q, &mut q, 1.0..=3.0, |v| {
                format!("{v:.2}")
            }) {
                params.set_filter_q(q);
            }
        });
    }

    fn meter_card(&mut self, ui: &mut egui::Ui, palette: &Palette, text: &PanelText) {
        theme::card(ui, palette, |ui| {
            theme::card_title(ui, palette, text.spectrum);

            let (rect, _) = ui.allocate_exact_size(
                Vec2::new(ui.available_width(), 64.0),
                Sense::hover(),
            );
            let painter = ui.painter_at(rect);
            // Themed, unlike the fixed near-black this used to be — on a light
            // desktop that panel read as a hole punched in the window.
            painter.rect_filled(rect, CornerRadius::same(radius::CONTROL), palette.sunken);

            let inner = rect.shrink2(Vec2::new(6.0, 6.0));
            let count = self.spectrum.len().max(1);
            let slot = inner.width() / count as f32;
            let bar_width = (slot * 0.62).max(2.0);

            for index in 0..count {
                let value = self.spectrum[index].clamp(0.0, 1.0);
                let x = inner.left() + index as f32 * slot + (slot - bar_width) * 0.5;

                // A stub at the floor even for silence, so the meter still
                // reads as a meter rather than an empty box.
                let height = (value * inner.height()).max(2.0);
                let bar = egui::Rect::from_min_size(
                    egui::pos2(x, inner.bottom() - height),
                    Vec2::new(bar_width, height),
                );
                painter.rect_filled(bar, CornerRadius::same(2), theme::level_colour(value, palette));

                // The peak marker, drawn as a thin cap rather than a full bar
                // so it cannot be mistaken for the level itself.
                let peak = self.peaks[index].clamp(0.0, 1.0);
                if peak > value + 0.01 {
                    let y = inner.bottom() - peak * inner.height();
                    painter.rect_filled(
                        egui::Rect::from_min_size(
                            egui::pos2(x, y - 1.5),
                            Vec2::new(bar_width, 1.5),
                        ),
                        CornerRadius::same(1),
                        palette.text_faint,
                    );
                }
            }
        });
    }

    /// The save box and the runtime readout.
    fn footer(
        &mut self,
        ui: &mut egui::Ui,
        status: &Arc<EngineStatus>,
        palette: &Palette,
        text: &PanelText,
    ) {
        theme::card(ui, palette, |ui| {
            // No heading. "Preset name" in the box and "Save" on the button
            // already say what this row is, and a footer that repeats its own
            // controls is spending height the cards above it wanted.
            ui.horizontal(|ui| {
                let button_width = 64.0;
                let field_width = (ui.available_width()
                    - button_width
                    - ui.spacing().item_spacing.x)
                    .max(80.0);
                let field = ui.add_sized(
                    [field_width, 24.0],
                    egui::TextEdit::singleline(&mut self.save_name)
                        .hint_text(text.save_name_hint),
                );
                // Enter is the same as clicking Save. After tuning, the name box
                // is where the cursor already is, so it is the shorter path.
                let pressed_enter =
                    field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let button = ui.add_sized([button_width, 24.0], egui::Button::new(text.save_button));
                if button.clicked() || pressed_enter {
                    self.request_save();
                }
                button.on_hover_text(text.save_help);
            });

            // One line either way, so the footer does not change height when a
            // message appears and shove the controls above it around.
            match &self.save_feedback {
                Some(feedback) => {
                    let colour = if feedback.ok {
                        palette.success
                    } else {
                        palette.danger
                    };
                    ui.label(RichText::new(&feedback.message).small().color(colour));
                }
                None => {
                    ui.label(
                        RichText::new(text.save_where)
                            .small()
                            .color(palette.text_weak),
                    );
                }
            }

            ui.add_space(space::S);
            ui.separator();
            ui.add_space(space::XS);

            // Truncated with the full string on hover: two of these names do
            // not fit in the window at any width worth having, and wrapping
            // them turns one line into four.
            theme::elided(
                ui,
                palette,
                text.device_in_prefix,
                &status.source_description().unwrap_or_else(|| "—".to_owned()),
            );
            theme::elided(
                ui,
                palette,
                text.device_out_prefix,
                &status.sink_description().unwrap_or_else(|| "—".to_owned()),
            );

            if let Some(error) = status.last_error() {
                ui.label(
                    RichText::new(format!("⚠ {error}"))
                        .small()
                        .color(palette.danger),
                );
            }

            // The exact numbers live in the tooltip; the line itself is
            // abbreviated because the underrun counter is a frame count, and a
            // frame count is eight digits within minutes of playback.
            let summary = format!(
                "{} · {} · {}",
                text.latency(status.latency_ms()),
                text.underruns(status.underruns()),
                text.drops(status.overruns()),
            );
            ui.label(
                RichText::new(summary)
                    .small()
                    .color(palette.text_weak),
            )
            .on_hover_text(text.stats_detail(
                status.latency_ms(),
                status.underruns(),
                status.overruns(),
            ));
        });
    }
}

/// One labelled slider with its value in a fixed right-hand column.
///
/// Returns whether the value changed. The column is a pixel width rather than
/// `format!("{:<8}")` padding, which is what keeps the sliders in the two
/// languages the same length — a CJK glyph is twice as wide as a Latin one.
fn slider_row(
    ui: &mut egui::Ui,
    palette: &Palette,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    format: impl Fn(f32) -> String,
) -> bool {
    let mut changed = false;

    ui.horizontal(|ui| {
        // Truncating rather than wrapping: a label long enough to wrap would
        // make its row twice as tall as its neighbours and break the alignment
        // the fixed label column exists to create. `truncate()` also puts the
        // full text on hover, so nothing is lost.
        ui.add_sized(
            [LABEL_WIDTH, ui.spacing().interact_size.y],
            egui::Label::new(RichText::new(label).color(palette.text)).truncate(),
        );

        // The slider gets whatever is left once the value column has its width,
        // computed up front so every row's track is the same length.
        let slider_width =
            (ui.available_width() - VALUE_WIDTH - ui.spacing().item_spacing.x).max(48.0);
        ui.spacing_mut().slider_width = slider_width;

        if ui
            .add(egui::Slider::new(value, range).show_value(false))
            .changed()
        {
            changed = true;
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            theme::value(ui, palette, &format(*value));
        });
    });

    changed
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

    /// A straight line is the case monotone interpolation must reproduce
    /// exactly: if it bulges between two points on a ramp, it will bulge
    /// everywhere.
    #[test]
    fn the_curve_passes_through_its_own_points() {
        let xs = [0.0, 1.0, 2.0, 3.0];
        let ys = [0.0, 6.0, 12.0, 4.0];

        for (x, y) in xs.iter().zip(&ys) {
            let got = band_curve(&xs, &ys, *x);
            assert!(
                (got - y).abs() < 0.001,
                "at {x} the curve returned {got}, expected {y}"
            );
        }
        // On the middle segment of a straight run the interpolant is the run.
        assert!((band_curve(&xs, &ys, 0.5) - 3.0).abs() < 0.001);
    }

    /// The point of the Fritsch–Carlson limiter, and the reason this is not a
    /// plain spline: a boost either side of a deep cut is exactly the shape
    /// that overshoots, and an overshoot would draw a gain the user never set.
    #[test]
    fn the_curve_never_leaves_the_range_of_its_own_points() {
        let xs: Vec<f32> = (0..10).map(|i| i as f32).collect();
        let shapes: [[f32; 10]; 4] = [
            // The worst case for a natural spline.
            [12.0, -12.0, 12.0, -12.0, 12.0, -12.0, 12.0, -12.0, 12.0, -12.0],
            [12.0, 12.0, -12.0, -12.0, 0.0, 0.0, 12.0, 12.0, -12.0, 0.0],
            [0.0, 0.0, 0.0, 0.0, 12.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            [12.0; 10],
        ];

        for ys in shapes {
            let (lo, hi) = (
                ys.iter().copied().fold(f32::INFINITY, f32::min),
                ys.iter().copied().fold(f32::NEG_INFINITY, f32::max),
            );
            let mut at = xs[0];
            while at <= xs[9] {
                let got = band_curve(&xs, &ys, at);
                assert!(
                    got >= lo - 0.001 && got <= hi + 0.001,
                    "at {at} the curve reached {got}, outside the set range {lo}..{hi}"
                );
                at += 0.05;
            }
        }
    }

    /// Outside the points the curve holds its end values rather than running
    /// off the plot — the margins at each end of the axis are real screen.
    #[test]
    fn the_curve_holds_still_beyond_its_ends() {
        let xs = [1.0, 2.0, 3.0];
        let ys = [3.0, 6.0, 9.0];
        assert_eq!(band_curve(&xs, &ys, 0.0), 3.0);
        assert_eq!(band_curve(&xs, &ys, 4.0), 9.0);

        // A single band is a constant, not a division by zero.
        assert_eq!(band_curve(&[5.0], &[7.0], 1.0), 7.0);
        assert_eq!(band_curve(&[], &[], 1.0), 0.0);
    }

    /// The snap is what makes "this band is flat" an exact question.
    #[test]
    fn a_gain_snaps_to_the_displayed_resolution() {
        assert_eq!(snap_gain(0.0), 0.0);
        assert_eq!(snap_gain(0.04), 0.0);
        assert_eq!(snap_gain(-0.14), 0.0);
        assert_eq!(snap_gain(0.16), 0.2);
        assert_eq!(snap_gain(3.27), 3.3);
        assert_eq!(snap_gain(-3.24), -3.2);
        // And the axis is a hard bound, whatever the pointer reports.
        assert_eq!(snap_gain(40.0), EQ_DB_RANGE);
        assert_eq!(snap_gain(-40.0), -EQ_DB_RANGE);
    }

    /// The window has to be tall enough for its own content.
    ///
    /// This is the whole point of measuring rather than hardcoding a height:
    /// the cards differ in number and height between builds, and the two
    /// languages lay out at different heights because the CJK face is taller.
    #[test]
    fn the_window_is_asked_to_fit_its_content() {
        // Header 38 + footer 152, and enough cards to need scrolling at any
        // window size a person would accept.
        assert_eq!(fitted_height(760.0, 190.0, Some(1080.0)), 950.0);
    }

    /// A window taller than the monitor hides its own bottom edge.
    ///
    /// Nothing else is capped — the content decides — but this one has to be,
    /// because the footer is where saving happens and a window that reaches
    /// past the screen puts that box under the taskbar.
    #[test]
    fn a_short_screen_shortens_the_window_instead_of_the_content() {
        // A 720-point laptop screen, with content that wants 950.
        assert_eq!(fitted_height(760.0, 190.0, Some(720.0)), 720.0 - FIT_MARGIN);

        // With no monitor size reported yet, unclamped.
        assert_eq!(fitted_height(760.0, 190.0, None), 950.0);
    }

    /// A window shorter than the floor is useless even when it fits.
    #[test]
    fn the_window_never_shrinks_past_the_header_and_the_footer() {
        assert_eq!(fitted_height(0.0, 0.0, Some(1080.0)), MIN_PANEL_HEIGHT);
        // A tiny monitor: the floor still wins, so the OS clamps the window
        // instead of this code producing one too short for its own controls.
        assert_eq!(fitted_height(600.0, 190.0, Some(300.0)), MIN_PANEL_HEIGHT);
    }

    /// Holds the window open for a while, when asked to.
    ///
    /// The one thing about this panel that cannot be asserted from inside the
    /// process is what it *looks* like: whether the cards are laid out
    /// sensibly, whether a colour reads the way the palette intended, whether
    /// the window it asked for is the window a person would want. Holding it
    /// open is what makes all of that reviewable — by eye, or by a screen
    /// capture, which is how the design was checked in the first place.
    ///
    /// Diagnostic only; zero by default, so an ordinary `--ignored` run still
    /// closes the window as fast as it can.
    fn dwell_after_opening() {
        let millis = std::env::var("FXMINI_PANEL_TEST_DWELL_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        if millis > 0 {
            std::thread::sleep(Duration::from_millis(millis));
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
    ///
    /// The equalizer grid is seeded rather than left empty. A detached engine
    /// publishes no band frequencies and the curve deliberately draws nothing
    /// without them, so an unseeded panel would open with a blank plot — which
    /// is exactly what a person reviewing the design by eye (or a capture of
    /// it) needs to see least. The shape has both a boost and a cut so the area
    /// fill is exercised either side of 0 dB.
    fn shared() -> PanelShared {
        let handle = crate::engine::detached_handle();
        let params = handle.params();

        const GRID: [f32; 10] = [
            31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
        ];
        params.set_num_bands(GRID.len());
        for (band, hz) in GRID.iter().enumerate() {
            params.set_band_freq(band, *hz);
            // A shape with both a boost and a cut, so the curve's fill is drawn
            // on both sides of 0 dB.
            params.set_band_gain(band, [6.0, 4.0, 3.0, 1.0, 0.0, -1.0, -2.0, -2.0, 0.0, -4.0][band]);
        }

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

            dwell_after_opening();

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
