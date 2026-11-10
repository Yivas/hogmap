//! The two fixed presentation themes. Their tokens colour painting and egui visuals only; layout
//! parameters, header heights and label thresholds never read them, so both themes produce the
//! same geometry for the same sample and viewport.

use eframe::egui::{self, Color32, CornerRadius, Shadow, Stroke};

/// The chosen theme is the only state this module persists. Its key and value format are a
/// contract with the file eframe keeps in the user's application-data directory, so the value
/// carries an explicit version: a later format can be recognised, and anything that is not an
/// exact match for the current one keeps the default instead of being half-read.
const PREFERENCE_KEY: &str = "theme";
const PREFERENCE_VERSION: &str = "v1";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

/// A fill, the label colour chosen for it and, for leaf cells, the seam drawn on its right and
/// bottom edges. Each seam is a darker shade of its own fill so it separates siblings quietly;
/// tones without a seam (directories, non-matching entries) draw none. `mark` colours the state
/// marks painted on the fill: non-matching entries have none, so a dimmed cell keeps its fill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tone {
    pub fill: Color32,
    pub text: Color32,
    pub seam: Option<Color32>,
    pub mark: Option<Color32>,
}

/// Directory header tones by nesting depth below the drawn root; deeper levels share the last.
pub const DEPTH_TONES: usize = 4;

pub struct Palette {
    pub chrome: Color32,
    pub secondary_text: Color32,
    pub separator: Color32,
    pub icon: Color32,
    pub disabled_icon: Color32,
    pub accent: Color32,
    pub tool_hover: Color32,
    pub tool_pressed: Color32,
    pub tool_selected: Color32,
    pub volume_header: Color32,
    pub volume_text: Color32,
    /// Shows between top-level cells and around the map.
    pub map_background: Color32,
    /// The waiting and empty messages drawn on `map_background`.
    pub message: Color32,
    pub directories: [Tone; DEPTH_TONES],
    pub file: Tone,
    pub aggregate: Tone,
    /// Partial, inaccessible and stale entries.
    pub incomplete: Tone,
    /// Non-matching entries while a highlight is active.
    pub dimmed: Tone,
    /// Entries whose highlight decision is unknown.
    pub undecided: Tone,
    /// Drawn above children, so it must read stronger than every seam.
    pub directory_border: Color32,
    /// Weaker than `selection` on every fill: hover is transient, selection is the state.
    pub hover: Color32,
    pub selection: Color32,
    /// Keyboard focus dashes, drawn over a solid `focus_backing` ring outside the control so the
    /// two-tone ring cannot be mistaken for a state mark or for the selection.
    pub focus: Color32,
    pub focus_backing: Color32,
}

const fn tone(fill: Color32, text: Color32, mark: Option<Color32>) -> Tone {
    Tone {
        fill,
        text,
        seam: None,
        mark,
    }
}

