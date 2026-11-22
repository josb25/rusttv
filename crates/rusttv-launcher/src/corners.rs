//! Adjusting a [`ControlKind::Corners`] control (a projector's four-corner
//! keystone) full screen: a grid pattern with the four corners, one of
//! them focused. No UI code here (ui/corners_view.rs draws it).
//!
//! Every input drives it: the arrows (remote, keyboard, gamepad) move the
//! focused corner, faster while held; OK goes to the next corner, after
//! the last to Done (Reset is beside it); Back is Done. The pointer focuses what it is over (a
//! quarter of the screen is its corner) and drags the focused corner.
//!
//! The picture itself shows the result once the device warps it; until
//! then each corner's offset is the only feedback.
//!
//! [`ControlKind::Corners`]: crate::device::ControlKind::Corners

use crate::model::Input;

/// What has the focus. OK walks around the picture, then to Done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Focus {
    /// A corner: 0 top left, 1 top right, 2 bottom left, 3 bottom right
    /// (the order of the corners' numbers).
    Corner(usize),
    Reset,
    Done,
}

/// The corners in the order OK visits them: clockwise from top left.
const RING: [usize; 4] = [0, 1, 3, 2];

/// The smallest and largest step of an arrow, in panel pixels.
const SLOW: i32 = 2;
const FAST: i32 = 24;
/// A repeat of the same arrow within this time speeds up (ms).
const HELD_MS: u64 = 200;

/// Something ui.rs must do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Change {
    /// Send these corners to the device (Reset sends the panel's own).
    Set([i32; 8]),
    /// Leave the adjust view.
    Close,
}

/// The open adjust view.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CornerEdit {
    pub tab: String,
    pub control: String,
    /// The control's label, the view's title.
    pub title: String,
    /// The panel, in pixels.
    size: [i32; 2],
    /// x, y of each corner (panel pixels).
    points: [i32; 8],
    focus: Focus,
    /// The arrow last applied, when (ms), and the step it took.
    last: Option<(Input, u64, i32)>,
    /// A drag: the pointer where it started (fractions of the screen) and
    /// the corner then.
    drag: Option<([f32; 2], [i32; 2])>,
}

