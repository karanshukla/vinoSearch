//! Tokens, fonts and egui's visuals, and what the desktop asks for.
//!
//! The colours are vinopilot.ca's site.css plus states (vinoSearch#148):
//! green only ever means "this one" (on, selected, done), amber means "needs
//! you" and red means "failed". Text is 4.5:1 and the edges of controls 3:1
//! against both the page and a field, in both themes; `tests` checks that
//! over the visuals as installed.

use std::sync::Arc;

use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Shadow, Stroke,
    TextStyle, Theme, Visuals,
};

/// One theme's colours.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    /// The page.
    pub bg: Color32,
    /// Groups, cards, list rows, the sidebar and headers.
    pub surface: Color32,
    /// Text fields, combo boxes, secondary buttons, menus.
    pub field: Color32,
    /// A pressed control; the track of a switch that is off.
    pub pressed: Color32,
    /// Hairlines between rows and around groups.
    pub rule: Color32,
    /// The outline of a button or a menu.
    pub rule_strong: Color32,
    /// The outline of a field or an unchecked box, and the rails and handles
    /// of sliders and scroll bars: 3:1 against the page and a field, as WCAG
    /// asks of the edges that make a control findable.
    pub edge: Color32,
    pub text: Color32,
    pub muted: Color32,
    /// Links, the selected nav item, focus rings, a switch that is on.
    pub accent: Color32,
    /// The primary button's fill and the switch's "on" track.
    pub primary_fill: Color32,
    /// Text on `primary_fill`.
    pub primary_text: Color32,
    /// The knob of a switch that is on.
    pub switch_knob: Color32,
    /// A selected row, nav item or chip: the accent at 10–14 %.
    pub tint: Color32,
    pub warn: Color32,
    pub warn_tint: Color32,
    pub error: Color32,
    pub error_tint: Color32,
    /// The index's kinds, for share bars: text, documents, images, code.
    pub kinds: [Color32; 4],
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

// Premultiplied, as Color32 stores it; the hex is the colour at full opacity.
const fn rgba(hex: u32, alpha: u8) -> Color32 {
    const fn channel(c: u32, alpha: u8) -> u8 {
        (c * alpha as u32 / 255) as u8
    }
    Color32::from_rgba_premultiplied(
        channel((hex >> 16) & 0xff, alpha),
        channel((hex >> 8) & 0xff, alpha),
        channel(hex & 0xff, alpha),
        alpha,
    )
}

pub const DARK: Palette = Palette {
    bg: rgb(0x121315),
    surface: rgb(0x17181b),
    field: rgb(0x1e2023),
    pressed: rgb(0x26282c),
    rule: rgb(0x2a2c30),
    rule_strong: rgb(0x3d4046),
    edge: rgb(0x6b6f77),
    text: rgb(0xe4e5e8),
    muted: rgb(0x9a9ea6),
    accent: rgb(0x7bdc97),
    primary_fill: rgb(0x56cc78),
    primary_text: rgb(0x0e0e10),
    switch_knob: rgb(0x0e0e10),
    tint: rgba(0x56cc78, 36),
    warn: rgb(0xf2b65c),
    warn_tint: rgba(0xf2b65c, 31),
    error: rgb(0xf28b82),
    error_tint: rgba(0xf28b82, 31),
    kinds: [rgb(0x56cc78), rgb(0x6ea8fe), rgb(0xb38cf0), rgb(0x9a9ea6)],
};

pub const LIGHT: Palette = Palette {
    bg: rgb(0xfcfcfc),
    surface: rgb(0xffffff),
    field: rgb(0xeef0f2),
    pressed: rgb(0xe3e5e8),
    rule: rgb(0xe2e3e6),
    rule_strong: rgb(0xc9cbd0),
    edge: rgb(0x7f838b),
    text: rgb(0x1c1d20),
    muted: rgb(0x5d6068),
    accent: rgb(0x1a7040),
    primary_fill: rgb(0x0e0e10),
    primary_text: rgb(0xfafafa),
    switch_knob: rgb(0xffffff),
    tint: rgba(0x1a7040, 26),
    warn: rgb(0x8a5300),
    warn_tint: rgba(0xb26e00, 26),
    error: rgb(0xb3261e),
    error_tint: rgba(0xb3261e, 20),
    kinds: [rgb(0x1f8a4c), rgb(0x2f6fdb), rgb(0x7b4fd0), rgb(0x6b6f77)],
};