const fn seamed(fill: Color32, text: Color32, seam: Color32, mark: Option<Color32>) -> Tone {
    Tone {
        fill,
        text,
        seam: Some(seam),
        mark,
    }
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

const DARK_CHROME: Color32 = rgb(0x1D2025);
const DARK_TEXT: Color32 = rgb(0xDADDE2);
const DARK_STATE_MARK: Color32 = rgb(0xE9D8B4);

const DARK: Palette = Palette {
    chrome: DARK_CHROME,
    secondary_text: rgb(0xA9B0B9),
    separator: rgb(0x3C454F),
    icon: rgb(0xE6E8EB),
    disabled_icon: rgb(0x626B77),
    accent: rgb(0x7CC4FF),
    tool_hover: rgb(0x353A42),
    tool_pressed: rgb(0x414852),
    tool_selected: rgb(0x28445C),
    volume_header: rgb(0x23445B),
    volume_text: rgb(0xECF5FC),
    map_background: rgb(0x0F1114),
    message: DARK_TEXT,
    directories: [
        tone(rgb(0x4E3820), rgb(0xF5EFE6), Some(DARK_STATE_MARK)),
        tone(rgb(0x5C4327), rgb(0xF5EFE6), Some(DARK_STATE_MARK)),
        tone(rgb(0x6A4E2E), rgb(0xF5EFE6), Some(DARK_STATE_MARK)),
        tone(rgb(0x785A35), rgb(0xF7F1E8), Some(DARK_STATE_MARK)),
    ],
    // Large files cover big areas, so the fill carries the map's centre. The earlier value read
    // as background once a cell reached gigabytes.
    file: seamed(
        rgb(0x35597A),
        rgb(0xEEF3F8),
        rgb(0x24405B),
        Some(DARK_STATE_MARK),
    ),
    aggregate: seamed(
        rgb(0x4F416E),
        rgb(0xF1ECF8),
        rgb(0x342A4A),
        Some(DARK_STATE_MARK),
    ),
    incomplete: seamed(
        rgb(0x45484E),
        rgb(0xF0F1F3),
        rgb(0x2C2E33),
        Some(DARK_STATE_MARK),
    ),
    dimmed: tone(rgb(0x1F2226), rgb(0xA3AAB3), None),
    undecided: seamed(
        rgb(0x3D3826),
        rgb(0xE8E1C8),
        rgb(0x221F13),
        Some(DARK_STATE_MARK),
    ),
    directory_border: rgb(0x0F1114),
    hover: rgb(0xBCC3CC),
    selection: rgb(0xA9D6FF),
    focus: rgb(0xFFD45C),
    focus_backing: rgb(0x0F1114),
};

const LIGHT_STATE_MARK: Color32 = rgb(0x2E2418);

/// The light palette keeps the earlier fills and black labels as the regression baseline.
const LIGHT: Palette = Palette {
    chrome: rgb(0xF0F2F5),
    secondary_text: rgb(0x535D68),
    separator: rgb(0xB6BFC8),
    icon: rgb(0x263645),
    disabled_icon: rgb(0x8A929D),
    accent: rgb(0x245F88),
    tool_hover: rgb(0xDEE6ED),
    tool_pressed: rgb(0xC4D4E2),
    tool_selected: rgb(0xD3E5F4),
    volume_header: rgb(0xDCEBF6),
    volume_text: rgb(0x153A55),
    map_background: Color32::from_gray(248),
    message: Color32::BLACK,
    directories: [
        tone(rgb(0xCAA67C), Color32::BLACK, Some(LIGHT_STATE_MARK)),
        tone(rgb(0xC49F75), Color32::BLACK, Some(LIGHT_STATE_MARK)),
        tone(rgb(0xBE986E), Color32::BLACK, Some(LIGHT_STATE_MARK)),
        tone(rgb(0xB89167), Color32::BLACK, Some(LIGHT_STATE_MARK)),
    ],
    file: seamed(
        rgb(0x9DC8E5),
        Color32::BLACK,
        rgb(0x7BA6C4),
        Some(LIGHT_STATE_MARK),
    ),
    aggregate: seamed(
        rgb(0xBEA6DA),
        Color32::BLACK,
        rgb(0x9A82B8),
        Some(LIGHT_STATE_MARK),
    ),
    incomplete: seamed(
        rgb(0xA6A6A6),
        Color32::BLACK,
        rgb(0x858585),
        Some(LIGHT_STATE_MARK),
    ),
    dimmed: tone(rgb(0xDEDEDE), Color32::BLACK, None),
    undecided: seamed(
        rgb(0xBBB7AC),
        Color32::BLACK,
        rgb(0x989488),
        Some(LIGHT_STATE_MARK),
    ),
    directory_border: rgb(0x66513A),
    hover: rgb(0x474747),
    selection: rgb(0x0D3B7A),
    focus: Color32::BLACK,
    focus_backing: Color32::WHITE,
};

impl Theme {
    pub fn palette(self) -> &'static Palette {
        match self {
            Theme::Dark => &DARK,
            Theme::Light => &LIGHT,
        }
    }

    /// The theme egui is drawing with; `apply` is the only writer.
    pub fn current(context: &egui::Context) -> Self {
        match context.theme() {
            egui::Theme::Dark => Theme::Dark,
            egui::Theme::Light => Theme::Light,
        }
    }

    /// The stored form, such as `v1:dark`.
    pub fn to_stored(self) -> String {
        let name = match self {
            Theme::Dark => "dark",
            Theme::Light => "light",
        };
        format!("{PREFERENCE_VERSION}:{name}")
    }

    /// Reads exactly one of the two current values. A missing version, an unknown version or an
    /// unknown name returns `None`, so a corrupt or future value falls back to the default theme
    /// rather than guessing which theme it meant.
    pub fn from_stored(value: &str) -> Option<Self> {
        let (version, name) = value.split_once(':')?;
        if version != PREFERENCE_VERSION {
            return None;
        }
        match name {
            "dark" => Some(Theme::Dark),
            "light" => Some(Theme::Light),
            _ => None,
        }
    }

    /// The stored preference, or `None` on a fresh profile or after the value was lost or edited.
    pub fn load(storage: &dyn eframe::Storage) -> Option<Self> {
        Self::from_stored(&storage.get_string(PREFERENCE_KEY)?)
    }

    /// Writes the preference under the single key this app owns.
    pub fn store(self, storage: &mut dyn eframe::Storage) {
        storage.set_string(PREFERENCE_KEY, self.to_stored());
    }

    fn visuals(self) -> egui::Visuals {
        match self {
            Theme::Dark => dark_visuals(),
            Theme::Light => light_visuals(),
        }
    }
}

