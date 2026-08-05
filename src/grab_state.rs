//! Tracks pointer/keyboard focus and pressed-button state, purely so a
//! reconnect can clean it up before resuming traffic: if a grab was
//! active when the compositor died, synthesize `wl_pointer.leave` and/or
//! a fake button-release first, so the client doesn't end up believing a
//! button is still held or a surface still has focus that the new
//! compositor will never confirm or release on its own. A stuck grab is
//! worse than a dropped click.
//!
//! Deliberately narrow, matching that rule's own scope: only *focus*
//! (which surface last got an `enter` without a matching `leave`) and
//! *pressed buttons* (button events seen without a matching release) are
//! tracked -- not axis/motion/modifier state, which don't leave anything
//! "stuck" if simply dropped.

use std::collections::HashMap;

#[derive(Default)]
struct PointerState {
    /// Surface (guest id) the pointer last entered, if no `leave` has
    /// been seen since.
    entered_surface: Option<u32>,
    /// Linux input event codes (e.g. `BTN_LEFT`) currently pressed, per
    /// the last `wl_pointer.button` seen for each.
    pressed_buttons: Vec<u32>,
}

#[derive(Default)]
struct KeyboardState {
    entered_surface: Option<u32>,
}

#[derive(Default)]
pub struct GrabTracker {
    pointers: HashMap<u32, PointerState>,
    keyboards: HashMap<u32, KeyboardState>,
}

/// Matches `enum wl_pointer_button_state { Released = 0, Pressed = 1 }`.
const BUTTON_STATE_PRESSED: u32 = 1;

impl GrabTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_pointer_enter(&mut self, pointer_guest_id: u32, surface_guest_id: u32) {
        self.pointers.entry(pointer_guest_id).or_default().entered_surface = Some(surface_guest_id);
    }

    pub fn on_pointer_leave(&mut self, pointer_guest_id: u32) {
        if let Some(state) = self.pointers.get_mut(&pointer_guest_id) {
            state.entered_surface = None;
        }
    }

    pub fn on_pointer_button(&mut self, pointer_guest_id: u32, button: u32, state: u32) {
        let pressed = self.pointers.entry(pointer_guest_id).or_default();
        if state == BUTTON_STATE_PRESSED {
            if !pressed.pressed_buttons.contains(&button) {
                pressed.pressed_buttons.push(button);
            }
        } else {
            pressed.pressed_buttons.retain(|&b| b != button);
        }
    }

    pub fn on_keyboard_enter(&mut self, keyboard_guest_id: u32, surface_guest_id: u32) {
        self.keyboards.entry(keyboard_guest_id).or_default().entered_surface = Some(surface_guest_id);
    }

    pub fn on_keyboard_leave(&mut self, keyboard_guest_id: u32) {
        if let Some(state) = self.keyboards.get_mut(&keyboard_guest_id) {
            state.entered_surface = None;
        }
    }

    /// Every currently-active grab that needs releasing: `(pointer_guest_id,
    /// surface_guest_id, pressed_buttons)` for each pointer with focus
    /// and/or pressed buttons, then `(keyboard_guest_id, surface_guest_id)`
    /// for each keyboard with focus. Doesn't clear any state itself --
    /// callers should call `clear` once they've actually sent the
    /// synthetic release events, not before (see `recover_state_after_reconnect`'s
    /// caller in `run_connection`).
    pub fn active_pointer_grabs(&self) -> impl Iterator<Item = (u32, Option<u32>, &[u32])> + '_ {
        self.pointers
            .iter()
            .filter(|(_, s)| s.entered_surface.is_some() || !s.pressed_buttons.is_empty())
            .map(|(&id, s)| (id, s.entered_surface, s.pressed_buttons.as_slice()))
    }

    pub fn active_keyboard_grabs(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.keyboards.iter().filter_map(|(&id, s)| s.entered_surface.map(|surface| (id, surface)))
    }

    /// Resets all tracked state -- call after synthesizing releases for
    /// everything `active_pointer_grabs`/`active_keyboard_grabs` reported,
    /// since a fresh compositor connection starts with no focus/grabs of
    /// its own and will re-establish them independently.
    pub fn clear(&mut self) {
        self.pointers.clear();
        self.keyboards.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_pointer_enter_and_leave() {
        let mut t = GrabTracker::new();
        t.on_pointer_enter(10, 6);
        let grabs: Vec<_> = t.active_pointer_grabs().collect();
        assert_eq!(grabs, vec![(10, Some(6), &[][..])]);

        t.on_pointer_leave(10);
        assert_eq!(t.active_pointer_grabs().count(), 0);
    }

    #[test]
    fn tracks_pressed_buttons_independent_of_focus() {
        let mut t = GrabTracker::new();
        t.on_pointer_enter(10, 6);
        t.on_pointer_button(10, 272, BUTTON_STATE_PRESSED);
        t.on_pointer_leave(10); // focus cleared, but button still held
        let grabs: Vec<_> = t.active_pointer_grabs().collect();
        assert_eq!(grabs, vec![(10, None, &[272][..])]);

        t.on_pointer_button(10, 272, 0); // released
        assert_eq!(t.active_pointer_grabs().count(), 0);
    }

    #[test]
    fn tracks_multiple_pressed_buttons() {
        let mut t = GrabTracker::new();
        t.on_pointer_button(10, 272, BUTTON_STATE_PRESSED);
        t.on_pointer_button(10, 273, BUTTON_STATE_PRESSED);
        let grabs: Vec<_> = t.active_pointer_grabs().collect();
        assert_eq!(grabs.len(), 1);
        assert_eq!(grabs[0].2.len(), 2);
    }

    #[test]
    fn tracks_keyboard_enter_and_leave() {
        let mut t = GrabTracker::new();
        t.on_keyboard_enter(11, 6);
        assert_eq!(t.active_keyboard_grabs().collect::<Vec<_>>(), vec![(11, 6)]);
        t.on_keyboard_leave(11);
        assert_eq!(t.active_keyboard_grabs().count(), 0);
    }

    #[test]
    fn clear_resets_everything() {
        let mut t = GrabTracker::new();
        t.on_pointer_enter(10, 6);
        t.on_keyboard_enter(11, 6);
        t.clear();
        assert_eq!(t.active_pointer_grabs().count(), 0);
        assert_eq!(t.active_keyboard_grabs().count(), 0);
    }

    #[test]
    fn no_grabs_by_default() {
        let t = GrabTracker::new();
        assert_eq!(t.active_pointer_grabs().count(), 0);
        assert_eq!(t.active_keyboard_grabs().count(), 0);
    }
}
