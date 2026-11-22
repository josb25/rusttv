//! What the app overlay holds and how the keys move through it: the app's
//! size, the volume, the device's own side-menu controls (a projector's
//! focus, from its device file) and closing the app, one row each, in a
//! column.

use crate::device::{Volume, VolumeChange};
use crate::linux::device_file::SideRow;
use crate::model::SizeStep;

/// A row of the overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    /// The app's size: Left and Right step it, OK goes to the next size.
    Size,
    /// The volume: Left and Right step it, OK mutes or unmutes.
    Volume,
    /// Device control `n` of [`Panel::side`]: Left and Right step it.
    Device(usize),
    /// OK closes the app.
    Close,
}

/// What an input did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Nothing changed.
    None,
    /// Something shown changed: draw again.
    Redraw,
    /// Change the volume (then draw again with the new one).
    Volume(VolumeChange),
    /// Step device control `n` of [`Panel::side`] (`1` right, `-1` left).
    Nudge(usize, i32),
    /// Close the overlay and the app.
    CloseApp,
    /// Close the overlay.
    Dismiss,
}

/// The overlay's state.
#[derive(Debug, Clone)]
pub struct Panel {
    /// The app's size in percent; `None` where apps can't be resized.
    pub size: Option<u32>,
    /// The size when the overlay opened.
    start_size: Option<u32>,
    /// The volume; `None` without volume control.
    pub volume: Option<Volume>,
    /// The device's own side-menu controls.
    pub side: Vec<SideRow>,
    focus: usize,
}

impl Panel {
    /// The first row has the focus.
    pub fn new(size: Option<u32>, volume: Option<Volume>, side: Vec<SideRow>) -> Self {
        Self {
            size,
            start_size: size,
            volume,
            side,
            focus: 0,
        }
    }