/// Installs both visual sets and selects `theme`, overriding the system preference. Call it from
/// the creation closure, before the first frame, and again before drawing after the View menu
/// changes the theme; the native window stays hidden until the first frame is painted, so that
/// frame already uses the restored choice.
pub fn apply(context: &egui::Context, theme: Theme) {
    context.set_visuals_of(egui::Theme::Dark, Theme::Dark.visuals());
    context.set_visuals_of(egui::Theme::Light, Theme::Light.visuals());
    context.set_theme(match theme {
        Theme::Dark => egui::Theme::Dark,
        Theme::Light => egui::Theme::Light,
    });
    context.all_styles_mut(|style| {
        style.animation_time = 0.1;
        style.spacing.item_spacing = egui::vec2(8.0, 4.0);
        style.spacing.button_padding = egui::vec2(8.0, 4.0);
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(13.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(13.0));
        style
            .text_styles
            .insert(egui::TextStyle::Small, egui::FontId::proportional(11.0));
        style
            .text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(18.0));
    });
}

fn dark_visuals() -> egui::Visuals {
    let mut visuals = egui::Visuals::dark();
    let raised = rgb(0x24282E);
    let edge = rgb(0x30353C);
    visuals.panel_fill = DARK_CHROME;
    visuals.window_fill = raised;
    visuals.window_stroke = Stroke::new(1.0, rgb(0x3A3F47));
    visuals.window_corner_radius = CornerRadius::same(3);
    visuals.menu_corner_radius = CornerRadius::same(3);
    visuals.window_shadow = Shadow {
        offset: [0, 4],
        blur: 12,
        spread: 0,
        color: Color32::from_black_alpha(140),
    };
    visuals.popup_shadow = Shadow {
        offset: [0, 3],
        blur: 8,
        spread: 0,
        color: Color32::from_black_alpha(140),
    };
    visuals.extreme_bg_color = rgb(0x14171B);
    visuals.faint_bg_color = rgb(0x22262B);
    visuals.code_bg_color = raised;
    visuals.weak_text_color = Some(rgb(0xA9B0B9));
    visuals.hyperlink_color = rgb(0x7CC4FF);
    visuals.error_fg_color = rgb(0xFF8A80);
    visuals.warn_fg_color = rgb(0xFFC266);
    visuals.selection.bg_fill = rgb(0x2C5A85);
    visuals.selection.stroke = Stroke::new(1.0, rgb(0xCFE6FF));
    visuals.text_cursor.stroke = Stroke::new(2.0, rgb(0xCFE6FF));

    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = DARK_CHROME;
    widgets.noninteractive.weak_bg_fill = DARK_CHROME;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, edge);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0, DARK_TEXT);
    widgets.inactive.bg_fill = rgb(0x2A2E35);
    widgets.inactive.weak_bg_fill = rgb(0x2A2E35);
    widgets.inactive.fg_stroke = Stroke::new(1.0, rgb(0xE6E8EB));
    widgets.hovered.bg_fill = rgb(0x353A42);
    widgets.hovered.weak_bg_fill = rgb(0x353A42);
    widgets.hovered.bg_stroke = Stroke::new(1.0, rgb(0x4E555F));
    widgets.hovered.fg_stroke = Stroke::new(1.5, rgb(0xF7F8FA));
    widgets.active.bg_fill = rgb(0x414852);
    widgets.active.weak_bg_fill = rgb(0x414852);
    widgets.active.bg_stroke = Stroke::new(1.0, rgb(0x7A828D));
    widgets.active.fg_stroke = Stroke::new(2.0, Color32::WHITE);
    widgets.open.bg_fill = DARK_CHROME;
    widgets.open.weak_bg_fill = rgb(0x30353C);
    widgets.open.bg_stroke = Stroke::new(1.0, rgb(0x3A3F47));
    widgets.open.fg_stroke = Stroke::new(1.0, rgb(0xE6E8EB));
    visuals
}

