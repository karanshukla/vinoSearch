//! vinoWhisper's settings theme (its gui/src/theme.rs), so the vino windows
//! match. Colours, type and shapes follow the vinoPilot landing page's site.css:
//! Inter, flat surfaces separated by hairlines, 6px corners, a green that only
//! ever means "this one".

use std::sync::Arc;

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle,
    Theme, Visuals,
};

pub struct Palette {
    pub bg: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub rule: Color32,
    pub code_bg: Color32,
    pub link: Color32,
    /// egui's own red is 3.9:1 on the light page, under the 4.5:1 text needs.
    pub error: Color32,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

pub const DARK: Palette = Palette {
    bg: rgb(0x121315),
    text: rgb(0xe4e5e8),
    muted: rgb(0x9a9ea6),
    rule: rgb(0x2a2c30),
    code_bg: rgb(0x1e2023),
    link: rgb(0x7bdc97),
    error: rgb(0xff6b6b),
};

pub const LIGHT: Palette = Palette {
    bg: rgb(0xfcfcfc),
    text: rgb(0x1c1d20),
    muted: rgb(0x5d6068),
    rule: rgb(0xe2e3e6),
    code_bg: rgb(0xeef0f2),
    link: rgb(0x1a7040),
    error: rgb(0xc5221f),
};

/// The outline of a checkbox or text field, a slider's rail and a scroll handle: 3:1 against
/// the page and against `code_bg`, as WCAG asks of the edges that make a control findable.
pub fn boundary(p: &Palette) -> Color32 {
    p.rule.lerp_to_gamma(p.muted, 0.7)
}

pub fn palette(ctx: &egui::Context) -> &'static Palette {
    match ctx.theme() {
        Theme::Dark => &DARK,
        Theme::Light => &LIGHT,
    }
}

// winit on Wayland only knows the theme of decorations it draws itself, so egui would fall
// back to dark on a light desktop. Ask the portal instead.
pub fn desktop_preference() -> Option<Theme> {
    parse_color_scheme(&read_setting("org.freedesktop.appearance", "color-scheme")?)
}

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

/// Whether the desktop asks for less motion: GNOME's "Reduce animation", or Plasma's
/// animation speed slid to instant.
fn reduced_motion() -> bool {
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

/// How much larger the desktop wants text: GNOME's Large Text, or Plasma's general font
/// past its 10 pt default. 1.0 when neither says.
pub fn text_scale() -> f32 {
    let gnome = read_setting("org.gnome.desktop.interface", "text-scaling-factor");
    let kde = read_setting("org.kde.kdeglobals.General", "font");
    let gnome = gnome.as_deref().and_then(scaling_factor).unwrap_or(1.0);
    let kde = kde.as_deref().and_then(kde_font_scale).unwrap_or(1.0);
    gnome.max(kde).clamp(1.0, 3.0)
}

// "v d 1.25".
fn scaling_factor(reply: &str) -> Option<f32> {
    let factor = reply.split_whitespace().last()?.parse::<f32>().ok()?;
    factor.is_finite().then_some(factor)
}

// A Qt font string, "v s \"Noto Sans,12,-1,5,50,0,0,0,0,0\"": family, then point size.
fn kde_font_scale(reply: &str) -> Option<f32> {
    let points = reply.split(',').nth(1)?.trim().parse::<f32>().ok()?;
    (points > 0.0).then_some(points / 10.0)
}

/// No animation time is the signal: egui's own transitions go instant and `window::spinner`
/// stops turning.
pub fn still(ctx: &egui::Context) -> bool {
    ctx.global_style().animation_time == 0.0
}

// 1 prefers dark, 2 prefers light, 0 is no preference, which desktops answer for their light default.
fn parse_color_scheme(reply: &str) -> Option<Theme> {
    match reply.split_whitespace().last()?.parse::<u32>().ok()? {
        1 => Some(Theme::Dark),
        _ => Some(Theme::Light),
    }
}

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

// Breeze paints the title bar in the colour scheme's Header colours, so a header bar painted
// in them runs into it, as Dolphin's toolbar does. KDE only: elsewhere the file means nothing.
pub fn kde_header() -> Option<KdeHeader> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    if !desktop
        .split(':')
        .any(|name| name.eq_ignore_ascii_case("KDE"))
    {
        return None;
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".config"))
        })?;
    parse_kde_header(&std::fs::read_to_string(config.join("kdeglobals")).ok()?)
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

