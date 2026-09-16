//! User-visible text, in Chinese and English.
//!
//! ## Why a struct of `&'static str` and not a lookup table
//!
//! Both languages are defined as one `const` per language, typed as [`Strings`].
//! A missing translation is then a *compile* error rather than a blank label or
//! a runtime fallback, and there is no key string to typo. The cost is that
//! adding a string touches both constants — which is the point.
//!
//! ## Why the language is a process-wide atomic
//!
//! The tray menu is built on the main thread; the tuning panel renders on its
//! own thread. Both have to agree on the language, and neither owns the other.
//! One relaxed atomic read per frame is cheaper than plumbing a language
//! through every call site, and it makes a switch take effect in an already-open
//! panel on its next repaint with no message passing — the same reason
//! [`crate::engine::SharedParams`] is made of atomics.
//!
//! ## What is deliberately *not* translated
//!
//! The log file, and anything the engine or a driver API hands back. Logs stay
//! in English so that a user reporting a problem can be asked for `fxmini.log`
//! and the lines mean the same thing no matter what the UI is set to. Also
//! untranslated: `dB`, `Hz` and `kHz`, which are symbols rather than words.
//!
//! ## Adding a language
//!
//! Add a variant to [`Lang`], a `const` for its [`Strings`], and the arm in
//! [`Lang::table`]. Everything else follows from the compiler complaining.

use std::sync::atomic::{AtomicU8, Ordering};

/// A language the interface can be drawn in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

/// The config spelling that means "ask the system".
///
/// `"auto"` is deliberately not a [`Lang`]: it is an instruction, and it is
/// resolved once at startup by [`resolve`].
pub const AUTO: &str = "auto";

impl Lang {
    /// Every language, in the order the switcher lists them.
    pub const ALL: [Lang; 2] = [Lang::Zh, Lang::En];

    /// The config-file spelling.
    pub fn code(self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }

    /// A language's name *in that language*, for the switcher.
    ///
    /// Deliberately not translated: someone who has landed in the wrong
    /// language is looking for the word they recognise, and "Chinese" does not
    /// help a reader who only knows 中文.
    pub fn endonym(self) -> &'static str {
        match self {
            Lang::Zh => "中文",
            Lang::En => "English",
        }
    }

    /// Parses a config-file spelling. `None` for anything unrecognised,
    /// including `"auto"`.
    pub fn from_code(code: &str) -> Option<Self> {
        match code.trim().to_ascii_lowercase().as_str() {
            "zh" | "cn" | "zh-cn" | "zh-hans" => Some(Lang::Zh),
            "en" | "en-us" | "en-gb" => Some(Lang::En),
            _ => None,
        }
    }

    /// Whether the config asks for this language to be chosen automatically.
    pub fn is_auto(code: &str) -> bool {
        code.trim().eq_ignore_ascii_case(AUTO)
    }

    /// What the operating system is set to.
    ///
    /// `GetUserDefaultUILanguage` is the language of the *interface*, not the
    /// locale — a user in Shanghai running an English Windows wants English,
    /// and this is the API that says so. Any Chinese variant (Simplified,
    /// Traditional, either region) maps to [`Lang::Zh`]; the strings here are
    /// Simplified, which a Traditional reader can also read.
    pub fn from_system() -> Self {
        use windows::Win32::Globalization::GetUserDefaultUILanguage;

        // The low byte of a LANGID is the primary language id, where 0x04 is
        // Chinese. Masking rather than comparing whole values is what covers
        // all eight Chinese sublanguages without a lookup table.
        let langid = unsafe { GetUserDefaultUILanguage() };
        if (langid & 0x00FF) == 0x04 {
            Lang::Zh
        } else {
            Lang::En
        }
    }

    /// This language's strings.
    pub fn table(self) -> &'static Strings {
        match self {
            Lang::Zh => &ZH,
            Lang::En => &EN,
        }
    }
}

