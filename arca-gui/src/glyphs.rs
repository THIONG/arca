//! The little pictures on the buttons.
//!
//! Painted rather than loaded. An icon font would be a file to ship and a
//! licence to track for eight shapes, and an SVG loader a whole renderer; these
//! are a dozen lines of rectangles and lines each, they take the colour of
//! whatever button they sit on, and they cannot come out as a hollow box on a
//! machine that is missing something. The arrows in the navigation row were
//! already drawn this way and these keep them company.
//!
//! All of them are drawn inside a square of `SIZE` and read at that size: no
//! detail smaller than a pixel, nothing that depends on a hairline landing on
//! an exact half pixel.

use eframe::egui::{self, Color32, Pos2, Rect, Stroke, Vec2};

/// The square every glyph is drawn inside.
pub const SIZE: f32 = 15.0;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    /// A folder standing open: opening an archive.
    Open,
    /// A closed box: making one.
    Compress,
    /// A box with an arrow leaving it downwards: taking everything out.
    ExtractAll,
    /// The same with a tick beside it: taking out what is picked.
    ExtractPicked,
    /// A tick inside a frame: checking what is inside is intact.
    Test,
    /// A padlock, shut or open, for putting a password on and taking it off.
    Locked,
    Unlocked,
    /// A cogwheel.
    Settings,
    /// A ticked and an empty box, for the two buttons over the list.
    CheckAll,
    UncheckAll,
    /// A question mark in a circle: the list of shortcuts.
    Help,
}

fn line(p: &egui::Painter, a: Pos2, b: Pos2, c: Color32, w: f32) {
    p.line_segment([a, b], Stroke::new(w, c));
}

