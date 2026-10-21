//! Sidebar building blocks in the style of macOS inspectors: section headers
//! with an icon, soft rounded cards, label/control rows, switches and a
//! segmented control. Spacing follows an 8 pt grid.

use egui::text::{LayoutJob, TextFormat};
use egui::{Align, Color32, CornerRadius, Layout, Margin, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, pos2, vec2};

use super::theme::{self, Palette, SECONDARY};
use crate::mixer::{MAX_DB, MIN_DB};

/// Height of one row inside a card.
pub const ROW_H: f32 = 28.0;
/// Card corner radius and inner padding.
const CARD_R: u8 = 10;
const CARD_PAD: i8 = 12;

#[derive(Clone, Copy)]
pub enum Icon {
    Scene,
    Display,
    Camera,
    Overlay,
    Audio,
    Output,
    Replay,
    Stream,
}

/// Card fill and hairline border for the sidebar.
fn card_colors(p: &Palette, dark: bool) -> (Color32, Color32) {
    if dark {
        (theme::blend(p.sidebar, Color32::from_white_alpha(10)), Color32::from_white_alpha(14))
    } else {
        (Color32::WHITE, Color32::from_black_alpha(16))
    }
}

/// Section header: a small icon and an uppercase, letter-spaced label.
pub fn header(ui: &mut Ui, icon: Icon, title: &str) {
    let p = theme::pal(ui);
    ui.add_space(16.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.add_space(2.0);
        let (rect, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
        paint_icon(ui, rect, icon, p.secondary);
        let mut job = LayoutJob::default();
        job.append(
            &title.to_uppercase(),
            0.0,
            TextFormat { font_id: theme::semibold(10.5), color: p.secondary, extra_letter_spacing: 0.6, ..Default::default() },
        );
        ui.label(job);
    });
    ui.add_space(8.0);
}

/// A rounded card holding grouped controls.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = theme::pal(ui);
    let (fill, border) = card_colors(&p, ui.visuals().dark_mode);
    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, border))
        .corner_radius(CornerRadius::same(CARD_R))
        .inner_margin(Margin::symmetric(CARD_PAD, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;
            add(ui)
        })
        .inner
}

/// Hairline between rows of a card, spanning the card's content width.
pub fn divider(ui: &mut Ui) {
    let p = theme::pal(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, p.separator.gamma_multiply(0.7)));
}

/// A label on the left and right-aligned controls.
pub fn row<R>(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = theme::pal(ui);
    let w = ui.available_width();
    ui.allocate_ui_with_layout(vec2(w, ROW_H), Layout::left_to_right(Align::Center), |ui| {
        ui.set_min_size(vec2(w, ROW_H));
        ui.label(egui::RichText::new(label).color(p.text));
        ui.with_layout(Layout::right_to_left(Align::Center), add).inner
    })
    .inner
}

/// Like [`row`] but with a secondary label column of fixed width and the
/// control filling the rest (sliders).
pub fn field<R>(ui: &mut Ui, label: &str, label_w: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = theme::pal(ui);
    let w = ui.available_width();
    ui.allocate_ui_with_layout(vec2(w, ROW_H), Layout::left_to_right(Align::Center), |ui| {
        ui.set_min_size(vec2(w, ROW_H));
        slot(ui, label_w, Align::Min, egui::RichText::new(label).size(SECONDARY + 1.0).color(p.secondary));
        add(ui)
    })
    .inner
}

/// Text in a fixed-width slot, aligned left or right (columns line up).
pub fn slot(ui: &mut Ui, width: f32, align: Align, text: egui::RichText) {
    let layout = if align == Align::Max { Layout::right_to_left(Align::Center) } else { Layout::left_to_right(Align::Center) };
    ui.allocate_ui_with_layout(vec2(width, ROW_H), layout, |ui| {
        ui.set_min_size(vec2(width, ROW_H));
        ui.add(egui::Label::new(text).truncate());
    });
}

/// Secondary helper text under a control.
pub fn hint(ui: &mut Ui, text: &str) {
    let p = theme::pal(ui);
    ui.add(egui::Label::new(egui::RichText::new(text).size(SECONDARY).color(p.secondary)).wrap());
}