impl Palette {
    /// A hovered control: halfway from `field` to `pressed`.
    pub fn hovered(&self) -> Color32 {
        self.field.lerp_to_gamma(self.pressed, 0.5)
    }

    /// The colour of the shadow under a menu or popup, the only shadowed
    /// surface: 0·12·32 at 45 % (dark) or 16 % (light).
    pub fn shadow(&self) -> Shadow {
        let color = if self.bg == DARK.bg {
            Color32::from_black_alpha(115)
        } else {
            Color32::from_rgba_unmultiplied(20, 20, 30, 41)
        };
        Shadow {
            offset: [0, 12],
            blur: 32,
            spread: 0,
            color,
        }
    }
}

pub fn palette(ctx: &egui::Context) -> &'static Palette {
    match ctx.theme() {
        Theme::Dark => &DARK,
        Theme::Light => &LIGHT,
    }
}

/// Inter at 500: row titles, button text.
pub fn medium() -> FontFamily {
    FontFamily::Name("Inter-Medium".into())
}

/// Inter at 600: window titles, group labels, selected nav items, key caps.
pub fn semibold() -> FontFamily {
    FontFamily::Name("Inter-SemiBold".into())
}

/// Inter at 700: page titles and stats.
pub fn bold() -> FontFamily {
    FontFamily::Name("Inter-Bold".into())
}

/// JetBrains Mono: paths and commands only.
pub fn mono() -> FontFamily {
    FontFamily::Monospace
}

fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    for (name, bytes) in [
        (
            "Inter-Regular",
            &include_bytes!("../assets/fonts/Inter-Regular.ttf")[..],
        ),
        (
            "Inter-Medium",
            &include_bytes!("../assets/fonts/Inter-Medium.ttf")[..],
        ),
        (
            "Inter-SemiBold",
            &include_bytes!("../assets/fonts/Inter-SemiBold.ttf")[..],
        ),
        (
            "Inter-Bold",
            &include_bytes!("../assets/fonts/Inter-Bold.ttf")[..],
        ),
        (
            "JetBrainsMono-Regular",
            &include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf")[..],
        ),
    ] {
        fonts
            .font_data
            .insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    }
    // The stock fonts stay behind ours for the glyphs the Latin subsets lack.
    let stock: Vec<String> = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let family = |first: &str| {
        let mut list = vec![first.to_owned()];
        if first != "Inter-Regular" {
            list.push("Inter-Regular".to_owned());
        }
        list.extend(stock.iter().cloned());
        list
    };
    fonts
        .families
        .insert(FontFamily::Proportional, family("Inter-Regular"));
    fonts.families.insert(medium(), family("Inter-Medium"));
    fonts.families.insert(semibold(), family("Inter-SemiBold"));
    fonts.families.insert(bold(), family("Inter-Bold"));
    let mono = fonts.families.entry(FontFamily::Monospace).or_default();
    mono.insert(0, "JetBrainsMono-Regular".to_owned());
    mono.push("Inter-Regular".to_owned());
    fonts
}