pub fn header(ctx: &egui::Context, kde: Option<&KdeHeader>, focused: bool) -> Header {
    match kde {
        Some(kde) if focused => kde.active,
        Some(kde) => kde.inactive,
        None => {
            let p = palette(ctx);
            Header {
                bg: p.bg,
                text: p.text,
                muted: p.muted,
            }
        }
    }
}

pub fn bold() -> FontFamily {
    FontFamily::Name("Inter-Bold".into())
}

fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    for (name, bytes) in [
        (
            "Inter-Regular",
            &include_bytes!("../assets/fonts/Inter-Regular.ttf")[..],
        ),
        (
            "Inter-Bold",
            &include_bytes!("../assets/fonts/Inter-Bold.ttf")[..],
        ),
    ] {
        fonts
            .font_data
            .insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    }
    // Regular leads the stock families so every widget picks it up; the default fonts stay
    // behind it for the glyphs the latin subset lacks.
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, "Inter-Regular".to_owned());
    }
    // Monospace is for commands and paths only, so keep the stock face first there.
    let mono = fonts.families.get_mut(&FontFamily::Monospace).unwrap();
    mono.remove(0);
    mono.push("Inter-Regular".to_owned());
    fonts.families.insert(
        bold(),
        vec!["Inter-Bold".to_owned(), "Inter-Regular".to_owned()],
    );
    fonts
}

fn visuals(p: &Palette, base: Visuals) -> Visuals {
    let corner = CornerRadius::same(6);
    let mut v = base;
    v.panel_fill = p.bg;
    v.window_fill = p.bg;
    v.extreme_bg_color = p.code_bg;
    v.faint_bg_color = p.code_bg;
    v.hyperlink_color = p.link;
    v.warn_fg_color = p.link;
    v.error_fg_color = p.error;
    v.window_stroke = Stroke::new(1.0_f32, p.rule);
    v.selection.bg_fill = p.link.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0_f32, p.link);

    v.widgets.noninteractive.bg_fill = p.bg;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.rule);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, p.text);
    for widget in [
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        widget.corner_radius = corner;
    }
    v.widgets.noninteractive.corner_radius = corner;
    v.widgets.inactive.bg_fill = p.code_bg;
    v.widgets.inactive.weak_bg_fill = p.code_bg;
    // most of the way to muted: the unchecked box of a checkbox has to show against the page
    v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, boundary(p));
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, p.text);
    v.widgets.hovered.bg_fill = p.code_bg;
    v.widgets.hovered.weak_bg_fill = p.code_bg;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.muted);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, p.text);
    v.widgets.active.bg_fill = p.code_bg;
    v.widgets.active.weak_bg_fill = p.code_bg;
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, p.link);
    v.widgets.active.fg_stroke = Stroke::new(1.0_f32, p.text);
    v.widgets.open.bg_fill = p.code_bg;
    v.widgets.open.weak_bg_fill = p.code_bg;
    v.widgets.open.bg_stroke = Stroke::new(1.0_f32, p.link);
    v
}

/// A solid scroll bar paints its handle in `bg_fill` on an `extreme_bg_color`
/// track, and the visuals above make both `code_bg`, so the handle vanished.
/// Nothing else here uses `bg_fill` (buttons take `weak_bg_fill`), so give it
/// the checkbox outline's grey, lighter while hovered and green while dragged. Checkboxes and
/// slider rails paint in `bg_fill` too, so they take the same grey.
pub fn visible_scroll_handles(ctx: &egui::Context) {
    for (theme, p) in [(Theme::Dark, &DARK), (Theme::Light, &LIGHT)] {
        ctx.style_mut_of(theme, |style| {
            let w = &mut style.visuals.widgets;
            w.inactive.bg_fill = boundary(p);
            w.hovered.bg_fill = p.muted;
            w.active.bg_fill = p.link;
        });
    }
}