/// An iOS/macOS style switch.
pub fn switch(ui: &mut Ui, on: &mut bool, enabled: bool) -> Response {
    let size = vec2(30.0, 18.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    if enabled && resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let p = theme::pal(ui);
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let off = if ui.visuals().dark_mode { Color32::from_white_alpha(38) } else { Color32::from_black_alpha(30) };
    let track = lerp_color(off, theme::accent(), t);
    let track = if enabled { track } else { track.gamma_multiply(0.45) };
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(9), track);
    let r = rect.height() / 2.0 - 2.0;
    let cx = egui::lerp(rect.left() + r + 2.0..=rect.right() - r - 2.0, t);
    let c = pos2(cx, rect.center().y);
    painter.circle_filled(c + vec2(0.0, 0.5), r + 0.5, Color32::from_black_alpha(40));
    painter.circle_filled(c, r, if enabled { Color32::WHITE } else { theme::blend(p.control, Color32::from_white_alpha(120)) });
    if resp.hovered() && enabled {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// Fader travel as (dB, position 0..1) pairs, as on a mixing desk: unity
/// at 70% with fine steps around it, the bottom silence, the top a boost.
const FADER_SCALE: [(f32, f32); 9] = [
    (MIN_DB, 0.0),
    (-40.0, 0.12),
    (-30.0, 0.25),
    (-20.0, 0.40),
    (-10.0, 0.55),
    (0.0, 0.70),
    (6.0, 0.80),
    (12.0, 0.88),
    (MAX_DB, 1.0),
];

/// Position (0..1) of a level in dB on a fader.
pub fn fader_pos(db: f32) -> f32 {
    if db.is_nan() || db <= MIN_DB {
        return 0.0;
    }
    let db = db.min(MAX_DB);
    let i = FADER_SCALE.iter().position(|&(d, _)| db <= d).unwrap_or(FADER_SCALE.len() - 1).max(1);
    let ((d0, p0), (d1, p1)) = (FADER_SCALE[i - 1], FADER_SCALE[i]);
    p0 + (db - d0) / (d1 - d0) * (p1 - p0)
}

/// The level in dB at a fader position (0..1).
pub fn fader_db(pos: f32) -> f32 {
    let pos = pos.clamp(0.0, 1.0);
    let i = FADER_SCALE.iter().position(|&(_, p)| pos <= p).unwrap_or(FADER_SCALE.len() - 1).max(1);
    let ((d0, p0), (d1, p1)) = (FADER_SCALE[i - 1], FADER_SCALE[i]);
    d0 + (pos - p0) / (p1 - p0) * (d1 - d0)
}

const KNOB_R: f32 = 6.5;

/// A horizontal audio fader for a level in dB, from silence to a boost.
/// Drag the knob, click the track to jump, scroll for fine steps,
/// double-click for 0 dB. Unity has a tick and a light detent. `live`
/// false (a muted source) greys the fill.
pub fn fader(ui: &mut Ui, db: &mut f32, width: f32, enabled: bool, live: bool) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(vec2(width, 2.0 * KNOB_R + 2.0), if enabled { Sense::click_and_drag() } else { Sense::hover() });
    // The knob's center travels between the track's ends.
    let travel = rect.shrink2(vec2(KNOB_R + 1.0, 0.0)).x_range();
    let x_of = |db: f32| egui::lerp(travel, fader_pos(db));
    let mut new = None;
    if enabled {
        // Grabbing the knob moves it from where it was grabbed; pressing the
        // track elsewhere jumps there.
        let grab_id = resp.id.with("grab");
        // Where the button went down: a drag is only recognized after the
        // pointer has moved.
        if resp.drag_started()
            && let Some(p) = ui.input(|i| i.pointer.press_origin())
        {
            let off = p.x - x_of(*db);
            ui.data_mut(|d| d.insert_temp(grab_id, if off.abs() <= KNOB_R + 2.0 { off } else { 0.0 }));
        }
        if resp.double_clicked() {
            new = Some(0.0);
        } else if (resp.dragged() || resp.clicked())
            && let Some(p) = resp.interact_pointer_pos()
        {
            let off = if resp.dragged() { ui.data(|d| d.get_temp(grab_id)).unwrap_or(0.0) } else { 0.0 };
            let on_knob = resp.clicked() && (p.x - x_of(*db)).abs() <= KNOB_R;
            if !on_knob {
                let v = (fader_db((p.x - off - travel.min) / travel.span()) * 10.0).round() / 10.0;
                new = Some(if v.abs() < 0.5 { 0.0 } else { v });
            }
        }
        if resp.hovered() {
            let d = ui.input(|i| i.smooth_scroll_delta());
            if d != egui::Vec2::ZERO {
                new = Some(db.max(MIN_DB) + (d.x + d.y) * 0.025);
            }
            ui.ctx().set_cursor_icon(if resp.dragged() { egui::CursorIcon::Grabbing } else { egui::CursorIcon::PointingHand });
        }
    }
    if let Some(v) = new.map(|v| v.clamp(MIN_DB, MAX_DB))
        && v != *db
    {
        *db = v;
        resp.mark_changed();
    }
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let painter = ui.painter();
    let cy = rect.center().y;
    let track = Rect::from_x_y_ranges(rect.x_range(), cy - 2.0..=cy + 2.0);
    let off = if dark { Color32::from_white_alpha(38) } else { Color32::from_black_alpha(30) };
    painter.rect_filled(track, CornerRadius::same(2), off);
    let kx = x_of(*db);
    let fill = if !enabled { off } else if live { theme::accent() } else { p.tertiary };
    if kx > track.left() + 0.5 {
        painter.rect_filled(Rect::from_x_y_ranges(track.left()..=kx, track.y_range()), CornerRadius::same(2), fill);
    }
    // Unity.
    let ux = x_of(0.0);
    painter.vline(ux, cy - KNOB_R..=cy + KNOB_R, Stroke::new(1.0, p.tertiary));
    let c = pos2(kx, cy);
    painter.circle_filled(c + vec2(0.0, 0.5), KNOB_R + 0.5, Color32::from_black_alpha(if enabled { 50 } else { 20 }));
    let knob = if enabled { Color32::WHITE } else { theme::blend(p.control, Color32::from_white_alpha(120)) };
    painter.circle_filled(c, KNOB_R, knob);
    if !dark {
        painter.circle_stroke(c, KNOB_R, Stroke::new(0.5, Color32::from_black_alpha(40)));
    }
    resp
}

/// A speaker button that mutes a source: sound waves when on, a red cross
/// when muted.
pub fn mute_button(ui: &mut Ui, muted: &mut bool, enabled: bool) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(vec2(22.0, 22.0), if enabled { Sense::click() } else { Sense::hover() });
    if enabled && resp.clicked() {
        *muted = !*muted;
        resp.mark_changed();
    }
    let p = theme::pal(ui);
    let painter = ui.painter();
    if enabled && resp.hovered() {
        painter.rect_filled(rect, CornerRadius::same(5), p.hover);
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let red = Color32::from_rgb(0xff, 0x3b, 0x30);
    let color = if !enabled { p.tertiary } else if *muted { red } else { p.secondary };
    let at = |x: f32, y: f32| pos2(rect.left() + x, rect.top() + y);
    // Speaker: a box and a cone.
    painter.add(Shape::convex_polygon(
        vec![at(4.5, 9.0), at(7.5, 9.0), at(11.0, 5.5), at(11.0, 16.5), at(7.5, 13.0), at(4.5, 13.0)],
        color,
        Stroke::NONE,
    ));
    let s = Stroke::new(1.4, color);
    if *muted {
        painter.line_segment([at(13.5, 8.5), at(18.5, 13.5)], s);
        painter.line_segment([at(18.5, 8.5), at(13.5, 13.5)], s);
    } else {
        for r in [3.5f32, 6.5] {
            let pts: Vec<Pos2> = (0..=8).map(|i| at(11.0, 11.0) + egui::Vec2::angled(-0.75 + 1.5 * i as f32 / 8.0) * r).collect();
            painter.add(Shape::line(pts, s));
        }
    }
    resp
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let [ar, ag, ab, aa] = a.to_srgba_unmultiplied();
    let [br, bg, bb, ba] = b.to_srgba_unmultiplied();
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(m(ar, br), m(ag, bg), m(ab, bb), m(aa, ba))
}

/// Segmented control: a recessed track with the chosen segment raised.
/// `options` are (value, label, enabled, disabled hint).
pub fn segmented<T: Copy + PartialEq>(ui: &mut Ui, value: &mut T, options: &[(T, &str, bool, &str)]) -> bool {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let (track_rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::hover());
    let track = if dark { Color32::from_black_alpha(70) } else { Color32::from_black_alpha(14) };
    ui.painter().rect_filled(track_rect, CornerRadius::same(8), track);
    let inner = track_rect.shrink(2.0);
    let w = inner.width() / options.len() as f32;
    let mut changed = false;
    for (i, (v, label, ok, why)) in options.iter().enumerate() {
        let r = Rect::from_min_size(pos2(inner.left() + w * i as f32, inner.top()), vec2(w, inner.height()));
        let resp = ui.interact(r, ui.id().with(("seg", i)), if *ok { Sense::click() } else { Sense::hover() });
        let sel = *value == *v;
        if sel {
            let raised = if dark { Color32::from_rgb(0x5a, 0x5a, 0x5e) } else { Color32::WHITE };
            ui.painter().rect_filled(r.translate(vec2(0.0, 0.5)), CornerRadius::same(6), Color32::from_black_alpha(if dark { 60 } else { 22 }));
            ui.painter().rect_filled(r, CornerRadius::same(6), raised);
        } else if resp.hovered() && *ok {
            ui.painter().rect_filled(r, CornerRadius::same(6), p.hover);
        }
        let color = if !ok { p.tertiary } else if sel { p.text } else { p.secondary };
        let font = if sel { theme::semibold(SECONDARY + 1.0) } else { egui::FontId::new(SECONDARY + 1.0, egui::FontFamily::Name("medium".into())) };
        let galley = ui.painter().layout_no_wrap(label.to_string(), font, color);
        ui.painter().galley(r.center() - galley.size() / 2.0, galley, color);
        let resp = if *ok { resp } else { resp.on_hover_text(*why) };
        if resp.clicked() && !sel {
            *value = *v;
            changed = true;
        }
    }
    changed
}

/// Up/down chevrons, as on a macOS pop-up button (for combo boxes).
pub fn popup_chevrons(ui: &Ui, rect: Rect, visuals: &egui::style::WidgetVisuals, _open: bool) {
    let c = rect.center();
    let stroke = Stroke::new(1.3, visuals.fg_stroke.color.gamma_multiply(0.8));
    let (w, h, gap) = (3.0, 2.2, 1.6);
    let painter = ui.painter();
    painter.line(vec![pos2(c.x - w, c.y - gap), pos2(c.x, c.y - gap - h), pos2(c.x + w, c.y - gap)], stroke);
    painter.line(vec![pos2(c.x - w, c.y + gap), pos2(c.x, c.y + gap + h), pos2(c.x + w, c.y + gap)], stroke);
}

/// Small line icons drawn with the painter (14×14 pt).
pub fn paint_icon(ui: &Ui, rect: Rect, icon: Icon, color: Color32) {
    let painter = ui.painter();
    let s = Stroke::new(1.3, color);
    let c = rect.center();
    let at = |x: f32, y: f32| -> Pos2 { pos2(rect.left() + x, rect.top() + y) };
    let r2 = CornerRadius::same(2);
    let outline = |r: Rect, cr: CornerRadius| painter.rect_stroke(r, cr, s, egui::StrokeKind::Middle);
    match icon {
        Icon::Scene => {
            // Two stacked layers.
            outline(Rect::from_min_max(at(1.5, 4.5), at(10.5, 12.5)), r2);
            painter.line(vec![at(4.0, 2.0), at(12.5, 2.0), at(12.5, 9.5)], s);
        }
        Icon::Display => {
            outline(Rect::from_min_max(at(1.0, 2.0), at(13.0, 10.0)), r2);
            painter.line_segment([at(7.0, 10.0), at(7.0, 12.5)], s);
            painter.line_segment([at(4.5, 12.5), at(9.5, 12.5)], s);
        }
        Icon::Camera => {
            outline(Rect::from_min_max(at(1.0, 4.0), at(9.5, 11.0)), r2);
            painter.add(Shape::convex_polygon(vec![at(10.0, 7.5), at(13.0, 5.0), at(13.0, 10.0)], Color32::TRANSPARENT, s));
        }
        Icon::Overlay => {
            outline(Rect::from_min_max(at(1.0, 2.0), at(13.0, 12.0)), r2);
            painter.circle_filled(at(9.5, 8.5), 2.0, color);
        }
        Icon::Audio => {
            // Waveform bars.
            for (x, h) in [(2.0, 3.0), (4.75, 7.0), (7.5, 10.0), (10.25, 6.0), (13.0, 2.5)] {
                painter.line_segment([pos2(rect.left() + x - 0.5, c.y - h / 2.0), pos2(rect.left() + x - 0.5, c.y + h / 2.0)], s);
            }
        }
        Icon::Output => {
            // Folder.
            painter.add(Shape::line(vec![at(1.0, 11.5), at(1.0, 3.0), at(5.0, 3.0), at(6.5, 4.5), at(13.0, 4.5), at(13.0, 11.5), at(1.0, 11.5)], s));
        }
        Icon::Replay => {
            // Circular arrow.
            let n = 20;
            let pts: Vec<Pos2> = (0..=n)
                .map(|i| {
                    let a = -2.4 + (i as f32 / n as f32) * 5.2;
                    c + vec2(a.cos(), a.sin()) * 5.0
                })
                .collect();
            let start = pts[0];
            painter.add(Shape::line(pts, s));
            painter.line(vec![start + vec2(-2.6, 0.2), start, start + vec2(0.4, -2.8)], s);
        }
        Icon::Stream => {
            painter.circle_filled(c, 1.6, color);
            for (r, span) in [(3.8, 0.75), (6.2, 0.8)] {
                for side in [0.0, std::f32::consts::PI] {
                    let pts: Vec<Pos2> = (0..=10)
                        .map(|i| {
                            let a = side - span + (i as f32 / 10.0) * span * 2.0;
                            c + vec2(a.cos(), a.sin()) * r
                        })
                        .collect();
                    painter.add(Shape::line(pts, s));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fader_scale() {
        assert_eq!(fader_pos(MIN_DB), 0.0);
        assert_eq!(fader_pos(-200.0), 0.0);
        assert_eq!(fader_pos(f32::NAN), 0.0);
        assert_eq!(fader_pos(0.0), 0.70);
        assert_eq!(fader_pos(MAX_DB), 1.0);
        assert_eq!(fader_pos(100.0), 1.0);
        assert!((fader_pos(-5.0) - 0.625).abs() < 1e-6);
        // Positions and levels map back and forth, and higher is louder.
        let mut last = f32::NEG_INFINITY;
        for i in 0..=1000 {
            let pos = i as f32 / 1000.0;
            let db = fader_db(pos);
            assert!(db > last, "{pos}");
            assert!((fader_pos(db) - pos).abs() < 1e-5, "{pos} -> {db}");
            last = db;
        }
        assert_eq!(fader_db(0.0), MIN_DB);
        assert_eq!(fader_db(1.0), MAX_DB);
        // Around unity a 300-point fader moves in steps under 0.3 dB.
        assert!(fader_db(0.70 + 1.0 / 300.0) - fader_db(0.70) < 0.3);
    }

    /// Frames of a 300-point fader driven by synthetic input.
    struct Faders {
        ctx: egui::Context,
        t: f64,
    }

    impl Faders {
        fn new() -> Self {
            Faders { ctx: egui::Context::default(), t: 0.0 }
        }

        /// One frame; returns the fader's rect.
        fn frame(&mut self, events: Vec<egui::Event>, db: &mut f32, enabled: bool) -> Rect {
            self.t += 1.0 / 60.0;
            let input = egui::RawInput {
                events,
                time: Some(self.t),
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 100.0))),
                ..Default::default()
            };
            let mut rect = Rect::NOTHING;
            let _ = self.ctx.run_ui(input, |ui| rect = fader(ui, db, 300.0, enabled, true).rect);
            rect
        }

        fn button(&mut self, at: Pos2, pressed: bool, db: &mut f32) {
            let e = egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
            self.frame(vec![egui::Event::PointerMoved(at), e], db, true);
        }

        fn drag(&mut self, from: Pos2, to: Pos2, db: &mut f32) {
            self.button(from, true, db);
            for i in 1..=5 {
                let at = from + (to - from) * (i as f32 / 5.0);
                self.frame(vec![egui::Event::PointerMoved(at)], db, true);
            }
            self.button(to, false, db);
        }
    }

    #[test]
    fn fader_follows_the_pointer() {
        let mut f = Faders::new();
        let mut db = -10.0;
        let r = f.frame(vec![], &mut db, true);
        let travel = r.shrink2(vec2(KNOB_R + 1.0, 0.0)).x_range();
        let x = |db: f32| egui::lerp(travel, fader_pos(db));
        // Grabbed off-center, the knob moves with the pointer and doesn't
        // jump to it: -10 dB plus 0.15 of the travel is 0 dB (the detent).
        let grab = pos2(x(-10.0) + 3.0, r.center().y);
        f.drag(grab, grab + vec2(0.15 * travel.span(), 0.0), &mut db);
        assert_eq!(db, 0.0);
        // A click on the track jumps there: 40% of the travel is -20 dB.
        f.button(pos2(x(-20.0), r.center().y), true, &mut db);
        f.button(pos2(x(-20.0), r.center().y), false, &mut db);
        assert!((db + 20.0).abs() <= 0.1, "{db}");
        // Dragged past the ends: silence and the full boost.
        f.drag(pos2(x(db), r.center().y), pos2(r.left() - 50.0, r.center().y), &mut db);
        assert_eq!(db, MIN_DB);
        f.drag(pos2(x(db), r.center().y), pos2(r.right() + 50.0, r.center().y), &mut db);
        assert_eq!(db, MAX_DB);
        // Double-click (well after the clicks above): back to 0 dB.
        f.t += 1.0;
        let at = pos2(x(12.0), r.center().y);
        for _ in 0..2 {
            f.button(at, true, &mut db);
            f.button(at, false, &mut db);
        }
        assert_eq!(db, 0.0);
    }

    #[test]
    fn fader_scrolls_in_fine_steps() {
        let mut f = Faders::new();
        let mut db = -6.0;
        let r = f.frame(vec![], &mut db, true);
        let wheel = egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: vec2(0.0, 40.0),
            phase: egui::TouchPhase::Move,
            modifiers: Default::default(),
        };
        f.frame(vec![egui::Event::PointerMoved(r.center()), wheel], &mut db, true);
        for _ in 0..60 {
            f.frame(vec![], &mut db, true);
        }
        // 40 points up: +1 dB.
        assert!((db + 5.0).abs() < 0.05, "{db}");
    }

    #[test]
    fn disabled_fader_ignores_input() {
        let mut f = Faders::new();
        let mut db = -6.0;
        let r = f.frame(vec![], &mut db, false);
        for pressed in [true, false] {
            let e = egui::Event::PointerButton { pos: r.left_center(), button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
            f.frame(vec![egui::Event::PointerMoved(r.left_center()), e], &mut db, false);
        }
        assert_eq!(db, -6.0);
    }

    #[test]
    fn mute_button_toggles() {
        let ctx = egui::Context::default();
        let mut muted = false;
        let mut rect = Rect::NOTHING;
        for (i, ev) in [None, Some(true), Some(false)].into_iter().enumerate() {
            let events = ev
                .map(|pressed| {
                    let pos = rect.center();
                    vec![egui::Event::PointerMoved(pos), egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() }]
                })
                .unwrap_or_default();
            let input = egui::RawInput { events, time: Some(i as f64 / 60.0), ..Default::default() };
            let _ = ctx.run_ui(input, |ui| rect = mute_button(ui, &mut muted, true).rect);
        }
        assert!(muted);
    }
}
