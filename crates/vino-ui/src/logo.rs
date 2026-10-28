//! The three app logos, painted at any size without an SVG renderer: an ink
//! tile with a faint inner edge, a white mark and a green accent. vinoSearch's
//! is `data/dev.vinosearch.svg`, vinoSpeak's `docs/assets/vinospeak.svg`;
//! vinoWhisper's is the speech bubble with green bars proposed on the vino UI
//! Foundations board (vinoWhisper#128).

use egui::{Color32, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, Vec2, vec2};

use crate::{a11y, icons};

const INK: Color32 = Color32::from_rgb(0x0e, 0x0e, 0x10);
const WHITE: Color32 = Color32::from_rgb(0xfa, 0xfa, 0xfa);
const GREEN: Color32 = Color32::from_rgb(0x56, 0xcc, 0x78);

/// The speech bubble, on the logo's 60-unit grid.
pub const WHISPER_BUBBLE: &str =
    "M16 20a4 4 0 0 1 4-4h20a4 4 0 0 1 4 4v14a4 4 0 0 1-4 4H27l-7 6v-6a4 4 0 0 1-4-4z";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Logo {
    Search,
    Whisper,
    Speak,
}

/// The logo in a `size` square, hidden from screen readers: the app's name
/// is written beside it.
pub fn logo(ui: &mut Ui, which: Logo, size: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    a11y::decorative(ui, response.id);
    if ui.is_rect_visible(rect) {
        paint(ui.painter(), rect, which);
    }
    response
}

pub fn search(ui: &mut Ui, size: f32) -> Response {
    logo(ui, Logo::Search, size)
}

pub fn whisper(ui: &mut Ui, size: f32) -> Response {
    logo(ui, Logo::Whisper, size)
}

pub fn speak(ui: &mut Ui, size: f32) -> Response {
    logo(ui, Logo::Speak, size)
}

/// The tile every logo sits on, and a mapping from the SVG's units: the tile
/// fills 4 to 60 of a 64-unit box, drawn edge to edge.
fn tile(painter: &Painter, rect: Rect) -> (f32, impl Fn(f32, f32) -> Pos2) {
    let k = rect.width().min(rect.height()) / 56.0;
    let origin = rect.min;
    let at = move |x: f32, y: f32| origin + vec2((x - 4.0) * k, (y - 4.0) * k);
    painter.rect_filled(
        Rect::from_min_max(at(4.0, 4.0), at(60.0, 60.0)),
        14.0 * k,
        INK,
    );
    painter.rect_stroke(
        Rect::from_min_max(at(5.0, 5.0), at(59.0, 59.0)),
        13.0 * k,
        Stroke::new(2.0 * k, Color32::from_white_alpha(46)),
        egui::StrokeKind::Middle,
    );
    (k, at)
}

/// A stroke with round ends.
fn capped(painter: &Painter, points: Vec<Pos2>, stroke: Stroke) {
    if let (Some(first), Some(last)) = (points.first(), points.last()) {
        painter.circle_filled(*first, stroke.width / 2.0, stroke.color);
        painter.circle_filled(*last, stroke.width / 2.0, stroke.color);
    }
    painter.add(Shape::line(points, stroke));
}

pub fn paint(painter: &Painter, rect: Rect, which: Logo) {
    let (k, at) = tile(painter, rect);
    match which {
        Logo::Search => {
            // The SVG's own box is 60 units from (2, 2); the tile is 52 wide
            // there, drawn here over the shared 56.
            let s = 56.0 / 52.0;
            let at = |x: f32, y: f32| at(4.0 + (x - 4.0) * s, 4.0 + (y - 4.0) * s);
            let box_ =
                |x: f32, y: f32, w: f32, h: f32| Rect::from_min_max(at(x, y), at(x + w, y + h));
            let k = k * s;
            painter.circle_stroke(at(28.0, 28.0), 12.0 * k, Stroke::new(5.0 * k, WHITE));
            painter.rect_filled(
                box_(22.0, 22.0, 12.0, 4.0),
                2.0 * k,
                Color32::from_rgb(0xa0, 0xa4, 0xac).gamma_multiply(0.5),
            );
            painter.rect_filled(box_(22.0, 30.0, 8.0, 4.0), 2.0 * k, GREEN);
            capped(
                painter,
                vec![at(37.0, 37.0), at(47.0, 47.0)],
                Stroke::new(6.0 * k, GREEN),
            );
        }
        Logo::Whisper => {
            let s = 56.0 / 52.0;
            let at = |x: f32, y: f32| at(4.0 + (x - 4.0) * s, 4.0 + (y - 4.0) * s);
            let k = k * s;
            for sub in icons::parse(WHISPER_BUBBLE) {
                let points: Vec<Pos2> = sub.points.iter().map(|p| at(p.x, p.y)).collect();
                painter.add(Shape::closed_line(points, Stroke::new(4.0 * k, WHITE)));
            }
            let bars = Stroke::new(4.0 * k, GREEN);
            painter.circle_filled(at(23.0, 27.0), 2.0 * k, GREEN);
            capped(painter, vec![at(30.0, 23.0), at(30.0, 31.0)], bars);
            capped(painter, vec![at(37.0, 25.0), at(37.0, 29.0)], bars);
        }
        Logo::Speak => {
            let speaker = [
                (14.0, 27.0),
                (16.0, 25.0),
                (21.0, 25.0),
                (29.0, 18.0),
                (29.0, 46.0),
                (21.0, 39.0),
                (16.0, 39.0),
                (14.0, 37.0),
            ];
            painter.add(Shape::convex_polygon(
                speaker.iter().map(|&(x, y)| at(x, y)).collect(),
                WHITE,
                Stroke::NONE,
            ));
            for path in ["M36 26Q40 32 36 38", "M42 20Q51 32 42 44"] {
                for sub in icons::parse(path) {
                    let points: Vec<Pos2> = sub.points.iter().map(|p| at(p.x, p.y)).collect();
                    capped(painter, points, Stroke::new(4.0 * k, GREEN));
                }
            }
        }
    }
}

/// A square of `size`.
pub fn square(size: f32) -> Vec2 {
    vec2(size, size)
}
