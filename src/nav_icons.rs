//! The sidebar's navigation icons, drawn as vectors so each can move in its
//! own way when its pane opens: the clock's hands turn, the bars regrow, the
//! sliders slide. At rest they read like the SF Symbols they replace.

use std::f32::consts::{PI, TAU};

use gpui::{Bounds, PathBuilder, Pixels, Point, Rgba, Window, point, px};

use crate::desktop_ui::NavigationIcon;

/// Icons are designed on a 16-unit square.
const GRID: f32 = 16.0;
const STROKE: f32 = 1.5;

/// Draws in grid units inside `bounds`, with round ends and joins.
struct Pen<'a> {
    bounds: Bounds<Pixels>,
    scale: f32,
    color: Rgba,
    window: &'a mut Window,
}

impl Pen<'_> {
    fn at(&self, (x, y): (f32, f32)) -> Point<Pixels> {
        point(
            self.bounds.origin.x + px(x * self.scale),
            self.bounds.origin.y + px(y * self.scale),
        )
    }

    fn paint(&mut self, path: PathBuilder) {
        if let Ok(path) = path.build() {
            self.window.paint_path(path, self.color);
        }
    }

    /// Lyon's default caps are square; a dot rounds each end and corner.
    fn dot(&mut self, center: (f32, f32), radius: f32) {
        let r = px(radius * self.scale);
        let c = self.at(center);
        let mut path = PathBuilder::fill();
        path.move_to(point(c.x + r, c.y));
        path.arc_to(point(r, r), px(0.0), false, true, point(c.x - r, c.y));
        path.arc_to(point(r, r), px(0.0), false, true, point(c.x + r, c.y));
        path.close();
        self.paint(path);
    }

    fn line(&mut self, points: &[(f32, f32)]) {
        let mut path = PathBuilder::stroke(px(STROKE * self.scale));
        path.move_to(self.at(points[0]));
        for point in &points[1..] {
            path.line_to(self.at(*point));
        }
        self.paint(path);
        for point in points {
            self.dot(*point, STROKE / 2.0);
        }
    }

    /// An arc clockwise on screen from `start` to `end`, angles in radians.
    fn arc(&mut self, center: (f32, f32), radii: (f32, f32), start: f32, end: f32) {
        let point_at = |angle: f32| {
            (
                center.0 + radii.0 * angle.cos(),
                center.1 + radii.1 * angle.sin(),
            )
        };
        let sweep = end - start;
        let mut path = PathBuilder::stroke(px(STROKE * self.scale));
        path.move_to(self.at(point_at(start)));
        // Halves keep each segment unambiguous, even for full circles.
        let middle = start + sweep / 2.0;
        for (from, to) in [(start, middle), (middle, end)] {
            path.arc_to(
                point(px(radii.0 * self.scale), px(radii.1 * self.scale)),
                px(0.0),
                to - from > PI,
                true,
                self.at(point_at(to)),
            );
        }
        self.paint(path);
        if sweep < TAU - 0.01 {
            self.dot(point_at(start), STROKE / 2.0);
            self.dot(point_at(end), STROKE / 2.0);
        }
    }

    fn circle(&mut self, center: (f32, f32), radius: f32) {
        self.arc(center, (radius, radius), 0.0, TAU);
    }

    fn rounded_rect(&mut self, (x, y): (f32, f32), (w, h): (f32, f32), radius: f32, fill: bool) {
        let r = radius.min(w / 2.0).min(h / 2.0);
        let radii = point(px(r * self.scale), px(r * self.scale));
        let mut path = if fill {
            PathBuilder::fill()
        } else {
            PathBuilder::stroke(px(STROKE * self.scale))
        };
        path.move_to(self.at((x + r, y)));
        path.line_to(self.at((x + w - r, y)));
        path.arc_to(radii, px(0.0), false, true, self.at((x + w, y + r)));
        path.line_to(self.at((x + w, y + h - r)));
        path.arc_to(radii, px(0.0), false, true, self.at((x + w - r, y + h)));
        path.line_to(self.at((x + r, y + h)));
        path.arc_to(radii, px(0.0), false, true, self.at((x, y + h - r)));
        path.line_to(self.at((x, y + r)));
        path.arc_to(radii, px(0.0), false, true, self.at((x + r, y)));
        path.close();
        self.paint(path);
    }

    /// A four-point sparkle with gently curved sides.
    fn sparkle(&mut self, (cx, cy): (f32, f32), size: f32) {
        let pinch = size * 0.18;
        let tips = [
            (cx, cy - size),
            (cx + size, cy),
            (cx, cy + size),
            (cx - size, cy),
        ];
        let controls = [
            (cx + pinch, cy - pinch),
            (cx + pinch, cy + pinch),
            (cx - pinch, cy + pinch),
            (cx - pinch, cy - pinch),
        ];
        let mut path = PathBuilder::fill();
        path.move_to(self.at(tips[0]));
        for index in 0..4 {
            path.curve_to(self.at(tips[(index + 1) % 4]), self.at(controls[index]));
        }
        path.close();
        self.paint(path);
    }
}

/// How long each icon's motion lasts.
pub(crate) const NAV_ICON_MOTION_MS: u64 = 700;

