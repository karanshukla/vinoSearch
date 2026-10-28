//! A 16 px stroke icon set, painted from SVG path data on a 24-unit grid:
//! round caps and joins, 1.8 units wide, in the text's colour. Painted rather
//! than a font so an icon is one shape, scales with the window and never
//! falls back to a missing-glyph box.

use egui::{Color32, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, Vec2, pos2, vec2};

use crate::a11y;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Icon {
    Search,
    Folder,
    Lock,
    Model,
    Hardware,
    Integration,
    Index,
    Plus,
    Excluded,
    File,
    FileText,
    Image,
    ChevronDown,
    Clock,
    Play,
    Stop,
    Voice,
    Speaker,
    Keyboard,
    Captions,
    Mic,
    Cleanup,
    Sessions,
    Settings,
    Archive,
    More,
    ArrowLeft,
    Warning,
    Error,
    Info,
    Download,
    Flask,
    Key,
    Refresh,
    Check,
}

impl Icon {
    /// The SVG path data, in a 24 × 24 box.
    pub fn path(self) -> &'static str {
        match self {
            Icon::Search => "M11 4a7 7 0 1 0 0 14a7 7 0 1 0 0-14 m9 16-3.5-3.5",
            Icon::Folder => {
                "M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"
            }
            Icon::Lock => "M5 11h14v10H5z M8 11V7a4 4 0 0 1 8 0v4",
            Icon::Model => "M12 3 4 7.5v9L12 21l8-4.5v-9z M4 7.5 12 12l8-4.5M12 12v9",
            Icon::Hardware => {
                "M6 6h12v12H6z M9.5 9.5h5v5h-5z M9 2v3M15 2v3M9 19v3M15 19v3M2 9h3M2 15h3M19 9h3M19 15h3"
            }
            Icon::Integration => "M9 2v5M15 2v5M6 7h12v4a6 6 0 0 1-12 0zM12 17v5",
            Icon::Index => "M3 12h4l3-8 4 16 3-8h4",
            Icon::Plus => "M12 5v14M5 12h14",
            Icon::Excluded => "M12 3a9 9 0 1 0 0 18a9 9 0 1 0 0-18 m-6.4 2.6 12.8 12.8",
            Icon::File => "M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z M14 3v5h5",
            Icon::FileText => {
                "M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z M14 3v5h5M9 13h6M9 17h6"
            }
            Icon::Image => "M3 4h18v16H3z M9 8a2 2 0 1 0 0 4a2 2 0 1 0 0-4 m12 8-5-5-9 9",
            Icon::ChevronDown => "m6 9 6 6 6-6",
            Icon::Clock => "M12 3a9 9 0 1 0 0 18a9 9 0 1 0 0-18 M12 7v5l3 2",
            Icon::Play => "M8 5v14l11-7z",
            Icon::Stop => "M6 6h12v12H6z",
            Icon::Voice => "M12 4a4 4 0 1 0 0 8a4 4 0 1 0 0-8 M4 21a8 8 0 0 1 16 0",
            Icon::Speaker => "M11 5 6 9H3v6h3l5 4z M16 9a4 4 0 0 1 0 6",
            Icon::Keyboard => "M2 6h20v12H2z M6 10h.01M10 10h.01M14 10h.01M18 10h.01M7 14h10",
            Icon::Captions => "M3 5h18v14H3z M7 11h4M13 11h4M7 15h7",
            Icon::Mic => "M9 3h6v11H9z M5 11a7 7 0 0 0 14 0M12 18v3",
            Icon::Cleanup => {
                "M20 20H9l-5-5 9.5-9.5a2 2 0 0 1 2.8 0l3.2 3.2a2 2 0 0 1 0 2.8L11 20 m-2.5-9.5 5 5"
            }
            Icon::Sessions => {
                "M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z M14 3v5h5M9 13h6M9 17h6"
            }
            Icon::Settings => {
                "M12 9a3 3 0 1 0 0 6a3 3 0 1 0 0-6 M12 2v3M12 19v3M2 12h3M19 12h3M4.9 4.9l2.1 2.1M17 17l2.1 2.1M4.9 19.1 7 17M17 7l2.1-2.1"
            }
            Icon::Archive => "M21 8v12H3V8M1 3h22v5H1zM10 12h4",
            Icon::More => "M5 12h.01M12 12h.01M19 12h.01",
            Icon::ArrowLeft => "M19 12H5M11 6l-6 6 6 6",
            Icon::Warning => "M12 3 2 20h20z M12 10v4M12 17h.01",
            Icon::Error => "M12 3a9 9 0 1 0 0 18a9 9 0 1 0 0-18 M12 8v5M12 16h.01",
            Icon::Info => "M12 3a9 9 0 1 0 0 18a9 9 0 1 0 0-18 M12 11v5M12 8h.01",
            Icon::Download => "M12 4v11M7 10l5 5 5-5M5 20h14",
            Icon::Flask => {
                "M9 3h6M10 3v6l-5 9a2 2 0 0 0 1.7 3h10.6a2 2 0 0 0 1.7-3l-5-9V3 M7.5 15h9"
            }
            Icon::Key => "M8 11a4 4 0 1 0 0 8a4 4 0 1 0 0-8 m3 1 9-9M17 6l3 3",
            Icon::Refresh => {
                "M20 11a8 8 0 0 0-14.5-4.5L4 8M4 3v5h5 M4 13a8 8 0 0 0 14.5 4.5L20 16M20 21v-5h-5"
            }
            Icon::Check => "m5 12 5 5 9-10",
        }
    }

    /// Where the three dots of `More` are heavier than the rest.
    fn stroke_scale(self) -> f32 {
        match self {
            Icon::More => 3.0 / 1.8,
            _ => 1.0,
        }
    }
}