/// Draws `glyph` in `rect`, in `color`. The rect is expected to be square and
/// about [`SIZE`] across; anything else still draws, just not as carefully.
pub fn draw(painter: &egui::Painter, rect: Rect, glyph: Glyph, color: Color32) {
    let c = rect.center();
    let r = Rect::from_center_size(Pos2::new(c.x.round(), c.y.round()), Vec2::splat(SIZE));
    let (x0, y0, x1, y1) = (r.left(), r.top(), r.right(), r.bottom());
    let w = r.width();
    let h = r.height();
    let thin = 1.4;

    match glyph {
        Glyph::Open => {
            // A folder, tab and all. It started as a box with a lid and could
            // not be told apart from the one next to it; the three that follow
            // are all boxes, so this one had better not be.
            painter.add(egui::Shape::convex_polygon(
                vec![
                    Pos2::new(x0 + 1.0, y1 - 2.0),
                    Pos2::new(x0 + 1.0, y0 + 3.0),
                    Pos2::new(x0 + 6.0, y0 + 3.0),
                    Pos2::new(x0 + 7.5, y0 + 5.0),
                    Pos2::new(x1 - 1.0, y0 + 5.0),
                    Pos2::new(x1 - 1.0, y1 - 2.0),
                ],
                Color32::TRANSPARENT,
                Stroke::new(thin, color),
            ));
        }
        Glyph::Compress => {
            // The same box as extracting, with the arrow going the other way.
            // In and out is the whole difference between the two jobs, so it is
            // the whole difference between the two pictures.
            let (l, right) = (x0 + 1.5, x1 - 1.5);
            line(painter, Pos2::new(l, y0 + 5.0), Pos2::new(l, y1 - 1.5), color, thin);
            line(painter, Pos2::new(right, y0 + 5.0), Pos2::new(right, y1 - 1.5), color, thin);
            line(painter, Pos2::new(l, y1 - 1.5), Pos2::new(right, y1 - 1.5), color, thin);
            let mid = (l + right) / 2.0;
            line(painter, Pos2::new(mid, y0 + 0.5), Pos2::new(mid, y0 + 5.5), color, thin);
            painter.add(egui::Shape::convex_polygon(
                vec![
                    Pos2::new(mid - 2.5, y0 + 4.0),
                    Pos2::new(mid + 2.5, y0 + 4.0),
                    Pos2::new(mid, y0 + 7.5),
                ],
                color,
                Stroke::NONE,
            ));
        }
        Glyph::ExtractAll | Glyph::ExtractPicked => {
            // A box open at the top with something coming out of it.
            let narrow = matches!(glyph, Glyph::ExtractPicked);
            let right = if narrow { x1 - 5.0 } else { x1 - 1.5 };
            line(painter, Pos2::new(x0 + 1.5, y0 + 5.0), Pos2::new(x0 + 1.5, y1 - 1.5), color, thin);
            line(painter, Pos2::new(right, y0 + 5.0), Pos2::new(right, y1 - 1.5), color, thin);
            line(painter, Pos2::new(x0 + 1.5, y1 - 1.5), Pos2::new(right, y1 - 1.5), color, thin);
            // Rising out of the box, which is the whole difference from the
            // picture next door: that arrow goes in, this one comes out.
            let mid = (x0 + 1.5 + right) / 2.0;
            line(painter, Pos2::new(mid, y0 + 3.0), Pos2::new(mid, y0 + 9.5), color, thin);
            painter.add(egui::Shape::convex_polygon(
                vec![
                    Pos2::new(mid - 2.5, y0 + 4.0),
                    Pos2::new(mid + 2.5, y0 + 4.0),
                    Pos2::new(mid, y0 + 0.5),
                ],
                color,
                Stroke::NONE,
            ));
            if narrow {
                // The tick that says "the ones that are picked".
                line(painter, Pos2::new(x1 - 4.0, y0 + 4.0), Pos2::new(x1 - 2.5, y0 + 5.5), color, thin);
                line(painter, Pos2::new(x1 - 2.5, y0 + 5.5), Pos2::new(x1 + 0.5, y0 + 1.5), color, thin);
            }
        }
        Glyph::Test => {
            // A magnifying glass. It began as a frame with a tick in it and
            // came out indistinguishable from the button that ticks everything,
            // which is two buttons along.
            let c = Pos2::new(x0 + 6.0, y0 + 6.0);
            painter.circle_stroke(c, 4.6, Stroke::new(thin, color));
            line(
                painter,
                Pos2::new(c.x + 3.4, c.y + 3.4),
                Pos2::new(x1 - 1.5, y1 - 1.5),
                color,
                2.0,
            );
        }
        Glyph::Locked | Glyph::Unlocked => {
            // The body filled rather than outlined: an outline this small ends
            // up as a grey smudge, and the shackle above it needs something
            // solid to sit on to read as a padlock at all.
            let body = Rect::from_min_max(
                Pos2::new(x0 + 1.5, y0 + 7.0),
                Pos2::new(x1 - 1.5, y1 - 1.0),
            );
            painter.rect_filled(body, 1.5, color);
            let shut = matches!(glyph, Glyph::Locked);
            // Shut, the shackle sits over the middle; open, it leans off to the
            // right and its near leg stops short of the body.
            let (sl, sr) = if shut {
                (x0 + 4.0, x1 - 4.0)
            } else {
                (x0 + 6.5, x1 - 1.5)
            };
            let top = y0 + 2.5;
            let s = Stroke::new(1.7_f32, color);
            painter.line_segment([Pos2::new(sl, y0 + 7.0), Pos2::new(sl, top)], s);
            painter.line_segment([Pos2::new(sl, top), Pos2::new(sr, top)], s);
            painter.line_segment(
                [
                    Pos2::new(sr, top),
                    Pos2::new(sr, if shut { y0 + 7.0 } else { y0 + 5.0 }),
                ],
                s,
            );
        }
        Glyph::Settings => {
            // Three sliders, not a cogwheel. A cog needs teeth, and teeth at
            // fifteen points across come out as an asterisk.
            for (k, knob) in [(0.0_f32, 0.66_f32), (1.0, 0.34), (2.0, 0.58)] {
                let y = y0 + 3.0 + k * 4.5;
                line(painter, Pos2::new(x0 + 1.0, y), Pos2::new(x1 - 1.0, y), color, thin);
                painter.circle_filled(Pos2::new(x0 + 1.0 + (w - 2.0) * knob, y), 2.1, color);
            }
        }
        Glyph::CheckAll | Glyph::UncheckAll => {
            let body = Rect::from_min_max(
                Pos2::new(x0 + 1.5, y0 + 1.5),
                Pos2::new(x1 - 1.5, y1 - 1.5),
            );
            painter.rect_stroke(body, 2.0, Stroke::new(thin, color));
            if matches!(glyph, Glyph::CheckAll) {
                line(painter, Pos2::new(x0 + 4.0, r.center().y), Pos2::new(x0 + 6.5, y1 - 4.5), color, thin);
                line(painter, Pos2::new(x0 + 6.5, y1 - 4.5), Pos2::new(x1 - 4.0, y0 + 4.5), color, thin);
            }
        }
        Glyph::Help => {
            painter.circle_stroke(r.center(), h * 0.42, Stroke::new(thin, color));
            let c = r.center();
            // The hook of a question mark, then its dot.
            line(painter, Pos2::new(c.x - 2.2, c.y - 2.0), Pos2::new(c.x + 0.6, c.y - 3.2), color, thin);
            line(painter, Pos2::new(c.x + 0.6, c.y - 3.2), Pos2::new(c.x + 1.8, c.y - 0.6), color, thin);
            line(painter, Pos2::new(c.x + 1.8, c.y - 0.6), Pos2::new(c.x, c.y + 1.2), color, thin);
            painter.circle_filled(Pos2::new(c.x, c.y + 3.4), 0.9, color);
        }
    }
}