/// Paints `icon` at `progress` through its motion; 0 and 1 are both at rest.
pub(crate) fn paint(
    icon: NavigationIcon,
    progress: f32,
    bounds: Bounds<Pixels>,
    color: Rgba,
    window: &mut Window,
) {
    let mut pen = Pen {
        bounds,
        scale: f32::from(bounds.size.height) / GRID,
        color,
        window,
    };
    let p = progress.clamp(0.0, 1.0);
    // Rises and returns: 0 at rest, 1 at the height of the motion.
    let swell = (p * PI).sin();
    let eased = crate::desktop_ui::ease_out(p);
    match icon {
        NavigationIcon::Settings => {
            // Three sliders; their knobs slide out and come back.
            for (row, (rest, travel)) in [(5.0, 5.0), (10.5, -5.0), (7.5, 4.0)]
                .into_iter()
                .enumerate()
            {
                let y = 3.5 + row as f32 * 4.5;
                pen.line(&[(2.0, y), (14.0, y)]);
                pen.dot((rest + travel * (p * PI).sin(), y), 2.1);
            }
        }
        NavigationIcon::Microphone => {
            // The capsule fills like a voice level, twice, then empties.
            pen.rounded_rect((5.5, 1.0), (5.0, 9.0), 2.5, false);
            let level = ((p * TAU).sin().abs() * (1.0 - p * 0.3)).clamp(0.0, 1.0) * swell.min(1.0);
            if level > 0.02 {
                let height = 7.0 * level;
                pen.rounded_rect(
                    (6.75, 9.0 - 0.75 - height + 0.75),
                    (2.5, height),
                    1.25,
                    true,
                );
            }
            pen.arc((8.0, 6.5), (4.75, 4.75), 0.0, PI);
            pen.line(&[(8.0, 11.25), (8.0, 14.5)]);
            pen.line(&[(5.5, 14.5), (10.5, 14.5)]);
        }
        NavigationIcon::Providers => {
            // A globe whose meridians turn half a revolution.
            let turn = eased * PI;
            pen.circle((8.0, 8.0), 6.5);
            pen.line(&[(1.5, 8.0), (14.5, 8.0)]);
            for offset in [0.0, PI / 2.0] {
                let width = 3.4 * (turn + offset).cos().abs();
                if width < 0.3 {
                    pen.line(&[(8.0, 1.5), (8.0, 14.5)]);
                } else {
                    pen.circle_ellipse((8.0, 8.0), (width, 6.5));
                }
            }
        }
        NavigationIcon::Models => {
            // Sparkles that twinkle in turn.
            let late = ((p - 0.25) / 0.75).clamp(0.0, 1.0);
            pen.sparkle((6.5, 9.5), 5.0 * (1.0 + 0.25 * swell));
            pen.sparkle((12.5, 3.5), 2.4 * (1.0 + 0.7 * (late * PI).sin()));
        }
        NavigationIcon::PostProcessing => {
            // Ragged lines that justify themselves and relax again.
            for (row, rest) in [14.0_f32, 10.0, 12.5, 8.0].into_iter().enumerate() {
                let y = 2.5 + row as f32 * 3.7;
                let end = rest + (14.0 - rest) * swell;
                pen.line(&[(2.0, y), (end, y)]);
            }
        }
        NavigationIcon::Hud => {
            // The capsule with the HUD's line, which waves like a voice.
            pen.rounded_rect((1.0, 4.5), (14.0, 7.0), 3.5, false);
            let points: Vec<(f32, f32)> = (0..=12)
                .map(|step| {
                    let t = step as f32 / 12.0;
                    let taper = (t * PI).sin();
                    let y = 8.0 + 1.8 * swell * taper * (t * TAU * 1.5 + p * TAU * 2.0).sin();
                    (4.5 + 7.0 * t, y)
                })
                .collect();
            pen.line(&points);
        }
        NavigationIcon::History => {
            // The clock: its hands spin around and land where they were.
            pen.circle((8.0, 8.0), 6.5);
            // Both end where they began: two turns of the minute hand, one of the hour hand.
            let minute = -PI / 2.0 + eased * 2.0 * TAU;
            let hour = eased * TAU;
            let hand =
                |angle: f32, length: f32| (8.0 + length * angle.cos(), 8.0 + length * angle.sin());
            pen.line(&[(8.0, 8.0), hand(minute, 4.5)]);
            pen.line(&[(8.0, 8.0), hand(hour, 3.0)]);
        }
        NavigationIcon::Statistics => {
            // Bars that settle at new heights mid-way and return.
            let rests = [6.0, 11.0, 8.5];
            let peaks = [11.0, 6.5, 12.5];
            for (index, (rest, peak)) in rests.into_iter().zip(peaks).enumerate() {
                let local = ((p - index as f32 * 0.08) / 0.84).clamp(0.0, 1.0);
                let height = rest + (peak - rest) * (local * PI).sin();
                let x = 1.75 + index as f32 * 4.6;
                pen.rounded_rect((x, 14.5 - height), (3.4, height), 1.0, true);
            }
        }
    }
}

impl Pen<'_> {
    fn circle_ellipse(&mut self, center: (f32, f32), radii: (f32, f32)) {
        self.arc(center, radii, 0.0, TAU);
    }
}