pub fn install(ctx: &egui::Context) {
    apply(ctx);
    follow_desktop(ctx);
}

/// Fonts, colours and sizes, without asking the desktop anything.
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
            .insert(TextStyle::Button, FontId::proportional(14.0));
        style
            .text_styles
            .insert(TextStyle::Small, FontId::proportional(12.5));
        style
            .text_styles
            .insert(TextStyle::Heading, FontId::new(30.0, bold()));
        style
            .text_styles
            .insert(TextStyle::Monospace, FontId::monospace(13.0));
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 4.0);
        style.spacing.interact_size.y = 28.0;
    });
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
        assert_eq!(scaling_factor("v d 1.25\n"), Some(1.25));
        assert_eq!(scaling_factor("v d 1\n"), Some(1.0));
        assert_eq!(scaling_factor("v d nan\n"), None);
        assert_eq!(scaling_factor(""), None);
        let font = |size: &str| format!("v s \"Noto Sans,{size},-1,5,50,0,0,0,0,0\"\n");
        assert_eq!(kde_font_scale(&font("10")), Some(1.0));
        assert_eq!(kde_font_scale(&font("15")), Some(1.5));
        assert_eq!(kde_font_scale(&font("0")), None);
        assert_eq!(kde_font_scale("v s \"Noto Sans\"\n"), None);
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

    fn contrast(a: Color32, b: Color32) -> f64 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn contrast_ratio_matches_the_wcag_examples() {
        assert!((contrast(Color32::BLACK, Color32::WHITE) - 21.0).abs() < 0.01);
        assert!((contrast(rgb(0x777777), Color32::WHITE) - 4.48).abs() < 0.01);
    }

    /// 4.5:1 for text and 3:1 for the edges of controls (WCAG 1.4.3 and 1.4.11), in the
    /// visuals as installed, both themes.
    #[test]
    fn text_and_control_edges_have_enough_contrast() {
        let ctx = egui::Context::default();
        apply(&ctx);
        visible_scroll_handles(&ctx);
        for (theme, p) in [(Theme::Dark, &DARK), (Theme::Light, &LIGHT)] {
            let style = ctx.style_of(theme);
            let v = &style.visuals;
            let w = &v.widgets;
            for (what, fg) in [
                ("text", p.text),
                ("muted", p.muted),
                ("link", v.hyperlink_color),
                ("warning", v.warn_fg_color),
                ("error", v.error_fg_color),
                ("button text", w.inactive.fg_stroke.color),
                ("noninteractive text", w.noninteractive.fg_stroke.color),
            ] {
                for (on, bg) in [("page", p.bg), ("field", p.code_bg)] {
                    let ratio = contrast(fg, bg);
                    assert!(ratio >= 4.5, "{theme:?}: {what} on {on} is {ratio:.2}:1");
                }
            }
            for (what, edge) in [
                ("checkbox and field outline", w.inactive.bg_stroke.color),
                ("checkbox box, rail, scroll handle", w.inactive.bg_fill),
                ("hovered outline", w.hovered.bg_stroke.color),
                ("focus outline", w.active.bg_stroke.color),
                ("text selection outline", v.selection.stroke.color),
            ] {
                for (on, bg) in [("page", p.bg), ("field", p.code_bg)] {
                    let ratio = contrast(edge, bg);
                    assert!(ratio >= 3.0, "{theme:?}: {what} on {on} is {ratio:.2}:1");
                }
            }
            // The tick inside a checkbox, on the box's fill.
            let tick = contrast(w.inactive.fg_stroke.color, w.inactive.bg_fill);
            assert!(tick >= 3.0, "{theme:?}: checkbox tick is {tick:.2}:1");
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
    fn no_header_group_means_the_landing_page_colours() {
        assert_eq!(
            parse_kde_header("[Colors:Button]\nBackgroundNormal=1,1,1\n"),
            None
        );
        assert_eq!(
            parse_kde_header("[Colors:Header]\nBackgroundNormal=oops\n"),
            None
        );
    }
}