fn visuals(p: &Palette, base: Visuals) -> Visuals {
    let corner = CornerRadius::same(8);
    let mut v = base;
    v.panel_fill = p.bg;
    // Menus and popups: a field-coloured surface with a strong rule, and the
    // only shadow anywhere.
    v.window_fill = p.field;
    v.window_stroke = Stroke::new(1.0_f32, p.rule_strong);
    v.window_shadow = p.shadow();
    v.popup_shadow = p.shadow();
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_corner_radius = CornerRadius::same(10);
    v.extreme_bg_color = p.field;
    v.faint_bg_color = p.field;
    v.code_bg_color = p.field;
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.error;
    v.selection.bg_fill = p.tint;
    v.selection.stroke = Stroke::new(1.0_f32, p.accent);
    v.text_cursor.stroke.color = p.text;

    v.widgets.noninteractive.bg_fill = p.bg;
    v.widgets.noninteractive.weak_bg_fill = p.surface;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.rule);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, p.text);
    v.widgets.noninteractive.corner_radius = corner;
    for widget in [
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        widget.corner_radius = corner;
        widget.fg_stroke = Stroke::new(1.0_f32, p.text);
    }
    // A field's outline, and an unchecked box's, has to show against the page.
    v.widgets.inactive.bg_fill = p.field;
    v.widgets.inactive.weak_bg_fill = p.field;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, p.edge);
    v.widgets.hovered.bg_fill = p.hovered();
    v.widgets.hovered.weak_bg_fill = p.hovered();
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.edge);
    v.widgets.hovered.expansion = 0.0;
    // Pressed, and focused from the keyboard: a 2 px accent ring.
    v.widgets.active.bg_fill = p.pressed;
    v.widgets.active.weak_bg_fill = p.pressed;
    v.widgets.active.bg_stroke = Stroke::new(2.0_f32, p.accent);
    v.widgets.active.expansion = 0.0;
    v.widgets.open.bg_fill = p.pressed;
    v.widgets.open.weak_bg_fill = p.pressed;
    v.widgets.open.bg_stroke = Stroke::new(1.0_f32, p.accent);
    v
}

/// A solid scroll bar paints its handle in `bg_fill` on an `extreme_bg_color`
/// track, and the visuals above make both the field colour, so the handle
/// vanished. Nothing else here uses `bg_fill` (buttons take `weak_bg_fill`),
/// so give it the edge grey, lighter while hovered and green while dragged.
/// Checkboxes and slider rails paint in `bg_fill` too, so they take the same
/// grey.
pub fn visible_scroll_handles(ctx: &egui::Context) {
    for (theme, p) in [(Theme::Dark, &DARK), (Theme::Light, &LIGHT)] {
        ctx.style_mut_of(theme, |style| {
            let w = &mut style.visuals.widgets;
            w.inactive.bg_fill = p.edge;
            w.hovered.bg_fill = p.muted;
            w.active.bg_fill = p.accent;
        });
    }
}

/// Fonts, colours, sizes and spacing, without asking the desktop anything.
pub fn apply(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    ctx.set_visuals_of(Theme::Dark, visuals(&DARK, Visuals::dark()));
    ctx.set_visuals_of(Theme::Light, visuals(&LIGHT, Visuals::light()));
    ctx.all_styles_mut(|style| {
        style
            .text_styles
            .insert(TextStyle::Body, FontId::proportional(14.0));
        style
            .text_styles
            .insert(TextStyle::Button, FontId::new(13.5, medium()));
        style
            .text_styles
            .insert(TextStyle::Small, FontId::proportional(12.5));
        style
            .text_styles
            .insert(TextStyle::Heading, FontId::new(22.0, bold()));
        style
            .text_styles
            .insert(TextStyle::Monospace, FontId::monospace(12.5));
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        style.spacing.interact_size.y = 30.0;
        style.spacing.menu_margin = egui::Margin::same(4);
        style.spacing.combo_width = 200.0;
        style.spacing.scroll = egui::style::ScrollStyle {
            bar_width: 10.0,
            ..egui::style::ScrollStyle::solid()
        };
    });
    visible_scroll_handles(ctx);
}

/// `apply`, then the desktop's theme and motion preference.
pub fn install(ctx: &egui::Context) {
    apply(ctx);
    follow_desktop(ctx);
}

// What the desktop asks for.