impl CornerEdit {
    pub fn new(
        tab: String,
        control: String,
        title: String,
        points: [i32; 8],
        size: [i32; 2],
    ) -> Self {
        Self {
            tab,
            control,
            title,
            size,
            points,
            focus: Focus::Corner(0),
            last: None,
            drag: None,
        }
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    #[cfg(test)]
    pub fn points(&self) -> [i32; 8] {
        self.points
    }

    /// The corners with no correction: the panel's own.
    fn full(&self) -> [i32; 8] {
        let [w, h] = self.size;
        [0, 0, w - 1, 0, 0, h - 1, w - 1, h - 1]
    }

    /// Corner `corner`'s offset from where it is without correction.
    pub fn offset(&self, corner: usize) -> [i32; 2] {
        let full = self.full();
        [
            self.points[2 * corner] - full[2 * corner],
            self.points[2 * corner + 1] - full[2 * corner + 1],
        ]
    }

    /// Moves corner `corner` to (x, y), kept in its own quarter of the
    /// panel; whether it moved. A move that would fold the picture (not
    /// a convex shape; the device refuses those) doesn't happen.
    fn place(&mut self, corner: usize, x: i32, y: i32) -> bool {
        let [w, h] = self.size;
        let (left, top) = (corner.is_multiple_of(2), corner < 2);
        let x = if left {
            x.clamp(0, w / 2 - 1)
        } else {
            x.clamp(w / 2, w - 1)
        };
        let y = if top {
            y.clamp(0, h / 2 - 1)
        } else {
            y.clamp(h / 2, h - 1)
        };
        let mut points = self.points;
        points[2 * corner] = x;
        points[2 * corner + 1] = y;
        if points == self.points || !convex(&points) {
            return false;
        }
        self.points = points;
        true
    }

    /// A key at `now` (ms).
    pub fn handle(&mut self, input: Input, now: u64) -> Option<Change> {
        let direction = match input {
            Input::Left => Some((-1, 0)),
            Input::Right => Some((1, 0)),
            Input::Up => Some((0, -1)),
            Input::Down => Some((0, 1)),
            _ => None,
        };
        if let Some((dx, dy)) = direction {
            let Focus::Corner(corner) = self.focus else {
                // On the buttons Left and Right move between them.
                match (self.focus, input) {
                    (Focus::Reset, Input::Right) => self.focus = Focus::Done,
                    (Focus::Done, Input::Left) => self.focus = Focus::Reset,
                    _ => {}
                }
                return None;
            };
            let step = match self.last {
                Some((last, at, step)) if last == input && now.saturating_sub(at) <= HELD_MS => {
                    (step * 3 / 2 + 1).min(FAST)
                }
                _ => SLOW,
            };
            self.last = Some((input, now, step));
            let (x, y) = (self.points[2 * corner], self.points[2 * corner + 1]);
            return self
                .place(corner, x + dx * step, y + dy * step)
                .then_some(Change::Set(self.points));
        }
        self.last = None;
        match input {
            Input::OkReleased => match self.focus {
                Focus::Corner(corner) => {
                    let next = RING.iter().position(|c| *c == corner).map_or(0, |i| i + 1);
                    self.focus = RING.get(next).map_or(Focus::Done, |c| Focus::Corner(*c));
                    None
                }
                Focus::Reset => {
                    self.points = self.full();
                    self.focus = Focus::Corner(0);
                    Some(Change::Set(self.points))
                }
                Focus::Done => Some(Change::Close),
            },
            Input::Back => Some(Change::Close),
            _ => None,
        }
    }

    /// The pointer over (x, y), in fractions of the screen: it focuses
    /// the corner of that quarter (not while dragging).
    pub fn hover(&mut self, x: f32, y: f32) {
        if self.drag.is_none() {
            self.focus = Focus::Corner(quarter(x, y));
        }
    }

    /// The pointer over a button: it has the focus.
    pub fn hover_button(&mut self, done: bool) {
        if self.drag.is_none() {
            self.focus = if done { Focus::Done } else { Focus::Reset };
        }
    }

    /// A press at (x, y): its quarter's corner is focused and dragged.
    pub fn press(&mut self, x: f32, y: f32) {
        let corner = quarter(x, y);
        self.focus = Focus::Corner(corner);
        self.drag = Some((
            [x, y],
            [self.points[2 * corner], self.points[2 * corner + 1]],
        ));
    }

    /// The pointer moved to (x, y) while pressed.
    pub fn drag_to(&mut self, x: f32, y: f32) -> Option<Change> {
        let ([x0, y0], [px, py]) = self.drag?;
        let Focus::Corner(corner) = self.focus else {
            return None;
        };
        let [w, h] = self.size;
        // Pixels on screen are panel pixels: the window fills the panel.
        let pixels = |share: f32, size: i32| (f64::from(share) * f64::from(size)).round() as i32;
        let (dx, dy) = (pixels(x - x0, w), pixels(y - y0, h));
        self.place(corner, px + dx, py + dy)
            .then_some(Change::Set(self.points))
    }

    pub fn release(&mut self) {
        self.drag = None;
    }
}

/// Whether the corners make a convex shape, walked around the picture.
fn convex(points: &[i32; 8]) -> bool {
    let at = |corner: usize| {
        (
            i64::from(points[2 * corner]),
            i64::from(points[2 * corner + 1]),
        )
    };
    let mut sign = 0;
    for i in 0..4 {
        let (a, b, c) = (at(RING[i]), at(RING[(i + 1) % 4]), at(RING[(i + 2) % 4]));
        let cross = (b.0 - a.0) * (c.1 - b.1) - (b.1 - a.1) * (c.0 - b.0);
        let side = cross.signum();
        if side == 0 || (sign != 0 && side != sign) {
            return false;
        }
        sign = side;
    }
    true
}

/// The corner whose quarter (x, y) is in (fractions of the screen).
fn quarter(x: f32, y: f32) -> usize {
    usize::from(x >= 0.5) + 2 * usize::from(y >= 0.5)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit() -> CornerEdit {
        CornerEdit::new(
            "projector".into(),
            "keystone".into(),
            "Keystone".into(),
            [0, 0, 1919, 0, 0, 1079, 1919, 1079],
            [1920, 1080],
        )
    }

    #[test]
    fn arrows_move_the_focused_corner_faster_while_held() {
        let mut edit = edit();
        assert_eq!(
            edit.handle(Input::Right, 0),
            Some(Change::Set([2, 0, 1919, 0, 0, 1079, 1919, 1079]))
        );
        // Held: each repeat a bigger step, up to FAST.
        let mut at = 0;
        let mut steps = Vec::new();
        for _ in 0..8 {
            at += 50;
            let before = edit.points()[0];
            edit.handle(Input::Right, at);
            steps.push(edit.points()[0] - before);
        }
        assert_eq!(steps, [4, 7, 11, 17, 24, 24, 24, 24]);
        // A pause: slow again.
        edit.handle(Input::Right, at + 1000);
        assert_eq!(edit.offset(0)[0], 2 + steps.iter().sum::<i32>() + 2);
        // Up at the top edge: nothing to send.
        assert_eq!(edit.handle(Input::Up, at + 2000), None);
    }

    #[test]
    fn a_corner_stays_in_its_quarter() {
        let mut edit = edit();
        // Right across: stops at the middle.
        edit.press(0.1, 0.1);
        assert_eq!(
            edit.drag_to(0.9, 0.1),
            Some(Change::Set([959, 0, 1919, 0, 0, 1079, 1919, 1079]))
        );
        edit.release();
        assert_eq!(edit.offset(0), [959, 0]);
        edit.handle(Input::Left, 0);
        edit.handle(Input::Left, 1000);
        // The bottom right corner, dragged up and left by a tenth.
        edit.press(0.9, 0.9);
        edit.drag_to(0.8, 0.8);
        assert_eq!(edit.offset(3), [-192, -108]);
        assert_eq!(edit.offset(0), [955, 0]);
    }

    #[test]
    fn a_move_that_would_fold_the_picture_stays_undone() {
        assert!(convex(&[0, 0, 1919, 0, 0, 1079, 1919, 1079]));
        assert!(!convex(&[0, 0, 1919, 0, 959, 540, 960, 539]));
        // Top right and bottom left pulled into the middle: top left can't
        // go far in, or the picture folds along that diagonal.
        let mut edit = CornerEdit::new(
            "projector".into(),
            "keystone".into(),
            "Keystone".into(),
            [0, 0, 960, 539, 959, 540, 1919, 1079],
            [1920, 1080],
        );
        edit.press(0.1, 0.1);
        assert_eq!(edit.drag_to(0.468_75, 0.562_5), None);
        // A little way in is fine.
        assert!(edit.drag_to(0.15, 0.15).is_some());
        assert_eq!(edit.offset(0), [96, 54]);
    }

    #[test]
    fn ok_walks_around_the_corners_then_to_done() {
        let mut edit = edit();
        let mut seen = vec![edit.focus()];
        for _ in 0..4 {
            assert_eq!(edit.handle(Input::OkReleased, 0), None);
            seen.push(edit.focus());
        }
        assert_eq!(
            seen,
            [
                Focus::Corner(0),
                Focus::Corner(1),
                Focus::Corner(3),
                Focus::Corner(2),
                Focus::Done,
            ]
        );
        assert_eq!(edit.handle(Input::Left, 0), None);
        assert_eq!(edit.focus(), Focus::Reset);
        edit.handle(Input::Right, 0);
        assert_eq!(edit.handle(Input::OkReleased, 0), Some(Change::Close));
    }

    #[test]
    fn reset_puts_the_corners_back_and_back_closes() {
        let mut edit = edit();
        edit.handle(Input::Down, 0);
        edit.hover_button(false);
        assert_eq!(
            edit.handle(Input::OkReleased, 0),
            Some(Change::Set([0, 0, 1919, 0, 0, 1079, 1919, 1079]))
        );
        assert_eq!(edit.points(), [0, 0, 1919, 0, 0, 1079, 1919, 1079]);
        assert_eq!(edit.focus(), Focus::Corner(0));
        assert_eq!(edit.handle(Input::Back, 0), Some(Change::Close));
    }

    #[test]
    fn the_pointer_focuses_the_corner_of_its_quarter() {
        let mut edit = edit();
        edit.hover(0.7, 0.2);
        assert_eq!(edit.focus(), Focus::Corner(1));
        edit.hover(0.2, 0.7);
        assert_eq!(edit.focus(), Focus::Corner(2));
        edit.hover_button(true);
        assert_eq!(edit.focus(), Focus::Done);
    }
}
