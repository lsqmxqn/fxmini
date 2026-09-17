//! Rounds the tuning panel's outer corners.
//!
//! Windows 11 rounds a top-level window for you. Windows 10 does not — there is
//! no `DWMWA_WINDOW_CORNER_PREFERENCE` before build 22000 — and FxMini has to
//! run on both, so on Windows 10 the corners have to be cut by hand.
//!
//! The only lever Windows 10 leaves is the window region. `SetWindowRgn` clips a
//! window to an arbitrary shape, corners included, and the compositor shows
//! whatever is behind the cut. Two consequences worth knowing before touching
//! this:
//!
//! * A region is a one-bit mask, so the corners come out stepped rather than
//!   blended. At an eight-point radius — the same one [`super::theme::radius::CARD`]
//!   gives a card — the steps are about a physical pixel, which does not read as
//!   jagged at normal viewing distance but is not the compositor's antialiasing
//!   either.
//! * The region clips the *whole* window. The panel draws its own caption now
//!   (see [`super::window_chrome`]) and the window has no decorations, so the
//!   window rectangle and the client area happen to be the same rectangle — but
//!   the region is still sized from `GetWindowRect`, which is the rectangle
//!   Windows itself measures a region against. The two agree today and would
//!   stop agreeing the day anything puts a system frame back.
//! * A maximised window is left square. Its corners are the corners of the
//!   screen, so cutting them would take four bites out of the desktop and show
//!   whatever is behind the window through them. [`RoundedWindow::apply`] clears
//!   the region for as long as the window is maximised, and cuts a fresh one
//!   when it is restored.
//!
//! On Windows 11 the DWM has already rounded the frame, so this repeats a shape
//! the compositor chose itself. It is applied on every version anyway rather
//! than behind a build-number check: one code path, and no branch that only ever
//! runs on the machine nobody is testing on. It also matters more than it used
//! to: the DWM rounds a *decorated* window by default, and stops once the
//! decorations are gone.
//!
//! The tray menu is deliberately *not* rounded. It is a Win32 popup menu drawn
//! by the system, and the one hook that could have caught its first frame
//! (`EVENT_SYSTEM_MENUPOPUPSTART`) is delivered after the menu is already up.

use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
use std::ffi::c_void;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{CreateRoundRectRgn, SetWindowRgn};
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
use windows_core::Free as _;

/// Cuts the panel window's corners off with a window region, unless the window
/// is maximised.
///
/// Stateful because `SetWindowRgn` is a one-shot call: the clip stays in force
/// until it is replaced, so the region only needs rebuilding when the window
/// changes size or when it is maximised or restored. Rebuilding it every frame
/// would allocate and hand over a GDI object thirty times a second to say the
/// same thing.
#[derive(Default)]
pub struct RoundedWindow {
    /// The panel's window, resolved on the first frame and then kept.
    hwnd: Option<HWND>,
    /// The physical size the region currently in force was cut for.
    ///
    /// `None` means no region is in force: either none has been cut yet, or the
    /// one that was has been cleared because the window is maximised.
    applied: Option<(i32, i32)>,
}

impl RoundedWindow {
    /// A shaper that will find its window on the first frame it is shown one.
    pub fn new() -> Self {
        Self::default()
    }

    /// Rounds the corners of `frame`'s window to `radius` points.
    ///
    /// `pixels_per_point` is egui's current scale; the radius is a point value
    /// and a region is measured in physical pixels, so taking the point value
    /// straight would clip the wrong amount on a scaled display.
    ///
    /// `maximized` removes the region instead of setting one, and is the only
    /// reason this is not a no-op after its first successful call.
    pub fn apply(
        &mut self,
        frame: &eframe::Frame,
        pixels_per_point: f32,
        radius: f32,
        maximized: bool,
    ) {
        let Some(window) = self.window(frame) else {
            return;
        };

        if maximized {
            if self.applied.is_some() {
                // SAFETY: a live HWND, and a null region, which is the
                // documented way to say "no region": the window reverts to its
                // own rectangle. Nothing is owned or freed by this call.
                unsafe {
                    let _ = SetWindowRgn(window, None, true);
                }
                // Forgotten so that restoring the window cuts a fresh region
                // rather than trusting one that is no longer there.
                self.applied = None;
            }
            return;
        }

        let mut rect = RECT::default();
        // SAFETY: `window` is a live HWND copied out of the frame, and `rect` is
        // a local the call is documented to fill in.
        if unsafe { GetWindowRect(window, &mut rect) }.is_err() {
            return;
        }

        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        if width <= 0 || height <= 0 || self.applied == Some((width, height)) {
            return;
        }

        let diameter = physical(radius * 2.0, pixels_per_point);

        // SAFETY: both calls take values this function owns — the same live HWND
        // as above, and a region created on the line above. `SetWindowRgn` takes
        // ownership of the region when it succeeds and leaves it to the caller
        // when it fails, so the failure arm frees it and nothing else does.
        unsafe {
            // The right and bottom edges are exclusive, so a region that is to
            // cover `width` pixels is asked for `width + 1`.
            let mut region = CreateRoundRectRgn(0, 0, width + 1, height + 1, diameter, diameter);
            if region.is_invalid() {
                return;
            }

            if SetWindowRgn(window, Some(region), true) == 0 {
                region.free();
                return;
            }
        }

        self.applied = Some((width, height));
    }

    /// The panel's window, resolved once and remembered.
    ///
    /// `Frame` is where eframe hands out the platform handle, and it is borrowed
    /// for a single frame, so the raw `HWND` is copied out the first time it is
    /// available. It stays valid for as long as the window exists, which is as
    /// long as this object does: both belong to the panel thread, and the panel
    /// is torn down with this app.
    fn window(&mut self, frame: &eframe::Frame) -> Option<HWND> {
        if self.hwnd.is_none() {
            let handle = frame.window_handle().ok()?;
            let RawWindowHandle::Win32(raw) = handle.as_raw() else {
                // Not Windows. Nothing here applies, and there is no other
                // backend to fall back to — this build only targets Windows, so
                // this arm exists for the type checker rather than for a
                // platform that could be reached.
                return None;
            };
            self.hwnd = Some(HWND(raw.hwnd.get() as *mut c_void));
        }

        self.hwnd
    }
}

/// A length in points, as whole physical pixels, never zero.
///
/// Zero is not a usable argument to either call here: a zero-size region is
/// invalid, and a zero diameter is a square corner written the slow way.
fn physical(points: f32, pixels_per_point: f32) -> i32 {
    (points * pixels_per_point).round().max(1.0) as i32
}