/// One value from the portal's Settings, as busctl prints it ("v u 1").
fn read_setting(namespace: &str, key: &str) -> Option<String> {
    let output = std::process::Command::new("busctl")
        .args([
            "--user",
            "call",
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Settings",
            "ReadOne",
            "ss",
            namespace,
            key,
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Light or dark. winit on Wayland only knows the theme of decorations it
/// draws itself, so egui would fall back to dark on a light desktop: ask the
/// portal instead. `VINO_UI_THEME=light|dark` overrides it, for screenshots
/// on a desktop without a portal.
pub fn desktop_preference() -> Option<Theme> {
    match std::env::var("VINO_UI_THEME").as_deref() {
        Ok("light") => return Some(Theme::Light),
        Ok("dark") => return Some(Theme::Dark),
        _ => {}
    }
    parse_color_scheme(&read_setting("org.freedesktop.appearance", "color-scheme")?)
}

// 1 prefers dark, 2 prefers light, 0 is no preference, which desktops answer
// for their light default.
fn parse_color_scheme(reply: &str) -> Option<Theme> {
    match reply.split_whitespace().last()?.parse::<u32>().ok()? {
        1 => Some(Theme::Dark),
        _ => Some(Theme::Light),
    }
}

/// Whether the desktop asks for less motion: GNOME's "Reduce animation", or
/// Plasma's animation speed slid to instant.
pub fn reduced_motion() -> bool {
    let gnome = read_setting("org.gnome.desktop.interface", "enable-animations");
    let kde = read_setting("org.kde.kdeglobals.KDE", "AnimationDurationFactor");
    gnome.as_deref().is_some_and(animations_off) || kde.as_deref().is_some_and(animations_off)
}

// "v b false", or a duration factor of "v d 0".
fn animations_off(reply: &str) -> bool {
    match reply.split_whitespace().last() {
        Some("false") => true,
        Some(n) => n.parse::<f64>().is_ok_and(|f| f == 0.0),
        None => false,
    }
}

/// No animation time is the signal: egui's own transitions go instant and
/// [`crate::widgets::spinner`] stops turning.
pub fn still(ctx: &egui::Context) -> bool {
    ctx.global_style().animation_time == 0.0
}

/// The desktop's theme and motion preference, applied. Call again when the
/// window regains focus: both may have changed in the background.
pub fn follow_desktop(ctx: &egui::Context) {
    if let Some(theme) = desktop_preference() {
        ctx.set_theme(theme);
    }
    let animation_time = if reduced_motion() {
        0.0
    } else {
        egui::Style::default().animation_time
    };
    ctx.all_styles_mut(|style| style.animation_time = animation_time);
}

// egui sizes text from its own style times the display scale, so GNOME's
// Large Text and Plasma's font settings never reached it. Applied as zoom,
// the same thing Ctrl + does, so the layout is the one already tested at 2x.
pub const TEXT_SCALE: std::ops::RangeInclusive<f32> = 1.0..=3.0;

/// The desktop's text scaling, clamped to `TEXT_SCALE`; None when it doesn't
/// say. Plasma has two knobs, a forced font DPI (kcmfontsrc) and the general
/// font's point size (kdeglobals); GNOME answers through the portal, or
/// gsettings when no portal runs.
pub fn desktop_text_scale() -> Option<f32> {
    let factor = if on_kde() {
        let config = config_dir()?;
        let read = |name: &str| std::fs::read_to_string(config.join(name)).unwrap_or_default();
        kde_text_scale(&read("kcmfontsrc"), &read("kdeglobals"))
    } else {
        gnome_text_scale()
    }?;
    Some(factor.clamp(*TEXT_SCALE.start(), *TEXT_SCALE.end()))
}

/// `desktop_text_scale`, or 1 when the desktop doesn't say.
pub fn text_scale() -> f32 {
    desktop_text_scale().unwrap_or(1.0)
}

fn gnome_text_scale() -> Option<f32> {
    if let Some(reply) = read_setting("org.gnome.desktop.interface", "text-scaling-factor") {
        return parse_scale(&reply);
    }
    let output = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "text-scaling-factor"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_scale(&String::from_utf8_lossy(&output.stdout))
}

// "v d 1.25", or gsettings' "1.25".
fn parse_scale(reply: &str) -> Option<f32> {
    let factor = reply.split_whitespace().last()?.parse::<f32>().ok()?;
    (factor.is_finite() && factor > 0.0).then_some(factor)
}

// A forced DPI wins, as it does in Qt; 96 is 1x and 10 pt is Breeze's default.
fn kde_text_scale(kcmfontsrc: &str, kdeglobals: &str) -> Option<f32> {
    let dpi = ini_value(kcmfontsrc, "[General]", "forceFontDPI")
        .and_then(|dpi| dpi.parse::<f32>().ok())
        .filter(|dpi| *dpi > 0.0);
    if let Some(dpi) = dpi {
        return Some(dpi / 96.0);
    }
    kde_font_scale(&ini_value(kdeglobals, "[General]", "font")?)
}

// A Qt font string, "Noto Sans,12,-1,5,50,0,0,0,0,0": family, then point size.
fn kde_font_scale(font: &str) -> Option<f32> {
    let points = font
        .trim_matches('"')
        .split(',')
        .nth(1)?
        .trim()
        .parse::<f32>()
        .ok()?;
    (points > 0.0).then_some(points / 10.0)
}

fn ini_value(file: &str, group: &str, key: &str) -> Option<String> {
    let mut inside = false;
    for line in file.lines().map(str::trim) {
        if line.starts_with('[') {
            inside = line == group;
        } else if inside
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            return Some(v.trim().to_owned());
        }
    }
    None
}

fn on_kde() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .any(|name| name.eq_ignore_ascii_case("KDE"))
}

fn config_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".config"))
        })
}