/// The icon in a `size` square, taking its space in the layout; hidden from
/// screen readers, since the text beside it says what it is for.
pub fn icon(ui: &mut Ui, icon: Icon, size: f32, color: Color32) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    a11y::decorative(ui, response.id);
    if ui.is_rect_visible(rect) {
        paint(ui.painter(), rect, icon, color);
    }
    response
}

/// The icon painted to fill `rect`.
pub fn paint(painter: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let k = rect.width().min(rect.height()) / 24.0;
    let origin = rect.center() - vec2(12.0, 12.0) * k;
    let width = 1.8 * k * icon.stroke_scale();
    let stroke = Stroke::new(width, color);
    for sub in parse(icon.path()) {
        let points: Vec<Pos2> = sub
            .points
            .iter()
            .map(|p| origin + p.to_vec2() * k)
            .collect();
        match points.len() {
            0 => {}
            // "h.01": a dot, drawn as the stroke's round cap alone.
            1 => {
                painter.circle_filled(points[0], width / 2.0, color);
            }
            _ if sub.closed => {
                painter.add(Shape::closed_line(points, stroke));
            }
            _ => {
                // Round caps: egui's strokes end square.
                painter.circle_filled(points[0], width / 2.0, color);
                painter.circle_filled(points[points.len() - 1], width / 2.0, color);
                painter.add(Shape::line(points, stroke));
            }
        }
    }
}

/// One subpath as points, flattened.
#[derive(Debug, Clone, PartialEq)]
pub struct Subpath {
    pub points: Vec<Pos2>,
    pub closed: bool,
}