fn light_visuals() -> egui::Visuals {
    let mut visuals = egui::Visuals::light();
    visuals.panel_fill = LIGHT.chrome;
    visuals.window_corner_radius = CornerRadius::same(3);
    visuals.menu_corner_radius = CornerRadius::same(3);
    // egui derives hint text from translucent body text, about 3:1 on white; set an AA colour.
    visuals.weak_text_color = Some(rgb(0x5F6368));
    visuals.error_fg_color = rgb(0x961919);
    visuals
}

/// WCAG 2.x contrast ratio between two opaque sRGB colours, from 1 to 21. Only the palette tests
/// need it: every pair is fixed at compile time.
#[cfg(test)]
fn contrast_ratio(first: Color32, second: Color32) -> f32 {
    let first = relative_luminance(first);
    let second = relative_luminance(second);
    (first.max(second) + 0.05) / (first.min(second) + 0.05)
}

#[cfg(test)]
fn relative_luminance(color: Color32) -> f32 {
    let channel = |value: u8| {
        let value = f32::from(value) / 255.0;
        if value <= 0.040_45 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
}

#[cfg(test)]
mod tests {
    use eframe::Storage as _;

    use super::*;

    const THEMES: [Theme; 2] = [Theme::Dark, Theme::Light];
    const TEXT_MINIMUM: f32 = 4.5;
    const GRAPHIC_MINIMUM: f32 = 3.0;
    /// Seams only separate siblings that already differ in size and label: visible, never louder
    /// than the structure. There is no WCAG number for this; the band is the design choice.
    const SEAM_MINIMUM: f32 = 1.35;
    const SEAM_MAXIMUM: f32 = 2.0;

    /// Every marked tone uses the same state-mark colour, so one of them stands for all.
    fn state_mark(palette: &Palette) -> Color32 {
        palette
            .incomplete
            .mark
            .expect("every theme defines a state-mark colour")
    }

    fn tones(palette: &Palette) -> Vec<(&'static str, Tone)> {
        let mut tones = vec![
            ("file", palette.file),
            ("aggregate", palette.aggregate),
            ("incomplete", palette.incomplete),
            ("dimmed", palette.dimmed),
            ("undecided", palette.undecided),
        ];
        tones.extend(palette.directories.iter().map(|tone| ("directory", *tone)));
        tones
    }

    #[test]
    fn contrast_ratio_matches_wcag_reference_values() {
        assert!((contrast_ratio(Color32::BLACK, Color32::WHITE) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(Color32::WHITE, Color32::BLACK) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(rgb(0x777777), rgb(0x777777)) - 1.0).abs() < 0.001);
        // #767676 is the commonly cited lightest grey that passes 4.5:1 on white.
        assert!((contrast_ratio(rgb(0x767676), Color32::WHITE) - 4.54).abs() < 0.01);
        assert!((contrast_ratio(rgb(0x777777), Color32::WHITE) - 4.48).abs() < 0.01);
    }

    #[test]
    fn every_map_label_meets_aa_against_its_fill() {
        for theme in THEMES {
            for (name, tone) in tones(theme.palette()) {
                let ratio = contrast_ratio(tone.text, tone.fill);
                assert!(ratio >= TEXT_MINIMUM, "{theme:?} {name}: {ratio:.2}");
            }
        }
    }

    #[test]
    fn interaction_and_state_marks_stand_out_from_every_fill() {
        for theme in THEMES {
            let palette = theme.palette();
            let marks = [
                ("state mark", state_mark(palette)),
                ("hover", palette.hover),
                ("selection", palette.selection),
                ("focus", palette.focus),
            ];
            for (fill_name, tone) in tones(palette) {
                for (mark_name, mark) in marks {
                    let ratio = contrast_ratio(mark, tone.fill);
                    assert!(
                        ratio >= GRAPHIC_MINIMUM,
                        "{theme:?} {mark_name} on {fill_name}: {ratio:.2}"
                    );
                }
            }
        }
    }

    #[test]
    fn hover_reads_weaker_than_selection_on_every_fill() {
        for theme in THEMES {
            let palette = theme.palette();
            for (name, tone) in tones(palette) {
                let hover = contrast_ratio(palette.hover, tone.fill);
                let selection = contrast_ratio(palette.selection, tone.fill);
                assert!(
                    hover < selection,
                    "{theme:?} {name}: hover {hover:.2}, selection {selection:.2}"
                );
            }
        }
    }

    #[test]
    fn focus_dashes_stand_out_from_their_backing() {
        for theme in THEMES {
            let palette = theme.palette();
            let ratio = contrast_ratio(palette.focus, palette.focus_backing);
            assert!(ratio >= GRAPHIC_MINIMUM, "{theme:?}: {ratio:.2}");
        }
    }

    #[test]
    fn only_leaf_tones_that_can_match_carry_seams() {
        for theme in THEMES {
            let palette = theme.palette();
            for tone in [
                palette.file,
                palette.aggregate,
                palette.incomplete,
                palette.undecided,
            ] {
                assert!(tone.seam.is_some(), "{theme:?}: {tone:?}");
            }
            assert_eq!(palette.dimmed.seam, None, "{theme:?}");
            for tone in palette.directories {
                assert_eq!(tone.seam, None, "{theme:?}");
            }
        }
    }

    #[test]
    fn state_marks_follow_the_painted_tone() {
        for theme in THEMES {
            let palette = theme.palette();
            let mark = state_mark(palette);
            for (name, tone) in tones(palette) {
                let expected = if name == "dimmed" { None } else { Some(mark) };
                assert_eq!(tone.mark, expected, "{theme:?} {name}");
            }
        }
    }

    #[test]
    fn every_seam_is_visible_but_subtle_against_its_own_fill() {
        for theme in THEMES {
            for (name, tone) in tones(theme.palette()) {
                if let Some(seam) = tone.seam {
                    let ratio = contrast_ratio(seam, tone.fill);
                    assert!(
                        (SEAM_MINIMUM..=SEAM_MAXIMUM).contains(&ratio),
                        "{theme:?} {name}: {ratio:.2}"
                    );
                }
            }
        }
    }

    #[test]
    fn directory_borders_read_stronger_than_every_seam() {
        for theme in THEMES {
            let palette = theme.palette();
            for (name, tone) in tones(palette) {
                if let Some(seam) = tone.seam {
                    let border = contrast_ratio(palette.directory_border, tone.fill);
                    let seam = contrast_ratio(seam, tone.fill);
                    assert!(
                        border > seam,
                        "{theme:?} {name}: border {border:.2}, seam {seam:.2}"
                    );
                }
            }
        }
    }

    #[test]
    fn chrome_text_meets_aa_in_both_themes() {
        for theme in THEMES {
            let visuals = theme.visuals();
            let widgets = &visuals.widgets;
            let pairs = [
                ("text", visuals.text_color(), visuals.panel_fill),
                (
                    "hint",
                    visuals.weak_text_color(),
                    visuals.text_edit_bg_color(),
                ),
                ("weak", visuals.weak_text_color(), visuals.panel_fill),
                ("error", visuals.error_fg_color, visuals.panel_fill),
                ("menu", visuals.text_color(), visuals.window_fill),
                (
                    "button",
                    widgets.inactive.text_color(),
                    widgets.inactive.weak_bg_fill,
                ),
                (
                    "hovered button",
                    widgets.hovered.text_color(),
                    widgets.hovered.weak_bg_fill,
                ),
                (
                    "pressed button",
                    widgets.active.text_color(),
                    widgets.active.weak_bg_fill,
                ),
                (
                    "map message",
                    theme.palette().message,
                    theme.palette().map_background,
                ),
            ];
            for (name, text, background) in pairs {
                let ratio = contrast_ratio(text, background);
                assert!(ratio >= TEXT_MINIMUM, "{theme:?} {name}: {ratio:.2}");
            }
        }
    }

    #[test]
    fn desktop_chrome_tokens_keep_text_and_controls_legible() {
        for theme in THEMES {
            let palette = theme.palette();
            for (name, foreground, background, minimum) in [
                (
                    "secondary",
                    palette.secondary_text,
                    palette.chrome,
                    TEXT_MINIMUM,
                ),
                (
                    "volume",
                    palette.volume_text,
                    palette.volume_header,
                    TEXT_MINIMUM,
                ),
                (
                    "header accent",
                    palette.accent,
                    palette.volume_header,
                    GRAPHIC_MINIMUM,
                ),
                ("focus", palette.focus, palette.chrome, GRAPHIC_MINIMUM),
            ] {
                let ratio = contrast_ratio(foreground, background);
                println!("{theme:?} {name}: {ratio:.2}:1");
                assert!(ratio >= minimum, "{theme:?} {name}: {ratio:.2}");
            }
            for (name, fill) in [
                ("drive idle", palette.chrome),
                ("drive hover", palette.tool_hover),
                ("drive selected", palette.tool_selected),
            ] {
                let ratio = contrast_ratio(palette.secondary_text, fill);
                println!("{theme:?} {name}: {ratio:.2}:1");
                assert!(ratio >= TEXT_MINIMUM, "{theme:?} {name}: {ratio:.2}");
            }
            for (name, fill) in [
                ("idle", palette.chrome),
                ("hover", palette.tool_hover),
                ("pressed", palette.tool_pressed),
                ("selected", palette.tool_selected),
            ] {
                let ratio = contrast_ratio(palette.icon, fill);
                println!("{theme:?} icon {name}: {ratio:.2}:1");
                assert!(ratio >= TEXT_MINIMUM, "{theme:?} icon {name}: {ratio:.2}");
            }
        }
    }

    fn pass_with_system_theme(context: &egui::Context, system: egui::Theme) {
        let input = egui::RawInput {
            system_theme: Some(system),
            ..Default::default()
        };
        let _ = context.run_ui(input, |_| {});
    }

    #[test]
    fn the_default_dark_theme_overrides_a_light_system_preference() {
        let context = egui::Context::default();
        pass_with_system_theme(&context, egui::Theme::Light);
        // Without `apply`, egui follows the reported system theme.
        assert_eq!(context.theme(), egui::Theme::Light);
        apply(&context, Theme::default());
        pass_with_system_theme(&context, egui::Theme::Light);
        assert_eq!(Theme::current(&context), Theme::Dark);
        let visuals = &context.global_style().visuals;
        assert_eq!(visuals.panel_fill, rgb(0x1D2025));
        assert_eq!(visuals.weak_text_color, Some(rgb(0xA9B0B9)));
    }

    #[test]
    fn the_light_theme_overrides_a_dark_system_preference() {
        let context = egui::Context::default();
        apply(&context, Theme::Light);
        pass_with_system_theme(&context, egui::Theme::Dark);
        assert_eq!(Theme::current(&context), Theme::Light);
        let visuals = &context.global_style().visuals;
        // Both values exist only in the light visuals.
        assert_eq!(visuals.weak_text_color, Some(rgb(0x5F6368)));
        assert_eq!(visuals.error_fg_color, rgb(0x961919));
    }

    #[derive(Default)]
    struct MemoryStorage(std::collections::HashMap<String, String>);

    impl eframe::Storage for MemoryStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }

        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.to_owned(), value);
        }

        fn remove_string(&mut self, key: &str) {
            self.0.remove(key);
        }

        fn flush(&mut self) {}
    }

    #[test]
    fn the_stored_preference_round_trips_between_the_two_themes() {
        assert_eq!(Theme::Dark.to_stored(), "v1:dark");
        assert_eq!(Theme::Light.to_stored(), "v1:light");
        for theme in THEMES {
            assert_eq!(Theme::from_stored(&theme.to_stored()), Some(theme));
        }
    }

    #[test]
    fn unknown_or_corrupt_values_are_rejected() {
        for value in [
            "",
            "dark",
            "light",
            "v1",
            "v1:",
            "v1:Dark",
            "v1:sepia",
            "v1:dark ",
            " v1:dark",
            "v1:dark:extra",
            "1:dark",
            ":dark",
        ] {
            assert_eq!(Theme::from_stored(value), None, "{value:?}");
        }
    }

    #[test]
    fn a_future_version_is_not_read_as_the_current_one() {
        assert_eq!(Theme::from_stored("v2:dark"), None);
        assert_eq!(Theme::from_stored("v2:light"), None);
    }

    #[test]
    fn storage_holds_the_theme_under_one_key_and_survives_saving_current() {
        let mut storage = MemoryStorage::default();
        assert_eq!(Theme::load(&storage), None);

        let context = egui::Context::default();
        apply(&context, Theme::Light);
        Theme::current(&context).store(&mut storage);
        assert_eq!(storage.0.len(), 1);
        assert_eq!(
            storage.0.get(PREFERENCE_KEY).map(String::as_str),
            Some("v1:light")
        );
        assert_eq!(Theme::load(&storage), Some(Theme::Light));

        storage.set_string(PREFERENCE_KEY, "v1:sepia".to_owned());
        assert_eq!(Theme::load(&storage), None);
        assert_eq!(Theme::load(&storage).unwrap_or_default(), Theme::Dark);
    }
}