/// The colours of a window's top strip.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Header {
    pub bg: Color32,
    pub text: Color32,
    pub muted: Color32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KdeHeader {
    pub active: Header,
    pub inactive: Header,
}

/// Breeze paints the title bar in the colour scheme's Header colours, so a
/// header painted in them runs into it, as Dolphin's toolbar does. KDE only:
/// elsewhere the file means nothing.
pub fn kde_header() -> Option<KdeHeader> {
    if !on_kde() {
        return None;
    }
    parse_kde_header(&std::fs::read_to_string(config_dir()?.join("kdeglobals")).ok()?)
}

fn parse_kde_header(kdeglobals: &str) -> Option<KdeHeader> {
    let group = |name: &str| -> Option<Header> {
        let mut inside = false;
        let (mut bg, mut text, mut muted) = (None, None, None);
        for line in kdeglobals.lines().map(str::trim) {
            if line.starts_with('[') {
                inside = line == name;
                continue;
            }
            if !inside {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let slot = match key.trim() {
                "BackgroundNormal" => &mut bg,
                "ForegroundNormal" => &mut text,
                "ForegroundInactive" => &mut muted,
                _ => continue,
            };
            *slot = parse_rgb(value);
        }
        Some(Header {
            bg: bg?,
            text: text?,
            muted: muted?,
        })
    };
    let active = group("[Colors:Header]")?;
    let inactive = group("[Colors:Header][Inactive]").unwrap_or(active);
    Some(KdeHeader { active, inactive })
}

fn parse_rgb(value: &str) -> Option<Color32> {
    let mut parts = value.split(',').map(|part| part.trim().parse::<u8>().ok());
    let color = Color32::from_rgb(parts.next()??, parts.next()??, parts.next()??);
    Some(color)
}

/// The header's colours: KDE's when it has them, else the surface.
pub fn header(ctx: &egui::Context, kde: Option<&KdeHeader>, focused: bool) -> Header {
    match kde {
        Some(kde) if focused => kde.active,
        Some(kde) => kde.inactive,
        None => {
            let p = palette(ctx);
            Header {
                bg: p.surface,
                text: p.text,
                muted: p.muted,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_portal_reply_picks_the_theme() {
        assert_eq!(parse_color_scheme("v u 1\n"), Some(Theme::Dark));
        assert_eq!(parse_color_scheme("v u 2\n"), Some(Theme::Light));
        assert_eq!(parse_color_scheme("v u 0\n"), Some(Theme::Light));
        assert_eq!(parse_color_scheme(""), None);
        assert_eq!(parse_color_scheme("garbage"), None);
    }

    #[test]
    fn text_scale_reads_both_desktops() {
        assert_eq!(parse_scale("v d 1.25\n"), Some(1.25));
        assert_eq!(parse_scale("1.5\n"), Some(1.5));
        assert_eq!(parse_scale("v d nan\n"), None);
        assert_eq!(parse_scale(""), None);
        let font = |size: &str| format!("[General]\nfont=Noto Sans,{size},-1,5,50,0,0,0,0,0\n");
        assert_eq!(kde_text_scale("", &font("10")), Some(1.0));
        assert_eq!(kde_text_scale("", &font("15")), Some(1.5));
        assert_eq!(kde_text_scale("", &font("0")), None);
        assert_eq!(kde_text_scale("", "[General]\nfont=Noto Sans\n"), None);
        assert_eq!(
            kde_text_scale("[General]\nforceFontDPI=144\n", &font("10")),
            Some(1.5),
            "a forced DPI wins"
        );
        assert_eq!(
            kde_text_scale("[General]\nforceFontDPI=0\n", &font("12")),
            Some(1.2)
        );
    }

    #[test]
    fn reduced_motion_reads_both_desktops() {
        assert!(animations_off("v b false\n"));
        assert!(!animations_off("v b true\n"));
        assert!(animations_off("v d 0\n"));
        assert!(!animations_off("v d 1\n"));
        assert!(!animations_off(""));
    }

    /// WCAG 2.x relative luminance of an sRGB colour.
    fn luminance(c: Color32) -> f64 {
        let channel = |v: u8| {
            let v = f64::from(v) / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
    }

    pub fn contrast(a: Color32, b: Color32) -> f64 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// A translucent tint as it lands on `over`.
    fn flatten(tint: Color32, over: Color32) -> Color32 {
        let a = f64::from(tint.a()) / 255.0;
        let mix = |t: u8, o: u8| (f64::from(t) * a + f64::from(o) * (1.0 - a)).round() as u8;
        Color32::from_rgb(
            mix(tint.r(), over.r()),
            mix(tint.g(), over.g()),
            mix(tint.b(), over.b()),
        )
    }

    #[test]
    fn contrast_ratio_matches_the_wcag_examples() {
        assert!((contrast(Color32::BLACK, Color32::WHITE) - 21.0).abs() < 0.01);
        assert!((contrast(rgb(0x777777), Color32::WHITE) - 4.48).abs() < 0.01);
    }

    /// 4.5:1 for text and 3:1 for the edges of controls (WCAG 1.4.3 and
    /// 1.4.11), in the visuals as installed, both themes, on the page, on a
    /// surface and on a field.
    #[test]
    fn text_and_control_edges_have_enough_contrast() {
        let ctx = egui::Context::default();
        apply(&ctx);
        for (theme, p) in [(Theme::Dark, &DARK), (Theme::Light, &LIGHT)] {
            let style = ctx.style_of(theme);
            let v = &style.visuals;
            let w = &v.widgets;
            let grounds = [
                ("page", p.bg),
                ("surface", p.surface),
                ("field", p.field),
                ("tint", flatten(p.tint, p.surface)),
                ("warn tint", flatten(p.warn_tint, p.surface)),
                ("error tint", flatten(p.error_tint, p.surface)),
            ];
            for (what, fg) in [
                ("text", p.text),
                ("muted", p.muted),
                ("link", v.hyperlink_color),
                ("warning", v.warn_fg_color),
                ("error", v.error_fg_color),
                ("button text", w.inactive.fg_stroke.color),
                ("noninteractive text", w.noninteractive.fg_stroke.color),
            ] {
                for (on, bg) in grounds {
                    let ratio = contrast(fg, bg);
                    assert!(ratio >= 4.5, "{theme:?}: {what} on {on} is {ratio:.2}:1");
                }
            }
            let primary = contrast(p.primary_text, p.primary_fill);
            assert!(
                primary >= 4.5,
                "{theme:?}: primary button text is {primary:.2}:1"
            );
            for (what, edge) in [
                ("checkbox and field outline", w.inactive.bg_stroke.color),
                ("checkbox box, rail, scroll handle", w.inactive.bg_fill),
                ("hovered outline", w.hovered.bg_stroke.color),
                ("focus outline", w.active.bg_stroke.color),
                ("text selection outline", v.selection.stroke.color),
                ("switch on", p.primary_fill),
            ] {
                for (on, bg) in [("page", p.bg), ("surface", p.surface), ("field", p.field)] {
                    let ratio = contrast(edge, bg);
                    assert!(ratio >= 3.0, "{theme:?}: {what} on {on} is {ratio:.2}:1");
                }
            }
            // The tick inside a checkbox, on the box's fill, and a switch's knob.
            let tick = contrast(w.inactive.fg_stroke.color, w.inactive.bg_fill);
            assert!(tick >= 3.0, "{theme:?}: checkbox tick is {tick:.2}:1");
            let knob = contrast(p.switch_knob, p.primary_fill);
            assert!(knob >= 3.0, "{theme:?}: switch knob is {knob:.2}:1");
        }
    }

    const KDEGLOBALS: &str = "\
[Colors:Button]
BackgroundNormal=1,1,1

[Colors:Header]
BackgroundNormal=222,224,226
ForegroundInactive=112,125,138
ForegroundNormal=35,38,41

[Colors:Header][Inactive]
BackgroundNormal=239,240,241
ForegroundInactive=112,125,138
ForegroundNormal=35,38,41
";

    #[test]
    fn the_header_colours_come_from_their_own_group() {
        let kde = parse_kde_header(KDEGLOBALS).unwrap();
        assert_eq!(kde.active.bg, Color32::from_rgb(222, 224, 226));
        assert_eq!(kde.active.text, Color32::from_rgb(35, 38, 41));
        assert_eq!(kde.active.muted, Color32::from_rgb(112, 125, 138));
        assert_eq!(kde.inactive.bg, Color32::from_rgb(239, 240, 241));
    }

    #[test]
    fn a_scheme_without_inactive_colours_reuses_the_active_ones() {
        let only_active = KDEGLOBALS
            .split("[Colors:Header][Inactive]")
            .next()
            .unwrap();
        let kde = parse_kde_header(only_active).unwrap();
        assert_eq!(kde.inactive, kde.active);
    }

    #[test]
    fn no_header_group_means_the_surface_colours() {
        assert_eq!(
            parse_kde_header("[Colors:Button]\nBackgroundNormal=1,1,1\n"),
            None
        );
        assert_eq!(
            parse_kde_header("[Colors:Header]\nBackgroundNormal=oops\n"),
            None
        );
        let ctx = egui::Context::default();
        ctx.set_theme(Theme::Dark);
        assert_eq!(header(&ctx, None, true).bg, DARK.surface);
    }

    #[test]
    fn every_family_is_installed_with_the_stock_fonts_behind_it() {
        let fonts = fonts();
        for family in [FontFamily::Proportional, medium(), semibold(), bold()] {
            let list = &fonts.families[&family];
            assert!(list[0].starts_with("Inter-"), "{family:?}: {list:?}");
            assert!(list.len() > 2, "{family:?} has no fallback: {list:?}");
        }
        assert_eq!(
            fonts.families[&FontFamily::Monospace][0],
            "JetBrainsMono-Regular"
        );
    }
}