/// The process-wide language, stored as an index.
///
/// An index rather than the enum's discriminant so that inserting a variant in
/// the middle of [`Lang`] cannot silently change what a previously stored byte
/// means. `index_of`/`lang_of` are the only places that mapping lives.
static CURRENT: AtomicU8 = AtomicU8::new(0);

/// Sets the language for the whole process.
///
/// Takes effect immediately in the panel, which re-reads it every frame, and
/// after the tray menu is rebuilt — see `app::App::set_language`, which does
/// both.
pub fn set(lang: Lang) {
    CURRENT.store(index_of(lang), Ordering::Relaxed);
}

/// The current language.
pub fn current() -> Lang {
    lang_of(CURRENT.load(Ordering::Relaxed))
}

/// The current language's strings. The one call most code makes.
pub fn t() -> &'static Strings {
    current().table()
}

/// Turns a config value into a language, consulting the system for `"auto"`.
///
/// An unrecognised value is treated as `"auto"` rather than as an error: a
/// hand-edited config with a typo in it should still show *a* language, not
/// refuse to start.
pub fn resolve(config_code: &str) -> Lang {
    Lang::from_code(config_code).unwrap_or_else(Lang::from_system)
}

fn index_of(lang: Lang) -> u8 {
    match lang {
        Lang::Zh => 0,
        Lang::En => 1,
    }
}

fn lang_of(index: u8) -> Lang {
    match index {
        1 => Lang::En,
        _ => Lang::Zh,
    }
}

/// Every string the interface can show.
pub struct Strings {
    pub tray: TrayText,
    pub panel: PanelText,
}

/// The tray menu.
///
/// Kept terse on purpose: the menu is scanned rather than read, and it is drawn
/// against the screen edge where a long label gets clipped rather than wrapped.
/// The entries are single-language — the bilingual "启用音效 / Enabled" labels
/// this replaced were twice as wide as they needed to be and still only told
/// half the users what they meant.
pub struct TrayText {
    pub enabled: &'static str,
    pub route_through: &'static str,
    pub panel: &'static str,
    pub presets: &'static str,
    pub no_presets: &'static str,
    pub autostart: &'static str,
    pub install_driver: &'static str,
    pub remove_driver: &'static str,
    pub rescan: &'static str,
    pub language: &'static str,
    pub quit: &'static str,

    /// Tooltip fragments; composed by [`TrayText::tooltip_playing`].
    pub no_preset: &'static str,
    pub not_enhanced: &'static str,
    pub tooltip_idle: &'static str,
    pub tooltip_disabled: &'static str,
    pub tooltip_no_card: &'static str,
}

impl TrayText {
    /// The tray tooltip while audio is being processed, e.g.
    /// `FxMini — 音乐 — 输出未经增强（默认设备不是虚拟声卡）`.
    ///
    /// Composed here rather than in `app` so the separators and the sentence
    /// order stay next to the words they glue together.
    pub fn tooltip_playing(
        &self,
        preset: Option<&str>,
        error: Option<&str>,
        not_enhanced: bool,
    ) -> String {
        let mut text = format!("FxMini — {}", preset.unwrap_or(self.no_preset));
        if let Some(error) = error {
            text.push_str(" (");
            text.push_str(error);
            text.push(')');
        }
        if not_enhanced {
            text.push_str(" — ");
            text.push_str(self.not_enhanced);
        }
        text
    }
}

/// The tuning panel.
pub struct PanelText {
    pub enabled: &'static str,
    pub status_processing: &'static str,
    pub status_idle: &'static str,
    pub status_no_card: &'static str,

    pub preset: &'static str,
    pub no_selection: &'static str,

    pub effects: &'static str,
    pub effect_fidelity: &'static str,
    pub effect_surround: &'static str,
    pub effect_ambience: &'static str,
    pub effect_dynamic_boost: &'static str,
    pub effect_bass: &'static str,

    pub equalizer: &'static str,
    pub bands: &'static str,