/// SVG path data (M, L, H, V, C, Q, A, Z and their relative forms, with
/// implicit repeats) as flattened subpaths. A dot drawn as a zero-length
/// segment keeps its one point.
pub fn parse(path: &str) -> Vec<Subpath> {
    let mut out = Vec::new();
    let mut current: Vec<Pos2> = Vec::new();
    let mut pos = pos2(0.0, 0.0);
    let mut start = pos;
    let mut command = 'M';
    let tokens = tokenize(path);
    let mut i = 0;
    let number = |i: &mut usize| -> Option<f32> {
        let n = match tokens.get(*i)? {
            Token::Number(n) => *n,
            Token::Command(_) => return None,
        };
        *i += 1;
        Some(n)
    };
    let flush = |current: &mut Vec<Pos2>, out: &mut Vec<Subpath>, closed: bool| {
        if !current.is_empty() {
            let mut points = std::mem::take(current);
            if points.len() >= 2 && points.last() == points.first() && closed {
                points.pop();
            }
            // "M12 12h0" and "h.01" are dots.
            if points.len() == 2 && points[0].distance(points[1]) < 0.05 {
                points.pop();
            }
            out.push(Subpath { points, closed });
        }
    };
    while i < tokens.len() {
        if let Token::Command(c) = tokens[i] {
            command = c;
            i += 1;
        }
        let relative = command.is_ascii_lowercase();
        let at = |x: f32, y: f32, pos: Pos2| {
            if relative {
                pos + vec2(x, y)
            } else {
                pos2(x, y)
            }
        };
        match command.to_ascii_uppercase() {
            'M' => {
                let (Some(x), Some(y)) = (number(&mut i), number(&mut i)) else {
                    break;
                };
                flush(&mut current, &mut out, false);
                pos = at(x, y, pos);
                start = pos;
                current.push(pos);
                // Further pairs are line-tos.
                command = if relative { 'l' } else { 'L' };
            }
            'L' => {
                let (Some(x), Some(y)) = (number(&mut i), number(&mut i)) else {
                    break;
                };
                pos = at(x, y, pos);
                current.push(pos);
            }
            'H' => {
                let Some(x) = number(&mut i) else { break };
                pos = if relative {
                    pos + vec2(x, 0.0)
                } else {
                    pos2(x, pos.y)
                };
                current.push(pos);
            }
            'V' => {
                let Some(y) = number(&mut i) else { break };
                pos = if relative {
                    pos + vec2(0.0, y)
                } else {
                    pos2(pos.x, y)
                };
                current.push(pos);
            }
            'C' => {
                let mut n = [0.0; 6];
                let mut ok = true;
                for slot in &mut n {
                    match number(&mut i) {
                        Some(v) => *slot = v,
                        None => ok = false,
                    }
                }
                if !ok {
                    break;
                }
                let c1 = at(n[0], n[1], pos);
                let c2 = at(n[2], n[3], pos);
                let end = at(n[4], n[5], pos);
                for t in 1..=8 {
                    let t = t as f32 / 8.0;
                    let u = 1.0 - t;
                    let p = pos.to_vec2() * (u * u * u)
                        + c1.to_vec2() * (3.0 * u * u * t)
                        + c2.to_vec2() * (3.0 * u * t * t)
                        + end.to_vec2() * (t * t * t);
                    current.push(p.to_pos2());
                }
                pos = end;
            }
            'Q' => {
                let mut n = [0.0; 4];
                let mut ok = true;
                for slot in &mut n {
                    match number(&mut i) {
                        Some(v) => *slot = v,
                        None => ok = false,
                    }
                }
                if !ok {
                    break;
                }
                let c = at(n[0], n[1], pos);
                let end = at(n[2], n[3], pos);
                for t in 1..=8 {
                    let t = t as f32 / 8.0;
                    let u = 1.0 - t;
                    let p = pos.to_vec2() * (u * u)
                        + c.to_vec2() * (2.0 * u * t)
                        + end.to_vec2() * (t * t);
                    current.push(p.to_pos2());
                }
                pos = end;
            }
            'A' => {
                let mut n = [0.0; 7];
                let mut ok = true;
                for slot in &mut n {
                    match number(&mut i) {
                        Some(v) => *slot = v,
                        None => ok = false,
                    }
                }
                if !ok {
                    break;
                }
                let end = at(n[5], n[6], pos);
                arc(
                    &mut current,
                    pos,
                    n[0],
                    n[1],
                    n[2],
                    n[3] != 0.0,
                    n[4] != 0.0,
                    end,
                );
                pos = end;
            }
            'Z' => {
                pos = start;
                flush(&mut current, &mut out, true);
                // A Z followed by numbers continues from the start point.
                current.clear();
                if matches!(tokens.get(i), Some(Token::Number(_))) {
                    current.push(pos);
                    command = if relative { 'l' } else { 'L' };
                }
            }
            _ => break,
        }
    }
    flush(&mut current, &mut out, false);
    out
}

