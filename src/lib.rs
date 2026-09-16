//! FxMini — a tray-resident FxSound-compatible audio enhancer.
//!
//! This crate hosts the Rust side of the application. The audio DSP itself is
//! not reimplemented: it is FxSound's own engine, vendored under `vendor/dsp`
//! and compiled into a static library by `build.rs`, then reached through the
//! C ABI in `capi/dfxdsp_capi.h`.
//!
//! Module map:
//!
//! | module | job |
//! |---|---|
//! | [`ffi`] | the C ABI, plus a thin RAII wrapper over the engine |
//! | [`config`] | settings and the `%APPDATA%\FxMini` paths |
//! | [`preset`] | `.fac` parsing, the embedded library, the preset folder |
//! | [`device`] | endpoint discovery, default-device switching, hot-plug events |
//! | [`engine`] | the audio thread: loopback capture → DSP → render |
//! | [`driver`] | detecting, installing and removing the virtual sound card |
//! | [`autostart`] | the `HKCU\...\Run` entry |
//! | [`ui`] | the tray icon and the tuning panel |
//! | [`app`] | wiring between the tray, the engine and the config |
//!
//! The binaries in `src/bin` are milestone smoke tests; the shipped application
//! is `src/main.rs`.

pub mod app;
pub mod autostart;
pub mod config;
pub mod device;
pub mod driver;
pub mod engine;
pub mod ffi;
pub mod preset;
pub mod routing;
pub mod ui;

pub use config::Config;
pub use engine::{AudioEngine, EngineHandle, EngineStatus};
pub use ffi::Dsp;
pub use preset::{FacPreset, PresetEntry};