    pub output: &'static str,
    pub balance: &'static str,
    pub master_gain: &'static str,
    pub normalization: &'static str,
    pub volume_leveling: &'static str,
    pub filter_q: &'static str,

    pub spectrum: &'static str,

    pub save_heading: &'static str,
    pub save_name_hint: &'static str,
    pub save_button: &'static str,
    pub save_help: &'static str,

    /// Footer counters. The number is appended by the methods below.
    pub latency_label: &'static str,
    pub underruns_label: &'static str,
    pub drops_label: &'static str,

    /// Save-result messages.
    pub saved_ok: &'static str,
    pub save_name_required: &'static str,
    pub save_name_invalid: &'static str,
    pub save_overwrote: &'static str,
    pub save_failed: &'static str,
}

impl PanelText {
    /// `in:  <device>`, or `in: —` when there is nothing to report.
    pub fn input_line(&self, description: Option<&str>) -> String {
        self.device_line("in:", description)
    }

    /// `out: <device>`, or `out: —`.
    pub fn output_line(&self, description: Option<&str>) -> String {
        self.device_line("out:", description)
    }

    /// `in:`/`out:` stay as-is in both languages: they are narrow enough not to
    /// wrap in the footer, and a device name beside them explains itself.
    fn device_line(&self, label: &str, description: Option<&str>) -> String {
        format!("{label}  {}", description.unwrap_or("—"))
    }

    pub fn latency(&self, ms: u32) -> String {
        format!("{} {ms} ms", self.latency_label)
    }

    pub fn underruns(&self, count: u64) -> String {
        format!("{} {count}", self.underruns_label)
    }

    pub fn drops(&self, count: u64) -> String {
        format!("{} {count}", self.drops_label)
    }

    /// The message shown after a save, given the file that was written and
    /// whether it replaced something.
    pub fn saved(&self, filename: &str, overwrote: bool) -> String {
        let template = if overwrote {
            self.save_overwrote
        } else {
            self.saved_ok
        };
        template.replace("{}", filename)
    }
}

/// Chinese (Simplified).
static ZH: Strings = Strings {
    tray: TrayText {
        enabled: "启用音效",
        route_through: "输出走 FxMini",
        panel: "调音面板…",
        presets: "预设",
        no_presets: "（无预设）",
        autostart: "开机自启",
        install_driver: "安装虚拟声卡…",
        remove_driver: "卸载虚拟声卡",
        rescan: "重新扫描预设",
        language: "语言",
        quit: "退出",

        no_preset: "无预设",
        not_enhanced: "输出未经增强（默认设备不是虚拟声卡）",
        tooltip_idle: "FxMini — 空闲",
        tooltip_disabled: "FxMini — 已停用",
        tooltip_no_card: "FxMini — 未安装虚拟声卡",
    },
    panel: PanelText {
        enabled: "启用",
        status_processing: "处理中",
        status_idle: "空闲",
        status_no_card: "未安装虚拟声卡",

        preset: "预设",
        no_selection: "—",

        effects: "音效",
        effect_fidelity: "保真度",
        effect_surround: "环绕",
        effect_ambience: "空间感",
        effect_dynamic_boost: "动态增强",
        effect_bass: "低音",

        equalizer: "均衡器",
        bands: "频段数",

        output: "输出",
        balance: "左右平衡",
        master_gain: "总增益",
        normalization: "响度归一",
        volume_leveling: "音量均衡",
        filter_q: "滤波器 Q 值",

        spectrum: "频谱",

        save_heading: "保存为预设",
        save_name_hint: "预设名称",
        save_button: "保存",
        save_help: "把当前的音效、均衡与输出设置存成一个 .fac 文件，之后可在托盘菜单或上方列表中选用。",
        latency_label: "延迟",
        underruns_label: "欠载",
        drops_label: "丢帧",

        saved_ok: "已保存 {}",
        save_name_required: "请先填写预设名称",
        save_name_invalid: "名称含文件名不允许的字符（\\ / : * ? \" < > |），或与系统保留名重名",
        save_overwrote: "已覆盖 {}",
        save_failed: "保存失败，详见日志",
    },
};

