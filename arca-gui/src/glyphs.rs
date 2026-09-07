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
    /// A padlock, shut or open, for putting a password on and taking it off.
    Locked,
    Unlocked,
    /// A horizontal ellipsis: everything that did not fit on the bar.
    More,
    /// The three that walk the folders.
    Back,
    Forward,
    Up,
}

/// The same picture out of the font Windows draws its own programs with.
///
/// Preferred over everything below it. Drawing an icon that reads at fifteen
/// points is a craft, and the hand-painted padlock came out as a handbag and
/// the hand-painted cogwheel as an asterisk; more to the point, these are the
/// shapes the user has already learned from every other window on the machine.
/// What stays below is the fallback for a machine without the font.
pub fn codepoint(glyph: Glyph) -> Option<char> {
    Some(match glyph {
        // A page with an arrow leaving it. The plain open folder, E838, is
        // about folders, and what is being opened here is a file.
        Glyph::Open => '\u{E8E5}',
        // A folder with a zip fastener down it, which is the icon Windows
        // itself puts on a .zip.
        Glyph::Compress => '\u{F012}',
        // An arrow coming down onto a line: out of the archive and onto the
        // disk. Both extract buttons share it; the words tell them apart.
        Glyph::ExtractAll | Glyph::ExtractPicked => '\u{E896}',
        Glyph::Locked => '\u{E72E}',
        Glyph::Unlocked => '\u{E785}',
        Glyph::More => '\u{E712}',
        Glyph::Back => '\u{E72B}',
        Glyph::Forward => '\u{E72A}',
        Glyph::Up => '\u{E74A}',
    })
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
            // A box open at the top with something coming out of it, and the
            // same picture for both buttons.
            //
            // Two goes at telling them apart failed at this size: a tick beside
            // the box meant squeezing the box narrow and both halves came out
            // cramped, and a bar left inside it was too small to see at all.
            // The two buttons are a hand's width apart and both are labelled,
            // so the label is what tells them apart and the picture says what
            // family they belong to. A difference nobody can see is worse than
            // no difference.
            let (l, right) = (x0 + 1.5, x1 - 1.5);
            line(painter, Pos2::new(l, y0 + 5.0), Pos2::new(l, y1 - 1.5), color, thin);
            line(painter, Pos2::new(right, y0 + 5.0), Pos2::new(right, y1 - 1.5), color, thin);
            line(painter, Pos2::new(l, y1 - 1.5), Pos2::new(right, y1 - 1.5), color, thin);
            // Rising out of the box, which is the whole difference from the
            // picture next door: that arrow goes in, this one comes out.
            let mid = (l + right) / 2.0;
            line(painter, Pos2::new(mid, y0 + 3.0), Pos2::new(mid, y0 + 9.0), color, thin);
            painter.add(egui::Shape::convex_polygon(
                vec![
                    Pos2::new(mid - 2.5, y0 + 4.0),
                    Pos2::new(mid + 2.5, y0 + 4.0),
                    Pos2::new(mid, y0 + 0.5),
                ],
                color,
                Stroke::NONE,
            ));
        }
        Glyph::Locked | Glyph::Unlocked => {
            // Outlined with a keyhole, not a filled slab: a solid body came out
            // as a blob with a wire over it. The shackle is a real arc rather
            // than three straight pieces, which is most of what makes it read
            // as a padlock instead of as a rectangle wearing a bracket.
            let body = Rect::from_min_max(
                Pos2::new(x0 + 2.0, y0 + 6.5),
                Pos2::new(x1 - 2.0, y1 - 1.5),
            );
            painter.rect_stroke(body, 1.5, Stroke::new(thin, color));
            painter.circle_filled(Pos2::new(body.center().x, body.center().y), 1.2, color);

            let shut = matches!(glyph, Glyph::Locked);
            // Shut, the arc sits over the middle of the body. Open, it is the
            // same arc lifted and turned, hinged on its right leg.
            let cx = if shut { body.center().x } else { body.center().x + 2.2 };
            let radius = 3.1;
            let bottom = y0 + 6.5;
            let steps = 12;
            let mut arc: Vec<Pos2> = (0..=steps)
                .map(|k| {
                    let t = std::f32::consts::PI * (k as f32) / (steps as f32);
                    Pos2::new(cx - radius * t.cos(), bottom - radius * t.sin())
                })
                .collect();
            if !shut {
                // The near leg stops short, which is what an open one looks
                // like; the far one still reaches the body.
                arc.truncate(steps - 2);
            }
            painter.add(egui::Shape::line(
                arc,
                Stroke::new(1.6_f32, color),
            ));
        }
        Glyph::More => {
            for k in [-1.0_f32, 0.0, 1.0] {
                painter.circle_filled(Pos2::new(r.center().x + k * 4.6, r.center().y), 1.35, color);
            }
        }
        Glyph::Back | Glyph::Forward | Glyph::Up => {
            let c = r.center();
            let (aw, ah) = (4.5, 5.5);
            let p = match glyph {
                Glyph::Back => [
                    Pos2::new(c.x + aw * 0.6, c.y - ah),
                    Pos2::new(c.x + aw * 0.6, c.y + ah),
                    Pos2::new(c.x - aw, c.y),
                ],
                Glyph::Forward => [
                    Pos2::new(c.x - aw * 0.6, c.y - ah),
                    Pos2::new(c.x - aw * 0.6, c.y + ah),
                    Pos2::new(c.x + aw, c.y),
                ],
                _ => [
                    Pos2::new(c.x - ah, c.y + aw * 0.6),
                    Pos2::new(c.x + ah, c.y + aw * 0.6),
                    Pos2::new(c.x, c.y - aw),
                ],
            };
            painter.add(egui::Shape::convex_polygon(p.to_vec(), color, Stroke::NONE));
        }
    }
}
