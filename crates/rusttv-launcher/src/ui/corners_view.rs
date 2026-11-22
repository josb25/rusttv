//! The full-screen view of a corners control (a projector's keystone,
//! corners.rs) in the window: quick.slint `CornersView`.
//!
//! Each move goes to the device at once, but never more than one at a
//! time: while one runs, only the latest points wait, and they go when it
//! ends (ui/device_view.rs `change_device`). Held arrows don't pile up
//! work behind a slow tool.

use slint::{ModelRc, SharedString, VecModel};

use super::{Controller, HomeWindow};
use crate::corners::{Change, Focus};
use crate::device::Value;
use crate::model::Input;

/// `CornersView.focus`: 0..3 a corner, these the buttons.
const FOCUS_RESET: i32 = 4;
const FOCUS_DONE: i32 = 5;

/// `corners-pointer` actions.
const HOVER: i32 = 0;
const PRESS: i32 = 1;
const DRAG: i32 = 2;
const RELEASE: i32 = 3;

impl Controller {
    /// A key while the view is open.
    pub(super) fn corners_input(&mut self, input: Input) {
        let now = u64::try_from((self.clock)().as_millis()).unwrap_or(u64::MAX);
        let Some(edit) = &mut self.corners else {
            return;
        };
        let change = edit.handle(input, now);
        self.corners_change(change);
        self.render();
    }

    /// The pointer on the view at (`x`, `y`), fractions of the screen.
    pub(super) fn corners_pointer(&mut self, action: i32, x: f32, y: f32) {
        let Some(edit) = &mut self.corners else {
            return;
        };
        let change = match action {
            HOVER => {
                edit.hover(x, y);
                None
            }
            PRESS => {
                edit.press(x, y);
                None
            }
            DRAG => edit.drag_to(x, y),
            RELEASE => {
                edit.release();
                None
            }
            _ => None,
        };
        self.corners_change(change);
        self.restart_idle();
        self.render();
    }

    /// The pointer over a button of the view (`done`: Done, else Reset);
    /// `clicked`: a click on it.
    pub(super) fn corners_button(&mut self, done: bool, clicked: bool) {
        let Some(edit) = &mut self.corners else {
            return;
        };
        edit.hover_button(done);
        if clicked {
            self.corners_input(Input::OkReleased);
            return;
        }
        self.render();
    }

    fn corners_change(&mut self, change: Option<Change>) {
        match change {
            Some(Change::Set(points)) => {
                let Some(edit) = &self.corners else {
                    return;
                };
                let (tab, control) = (edit.tab.clone(), edit.control.clone());
                self.corners_status.clear();
                if self.device_pending == 0 {
                    self.change_device(tab, control, Value::Corners(points));
                } else {
                    self.corners_queued = Some((tab, control, points));
                }
            }
            Some(Change::Close) => {
                self.corners = None;
                // The Settings row shows the new points once read again.
                if self.device_pending == 0 {
                    self.refresh_device_tabs();
                }
            }
            None => {}
        }
    }

    /// The device finished a change: the latest waiting points go now.
    pub(super) fn corners_flush(&mut self) {
        if self.device_pending > 0 {
            return;
        }
        if let Some((tab, control, points)) = self.corners_queued.take() {
            self.change_device(tab, control, Value::Corners(points));
        }
    }

    pub(super) fn render_corners(&self, window: &HomeWindow) {
        let Some(edit) = &self.corners else {
            window.set_corners_shown(false);
            return;
        };
        window.set_corners_title(edit.title.as_str().into());
        window.set_corners_focus(match edit.focus() {
            Focus::Corner(corner) => i32::try_from(corner).unwrap_or(0),
            Focus::Reset => FOCUS_RESET,
            Focus::Done => FOCUS_DONE,
        });
        let offsets: Vec<SharedString> = (0..4)
            .map(|corner| match edit.offset(corner) {
                [0, 0] => SharedString::new(),
                [x, y] => format!("{x:+}, {y:+}").into(),
            })
            .collect();
        window.set_corners_offsets(ModelRc::new(VecModel::from(offsets)));
        window.set_corners_status(self.corners_status.as_str().into());
        window.set_corners_shown(true);
    }
}
