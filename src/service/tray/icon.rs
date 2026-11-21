//! The tray icon, in the vinoPilot trays' style (vinoWatch, vinoWhisper): a
//! themed `-symbolic` icon in their shared frame, which Plasma recolours to
//! the panel's text, and the app tile painted as pixmaps for a host that
//! can't find it in the theme.

/// `data/dev.vinosearch-symbolic.svg`, installed as `<icon>-symbolic`.
pub const SYMBOLIC_SVG: &str = include_str!("../../../data/dev.vinosearch-symbolic.svg");

const SIZES: [u32; 6] = [16, 22, 24, 32, 48, 64];

/// `data/dev.vinosearch.svg`'s shapes, in its units (viewBox 2 2 60 60).
const TILE: [u8; 4] = [14, 14, 16, 255];
const EDGE: [u8; 4] = [255, 255, 255, 46];
const WHITE: [u8; 4] = [250, 250, 250, 255];
const GREY: [u8; 4] = [160, 164, 172, 128];
const GREEN: [u8; 4] = [86, 204, 120, 255];

/// Signed distance to a rounded rectangle: negative inside.
fn rounded(p: (f32, f32), x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let qx = (p.0 - cx).abs() - (w / 2.0 - r);
    let qy = (p.1 - cy).abs() - (h / 2.0 - r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

fn segment(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    ((p.0 - a.0 - t * dx).powi(2) + (p.1 - a.1 - t * dy).powi(2)).sqrt()
}

/// The colour at `p`, painted bottom to top.
fn paint(p: (f32, f32)) -> [f32; 4] {
    let layers: [(bool, [u8; 4]); 6] = [
        (rounded(p, 4.0, 4.0, 56.0, 56.0, 14.0) < 0.0, TILE),
        (rounded(p, 5.0, 5.0, 54.0, 54.0, 13.0).abs() < 1.0, EDGE),
        (
            (((p.0 - 28.0).powi(2) + (p.1 - 28.0).powi(2)).sqrt() - 12.0).abs() < 2.5,
            WHITE,
        ),
        (rounded(p, 22.0, 22.0, 12.0, 4.0, 2.0) < 0.0, GREY),
        (rounded(p, 22.0, 30.0, 8.0, 4.0, 2.0) < 0.0, GREEN),
        (segment(p, (37.0, 37.0), (47.0, 47.0)) < 3.0, GREEN),
    ];
    let mut out = [0.0f32; 4];
    for (inside, [r, g, b, a]) in layers {
        if !inside {
            continue;
        }
        let a = f32::from(a) / 255.0;
        for (o, c) in out.iter_mut().zip([r, g, b]) {
            *o = f32::from(c) / 255.0 * a + *o * (1.0 - a);
        }
        out[3] = a + out[3] * (1.0 - a);
    }
    out
}

/// ARGB32 in network byte order, as StatusNotifierItem's IconPixmap wants;
/// 4x4 samples a pixel.
fn pixmap(size: u32) -> ksni::Icon {
    const N: u32 = 4;
    let unit = 60.0 / size as f32;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let mut sum = [0.0f32; 4];
            for sy in 0..N {
                for sx in 0..N {
                    let p = (
                        2.0 + (x as f32 + (sx as f32 + 0.5) / N as f32) * unit,
                        2.0 + (y as f32 + (sy as f32 + 0.5) / N as f32) * unit,
                    );
                    let [r, g, b, a] = paint(p);
                    // Premultiplied, so edges average without dark fringes.
                    for (s, v) in sum.iter_mut().zip([r * a, g * a, b * a, a]) {
                        *s += v;
                    }
                }
            }
            let a = sum[3] / (N * N) as f32;
            let un = |c: f32| {
                if a > 0.0 {
                    (c / (N * N) as f32 / a * 255.0).round() as u8
                } else {
                    0
                }
            };
            data.extend([
                (a * 255.0).round() as u8,
                un(sum[0]),
                un(sum[1]),
                un(sum[2]),
            ]);
        }
    }
    ksni::Icon {
        width: size as i32,
        height: size as i32,
        data,
    }
}

pub fn pixmaps() -> Vec<ksni::Icon> {
    SIZES.into_iter().map(pixmap).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_size_is_a_complete_argb_image() {
        for icon in pixmaps() {
            assert_eq!(icon.data.len(), (icon.width * icon.height * 4) as usize);
        }
    }

    #[test]
    fn the_pixmap_is_the_app_tile() {
        let icon = pixmap(64);
        let at = |x: i32, y: i32| {
            let i = ((y * icon.width + x) * 4) as usize;
            &icon.data[i..i + 4]
        };
        assert_eq!(at(0, 0)[0], 0, "transparent outside the tile");
        assert_eq!(at(32, 50), [255, 14, 14, 16], "the dark tile");
        let handle = at(44, 44);
        assert!(
            handle[2] > 180 && handle[1] < 120,
            "the green handle: {handle:?}"
        );
        let ring = at(28, 13);
        assert!(ring[1] > 200 && ring[2] > 200, "the white ring: {ring:?}");
    }

    #[test]
    fn the_symbolic_icon_is_in_the_shared_frame_and_recoloured() {
        // vinoWatch's and vinoWhisper's tray frame, path for path.
        assert!(SYMBOLIC_SVG.contains(
            "M3 2h10a2 2 0 0 1 2 2v8a2 2 0 0 1 -2 2h-10a2 2 0 0 1 -2 -2v-8a2 2 0 0 1 2 -2z"
        ));
        assert!(SYMBOLIC_SVG.contains("ColorScheme-Text"));
        assert!(SYMBOLIC_SVG.contains("#289650"), "the trays' panel green");
    }
}