/// English.
static EN: Strings = Strings {
    tray: TrayText {
        enabled: "Enabled",
        route_through: "Route through FxMini",
        panel: "Tuning panel…",
        presets: "Presets",
        no_presets: "(none found)",
        autostart: "Start with Windows",
        install_driver: "Install sound card…",
        remove_driver: "Remove sound card",
        rescan: "Rescan presets",
        language: "Language",
        quit: "Quit",

        no_preset: "no preset",
        not_enhanced: "output is NOT enhanced (default device is not the virtual card)",
        tooltip_idle: "FxMini — idle",
        tooltip_disabled: "FxMini — disabled",
        tooltip_no_card: "FxMini — virtual sound card not installed",
    },
    panel: PanelText {
        enabled: "Enabled",
        status_processing: "processing",
        status_idle: "idle",
        status_no_card: "no virtual sound card",

        preset: "Preset",
        no_selection: "—",

        effects: "Effects",
        effect_fidelity: "Fidelity",
        effect_surround: "Surround",
        effect_ambience: "Ambience",
        effect_dynamic_boost: "Dynamic boost",
        effect_bass: "Bass",

        equalizer: "Equalizer",
        bands: "Bands",

        output: "Output",
        balance: "Balance",
        master_gain: "Master gain",
        normalization: "Normalization",
        volume_leveling: "Volume leveling",
        filter_q: "Filter Q",

        spectrum: "Spectrum",

        save_heading: "Save as preset",
        save_name_hint: "Preset name",
        save_button: "Save",
        save_help: "Stores the current effects, equalizer and output settings as a .fac file, \
                    selectable from the tray menu or the list above.",
        latency_label: "latency",
        underruns_label: "underruns",
        drops_label: "drops",

        saved_ok: "Saved {}",
        save_name_required: "Give the preset a name first",
        save_name_invalid: "The name has a character Windows forbids in a filename \
                            (\\ / : * ? \" < > |), or collides with a reserved device name",
        save_overwrote: "Overwrote {}",
        save_failed: "Could not save; see the log",
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_codes_round_trip() {
        for lang in Lang::ALL {
            assert_eq!(Lang::from_code(lang.code()), Some(lang));
        }
    }

    #[test]
    fn aliases_and_junk() {
        assert_eq!(Lang::from_code("ZH-CN"), Some(Lang::Zh));
        assert_eq!(Lang::from_code(" en "), Some(Lang::En));
        // `auto` must not resolve to a language here — that is `resolve`'s job.
        assert_eq!(Lang::from_code(AUTO), None);
        assert!(Lang::is_auto(AUTO));
        assert!(Lang::is_auto(" Auto "));
        assert!(!Lang::is_auto("zh"));
    }

    #[test]
    fn every_string_is_present_in_both_languages() {
        // The types make a *missing* field impossible; this catches the other
        // mistake, a field left as an empty string because a translation was
        // forgotten.
        for lang in Lang::ALL {
            let strings = lang.table();
            let tray = &strings.tray;
            for (name, value) in [
                ("enabled", tray.enabled),
                ("route_through", tray.route_through),
                ("panel", tray.panel),
                ("presets", tray.presets),
                ("no_presets", tray.no_presets),
                ("autostart", tray.autostart),
                ("install_driver", tray.install_driver),
                ("remove_driver", tray.remove_driver),
                ("rescan", tray.rescan),
                ("language", tray.language),
                ("quit", tray.quit),
                ("no_preset", tray.no_preset),
                ("not_enhanced", tray.not_enhanced),
                ("tooltip_idle", tray.tooltip_idle),
                ("tooltip_disabled", tray.tooltip_disabled),
                ("tooltip_no_card", tray.tooltip_no_card),
            ] {
                assert!(!value.trim().is_empty(), "{} tray.{name} is empty", lang.code());
            }

            let panel = &strings.panel;
            for (name, value) in [
                ("enabled", panel.enabled),
                ("status_processing", panel.status_processing),
                ("status_idle", panel.status_idle),
                ("status_no_card", panel.status_no_card),
                ("preset", panel.preset),
                ("no_selection", panel.no_selection),
                ("effects", panel.effects),
                ("effect_fidelity", panel.effect_fidelity),
                ("effect_surround", panel.effect_surround),
                ("effect_ambience", panel.effect_ambience),
                ("effect_dynamic_boost", panel.effect_dynamic_boost),
                ("effect_bass", panel.effect_bass),
                ("equalizer", panel.equalizer),
                ("bands", panel.bands),
                ("output", panel.output),
                ("balance", panel.balance),
                ("master_gain", panel.master_gain),
                ("normalization", panel.normalization),
                ("volume_leveling", panel.volume_leveling),
                ("filter_q", panel.filter_q),
                ("spectrum", panel.spectrum),
                ("save_heading", panel.save_heading),
                ("save_name_hint", panel.save_name_hint),
                ("save_button", panel.save_button),
                ("save_help", panel.save_help),
                ("latency_label", panel.latency_label),
                ("underruns_label", panel.underruns_label),
                ("drops_label", panel.drops_label),
                ("saved_ok", panel.saved_ok),
                ("save_name_required", panel.save_name_required),
                ("save_name_invalid", panel.save_name_invalid),
                ("save_overwrote", panel.save_overwrote),
                ("save_failed", panel.save_failed),
            ] {
                assert!(!value.trim().is_empty(), "{} panel.{name} is empty", lang.code());
            }
        }
    }

    #[test]
    fn the_save_messages_actually_interpolate() {
        // `PanelText::saved` substitutes by hand because a format template
        // cannot be a runtime value, so a template that lost its `{}` would
        // silently drop the filename.
        for lang in Lang::ALL {
            let panel = &lang.table().panel;
            for template in [panel.saved_ok, panel.save_overwrote] {
                assert!(
                    template.contains("{}"),
                    "{}: {template:?} has no placeholder",
                    lang.code()
                );
            }
            assert!(panel.saved("x.fac", false).contains("x.fac"));
            assert!(panel.saved("x.fac", true).contains("x.fac"));
        }
    }

    #[test]
    fn selecting_a_language_is_visible_through_t() {
        // Global state, so restore whatever was there: other tests in this
        // binary share the process.
        let before = current();
        set(Lang::En);
        assert_eq!(current(), Lang::En);
        assert_eq!(t().tray.quit, "Quit");
        set(Lang::Zh);
        assert_eq!(current(), Lang::Zh);
        assert_eq!(t().tray.quit, "退出");
        set(before);
    }

    #[test]
    fn tooltip_composition_covers_all_four_cases() {
        let text = &ZH.tray;
        assert_eq!(
            text.tooltip_playing(Some("音乐"), None, false),
            "FxMini — 音乐"
        );
        assert_eq!(
            text.tooltip_playing(None, None, false),
            "FxMini — 无预设"
        );
        assert_eq!(
            text.tooltip_playing(None, Some("boom"), false),
            "FxMini — 无预设 (boom)"
        );
        assert_eq!(
            text.tooltip_playing(Some("音乐"), None, true),
            "FxMini — 音乐 — 输出未经增强（默认设备不是虚拟声卡）"
        );
    }

    #[test]
    fn device_lines_fall_back_to_a_dash() {
        assert_eq!(ZH.panel.input_line(None), "in:  —");
        assert_eq!(EN.panel.output_line(Some("Speakers")), "out:  Speakers");
        assert_eq!(ZH.panel.latency(120), "延迟 120 ms");
        assert_eq!(EN.panel.underruns(3), "underruns 3");
    }
}