    /// The rows there are, top to bottom.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        if self.size.is_some() {
            rows.push(Row::Size);
        }
        if self.volume.is_some() {
            rows.push(Row::Volume);
        }
        rows.extend((0..self.side.len()).map(Row::Device));
        rows.push(Row::Close);
        rows
    }

    /// The focused row's index in [`Panel::rows`].
    pub fn focus(&self) -> usize {
        self.focus.min(self.rows().len().saturating_sub(1))
    }

    /// Focuses row `index`; `true` if that changed the focus.
    pub fn focus_on(&mut self, index: usize) -> bool {
        let index = index.min(self.rows().len().saturating_sub(1));
        let changed = index != self.focus();
        self.focus = index;
        changed
    }

    /// Focuses the row of device control `id` of tab `tab`; `true` if
    /// that changed the focus.
    pub fn focus_control(&mut self, tab: &str, id: &str) -> bool {
        let Some(n) = self
            .side
            .iter()
            .position(|row| row.tab == tab && row.id == id)
        else {
            return false;
        };
        let rows = self.rows();
        let index = rows
            .iter()
            .position(|row| *row == Row::Device(n))
            .unwrap_or(0);
        self.focus_on(index)
    }

    /// The device control `key` steps, and which way (`1` right, `-1`
    /// left).
    pub fn device_key(&self, key: u32) -> Option<(usize, i32)> {
        self.side
            .iter()
            .enumerate()
            .find_map(|(n, row)| match row.keys {
                Some([left, _]) if left == key => Some((n, -1)),
                Some([_, right]) if right == key => Some((n, 1)),
                _ => None,
            })
    }

    /// Whether the focused row wants the test pattern over the screen.
    pub fn shows_pattern(&self) -> bool {
        match self.rows().get(self.focus()) {
            Some(Row::Device(n)) => self.side.get(*n).is_some_and(|row| row.pattern),
            _ => false,
        }
    }

    /// Whether the size differs from when the overlay opened (the app
    /// restarts for it).
    pub fn resized(&self) -> bool {
        self.size != self.start_size
    }

    /// The focus moves up (`-1`) or down (`1`).
    pub fn move_focus(&mut self, by: isize) -> Effect {
        let target = self.focus().saturating_add_signed(by);
        if self.focus_on(target) {
            Effect::Redraw
        } else {
            Effect::None
        }
    }

    /// Row `index` one step bigger or louder (`up`), or smaller or quieter.
    pub fn step(&mut self, index: usize, up: bool) -> Effect {
        match self.rows().get(index) {
            Some(Row::Size) => self.resize(if up {
                SizeStep::Bigger
            } else {
                SizeStep::Smaller
            }),
            Some(Row::Volume) => Effect::Volume(if up {
                VolumeChange::Up
            } else {
                VolumeChange::Down
            }),
            Some(Row::Device(n)) => Effect::Nudge(*n, if up { 1 } else { -1 }),
            Some(Row::Close) | None => Effect::None,
        }
    }

    /// OK on row `index`.
    pub fn activate(&mut self, index: usize) -> Effect {
        match self.rows().get(index) {
            Some(Row::Size) => self.resize(SizeStep::Cycle),
            Some(Row::Volume) => Effect::Volume(VolumeChange::Mute),
            Some(Row::Close) => Effect::CloseApp,
            Some(Row::Device(_)) | None => Effect::None,
        }
    }

    fn resize(&mut self, step: SizeStep) -> Effect {
        let Some(size) = self.size else {
            return Effect::None;
        };
        let new = step.apply(size);
        if new == size {
            return Effect::None;
        }
        self.size = Some(new);
        Effect::Redraw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(level: f32) -> Option<Volume> {
        Some(Volume {
            level,
            muted: false,
        })
    }

    #[test]
    fn rows_follow_what_the_app_and_device_offer() {
        assert_eq!(
            Panel::new(Some(100), volume(0.5), Vec::new()).rows(),
            [Row::Size, Row::Volume, Row::Close]
        );
        assert_eq!(Panel::new(None, None, Vec::new()).rows(), [Row::Close]);
    }

    #[test]
    fn the_focus_moves_and_stays_inside() {
        let mut panel = Panel::new(Some(100), volume(0.5), Vec::new());
        assert_eq!(panel.move_focus(-1), Effect::None);
        assert_eq!(panel.move_focus(1), Effect::Redraw);
        assert_eq!(panel.move_focus(1), Effect::Redraw);
        assert_eq!(panel.move_focus(1), Effect::None);
        assert_eq!(panel.focus(), 2);
        assert!(!panel.focus_on(2));
        // Past the last row is the last row.
        assert!(panel.focus_on(0));
        assert!(panel.focus_on(9));
        assert_eq!(panel.focus(), 2);
    }

    #[test]
    fn size_steps_cycle_and_count_as_resized() {
        let mut panel = Panel::new(Some(100), volume(0.5), Vec::new());
        assert_eq!(panel.step(0, true), Effect::Redraw);
        assert_eq!(panel.size, Some(110));
        assert!(panel.resized());
        assert_eq!(panel.step(0, false), Effect::Redraw);
        assert!(!panel.resized());
        panel.size = Some(200);
        assert_eq!(panel.step(0, true), Effect::None);
        assert_eq!(panel.activate(0), Effect::Redraw);
        assert_eq!(panel.size, Some(75));
    }

    fn focus_row(pattern: bool) -> SideRow {
        SideRow {
            tab: "projector".into(),
            id: "focus".into(),
            label: "Focus".into(),
            pattern,
            keys: Some([202, 203]),
        }
    }

    #[test]
    fn device_rows_sit_before_close_and_step_their_control() {
        let mut panel = Panel::new(Some(100), volume(0.5), vec![focus_row(true)]);
        assert_eq!(
            panel.rows(),
            [Row::Size, Row::Volume, Row::Device(0), Row::Close]
        );
        assert_eq!(panel.step(2, true), Effect::Nudge(0, 1));
        assert_eq!(panel.step(2, false), Effect::Nudge(0, -1));
        assert_eq!(panel.activate(2), Effect::None);
        // The pattern only while its row has the focus.
        assert!(!panel.shows_pattern());
        assert!(panel.focus_control("projector", "focus"));
        assert_eq!(panel.focus(), 2);
        assert!(panel.shows_pattern());
        assert!(!panel.focus_control("projector", "zoom"));
        assert_eq!(panel.device_key(202), Some((0, -1)));
        assert_eq!(panel.device_key(203), Some((0, 1)));
        assert_eq!(panel.device_key(105), None);
        let plain = Panel::new(None, None, vec![focus_row(false)]);
        assert!(!Panel { focus: 0, ..plain }.shows_pattern());
    }

    #[test]
    fn volume_and_close_rows_ask_for_their_change() {
        let mut panel = Panel::new(None, volume(0.5), Vec::new());
        assert_eq!(panel.step(0, true), Effect::Volume(VolumeChange::Up));
        assert_eq!(panel.step(0, false), Effect::Volume(VolumeChange::Down));
        assert_eq!(panel.activate(0), Effect::Volume(VolumeChange::Mute));
        assert_eq!(panel.step(1, true), Effect::None);
        assert_eq!(panel.activate(1), Effect::CloseApp);
    }
}
