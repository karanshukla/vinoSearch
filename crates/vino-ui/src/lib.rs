//! vino UI: the one look of the vinoPilot egui apps (vinoSearch, vinoWhisper,
//! vinoSpeak), built on vinopilot.ca's site.css: Inter, flat surfaces
//! separated by hairlines, ink and green. One copy here replaces the
//! `theme.rs` each app carried (vinoSearch#148).
//!
//! - [`theme`]: the colour tokens for dark and light, fonts, egui's visuals
//!   and spacing, and what the desktop asks for (light or dark, text size,
//!   reduced motion, KDE's header colours).
//! - [`widgets`]: the parts every window is made of: groups of rows, switches,
//!   segmented controls, buttons, chips, tags, key caps, banners, nav items
//!   and tabs, and a spinner that stands still when motion is reduced.
//! - [`icons`]: a 16 px stroke icon set painted from SVG paths.
//! - [`logo`]: the three app logos, painted so they stay sharp at any scale.
//! - [`a11y`]: names, roles and headings for screen readers, which egui's
//!   painted widgets don't give by themselves.

pub use egui;

pub mod a11y;
pub mod icons;
pub mod logo;
pub mod theme;
pub mod widgets;

pub use icons::Icon;
pub use theme::{Palette, palette};
pub use widgets::{Button, ButtonKind, ChipKind, Row, Tone};