/// An SVG elliptical arc, flattened (SVG 1.1 appendix F.6.5).
#[allow(clippy::too_many_arguments)]
fn arc(
    into: &mut Vec<Pos2>,
    from: Pos2,
    rx: f32,
    ry: f32,
    rotation: f32,
    large: bool,
    sweep: bool,
    to: Pos2,
) {
    if rx == 0.0 || ry == 0.0 || from == to {
        into.push(to);
        return;
    }
    let (rx, ry) = (rx.abs(), ry.abs());
    let phi = rotation.to_radians();
    let (sin, cos) = phi.sin_cos();
    let d = (from - to) / 2.0;
    let x1 = cos * d.x + sin * d.y;
    let y1 = -sin * d.x + cos * d.y;
    let mut rx = rx;
    let mut ry = ry;
    let lambda = (x1 * x1) / (rx * rx) + (y1 * y1) / (ry * ry);
    if lambda > 1.0 {
        rx *= lambda.sqrt();
        ry *= lambda.sqrt();
    }
    let num = (rx * rx * ry * ry - rx * rx * y1 * y1 - ry * ry * x1 * x1).max(0.0);
    let den = rx * rx * y1 * y1 + ry * ry * x1 * x1;
    let mut coef = if den == 0.0 { 0.0 } else { (num / den).sqrt() };
    if large == sweep {
        coef = -coef;
    }
    let cx1 = coef * rx * y1 / ry;
    let cy1 = -coef * ry * x1 / rx;
    let mid = (from.to_vec2() + to.to_vec2()) / 2.0;
    let cx = cos * cx1 - sin * cy1 + mid.x;
    let cy = sin * cx1 + cos * cy1 + mid.y;
    let angle = |ux: f32, uy: f32, vx: f32, vy: f32| {
        let dot = ux * vx + uy * vy;
        let len = (ux * ux + uy * uy).sqrt() * (vx * vx + vy * vy).sqrt();
        let mut a = (dot / len).clamp(-1.0, 1.0).acos();
        if ux * vy - uy * vx < 0.0 {
            a = -a;
        }
        a
    };
    let theta = angle(1.0, 0.0, (x1 - cx1) / rx, (y1 - cy1) / ry);
    let mut delta = angle(
        (x1 - cx1) / rx,
        (y1 - cy1) / ry,
        (-x1 - cx1) / rx,
        (-y1 - cy1) / ry,
    );
    if !sweep && delta > 0.0 {
        delta -= std::f32::consts::TAU;
    } else if sweep && delta < 0.0 {
        delta += std::f32::consts::TAU;
    }
    let steps = ((delta.abs() / (std::f32::consts::PI / 8.0)).ceil() as usize).max(2);
    for s in 1..=steps {
        let t = theta + delta * s as f32 / steps as f32;
        let (st, ct) = t.sin_cos();
        let x = cx + cos * rx * ct - sin * ry * st;
        let y = cy + sin * rx * ct + cos * ry * st;
        into.push(pos2(x, y));
    }
    // Land exactly where the path says.
    if let Some(last) = into.last_mut() {
        *last = to;
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Token {
    Command(char),
    Number(f32),
}

fn tokenize(path: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_ascii_alphabetic() {
            out.push(Token::Command(c));
            i += 1;
        } else if c.is_ascii_digit() || c == '-' || c == '.' || c == '+' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                let d = bytes[i] as char;
                // A second '.' or a '-' starts the next number: "4.5.5", "4-4".
                if d.is_ascii_digit() || (d == '.' && !path[start..i].contains('.')) {
                    i += 1;
                } else {
                    break;
                }
            }
            if let Ok(n) = path[start..i].parse::<f32>() {
                out.push(Token::Number(n));
            }
        } else {
            i += 1;
        }
    }
    out
}

/// The icon's size for a body line.
pub const SIZE: f32 = 16.0;

/// Both in one square.
pub fn size(size: f32) -> Vec2 {
    vec2(size, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_box_is_four_corners() {
        let subs = parse("M6 6h12v12H6z");
        assert_eq!(subs.len(), 1);
        assert!(subs[0].closed);
        assert_eq!(
            subs[0].points,
            vec![
                pos2(6.0, 6.0),
                pos2(18.0, 6.0),
                pos2(18.0, 18.0),
                pos2(6.0, 18.0)
            ]
        );
    }

    #[test]
    fn relative_moves_and_implicit_line_tos_follow_the_pen() {
        let subs = parse("m6 9 6 6 6-6");
        assert_eq!(subs.len(), 1);
        assert_eq!(
            subs[0].points,
            vec![pos2(6.0, 9.0), pos2(12.0, 15.0), pos2(18.0, 9.0)]
        );
    }

    #[test]
    fn an_arc_stays_on_its_circle() {
        let subs = parse("M12 3a9 9 0 1 0 0 18a9 9 0 1 0 0-18");
        assert_eq!(subs.len(), 1);
        let points = &subs[0].points;
        assert!(points.len() > 12, "flattened finely: {}", points.len());
        for p in points {
            let r = p.distance(pos2(12.0, 12.0));
            assert!((r - 9.0).abs() < 0.2, "{p:?} is {r} from the centre");
        }
        assert!(
            points.iter().any(|p| p.x < 4.0),
            "the arc went round the left"
        );
        assert!(points.iter().any(|p| p.x > 20.0), "and the right");
    }

    #[test]
    fn a_dot_is_one_point() {
        let subs = parse("M5 12h.01M12 12h.01");
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0].points.len(), 1);
        assert_eq!(subs[1].points, vec![pos2(12.0, 12.0)]);
    }

    #[test]
    fn every_icon_parses_inside_its_box() {
        for icon in [
            Icon::Search,
            Icon::Folder,
            Icon::Lock,
            Icon::Model,
            Icon::Hardware,
            Icon::Integration,
            Icon::Index,
            Icon::Plus,
            Icon::Excluded,
            Icon::File,
            Icon::FileText,
            Icon::Image,
            Icon::ChevronDown,
            Icon::Clock,
            Icon::Play,
            Icon::Stop,
            Icon::Voice,
            Icon::Speaker,
            Icon::Keyboard,
            Icon::Captions,
            Icon::Mic,
            Icon::Cleanup,
            Icon::Sessions,
            Icon::Settings,
            Icon::Archive,
            Icon::More,
            Icon::ArrowLeft,
            Icon::Warning,
            Icon::Error,
            Icon::Info,
            Icon::Download,
            Icon::Flask,
            Icon::Key,
            Icon::Refresh,
            Icon::Check,
        ] {
            let subs = parse(icon.path());
            assert!(!subs.is_empty(), "{icon:?} drew nothing");
            for sub in &subs {
                for p in &sub.points {
                    assert!(
                        (-0.5..=24.5).contains(&p.x) && (-0.5..=24.5).contains(&p.y),
                        "{icon:?} leaves the box at {p:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_logo_bubble_parses_with_its_arcs() {
        let subs = parse(crate::logo::WHISPER_BUBBLE);
        assert_eq!(subs.len(), 1);
        assert!(subs[0].closed);
        assert!(subs[0].points.len() > 20);
    }
}
