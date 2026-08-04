//! Tracks `wl_callback`s created by `wl_surface.frame()` that are still
//! awaiting their `done` event, purely so a reconnect can answer any that
//! the OLD compositor died holding a promise on.
//!
//! Found live 2026-08-04, one step past the `wl_buffer.release` synthesis
//! fix (`buffer_flow.rs`) for the same live validation run: a real
//! gtk4-demo-like client's LAST `frame()` before a crash reached the old
//! compositor just fine (successfully forwarded, no drop, no synthesis --
//! the `relay_ready_messages` branch that already handles a *dropped*
//! frame(), because the surface itself was momentarily untranslatable
//! during the narrow post-`bump_generation()` recovery window, never fires
//! for this case at all) -- but the compositor died before ever sending
//! back the matching `wl_callback.done`. Recreating every object involved
//! (surface, buffer, even a synthesized `wl_buffer.release` for the
//! buffer that commit was carrying) still left the client stalled forever,
//! since GTK's own frame clock blocks specifically on that one
//! `wl_callback.done`, and nothing about object recreation answers a
//! promise that was never fulfilled in the first place. Same "unanswered
//! promise" class as `buffer_flow.rs`, and the *dropped*-frame() case
//! already handled inline in `relay_ready_messages` -- this is the third,
//! previously-unhandled variant: a frame() that was neither dropped nor
//! answered, just abandoned mid-flight.

use std::collections::HashSet;

#[derive(Default)]
pub struct PendingFrameTracker {
    /// Guest ids of `wl_callback` objects created by a `wl_surface.frame()`
    /// that was forwarded (or dropped only because the connection was
    /// frozen, not because the sender was untranslatable -- that case
    /// synthesizes done+delete_id immediately and never reaches this
    /// tracker at all) with no `done` event seen since.
    awaiting_done: HashSet<u32>,
}

impl PendingFrameTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// `wl_surface.frame()` was processed (forwarded live, or silently
    /// dropped while frozen -- either way, `new_id` was allocated and no
    /// synthesis has happened for it yet).
    pub fn on_frame_requested(&mut self, callback_guest_id: u32) {
        self.awaiting_done.insert(callback_guest_id);
    }

    /// `wl_callback.done` legitimately arrived from the host -- the
    /// promise is fulfilled, nothing left to synthesize.
    pub fn on_done_received(&mut self, callback_guest_id: u32) {
        self.awaiting_done.remove(&callback_guest_id);
    }

    /// Every callback still awaiting a `done` -- a compositor crash
    /// between `frame()` and the `done` it would eventually have sent.
    /// Draining (not just reading) is deliberate, same reasoning as
    /// `BufferFlowTracker::drain_in_flight`: none of these are this
    /// tracker's concern any more after a reconnect, whether or not the
    /// caller manages to actually synthesize a response for each one.
    pub fn drain_awaiting_done(&mut self) -> Vec<u32> {
        self.awaiting_done.drain().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_request_is_tracked_until_done_arrives() {
        let mut t = PendingFrameTracker::new();
        t.on_frame_requested(60);
        assert_eq!(t.drain_awaiting_done(), vec![60]);
    }

    #[test]
    fn done_received_clears_the_pending_entry() {
        let mut t = PendingFrameTracker::new();
        t.on_frame_requested(60);
        t.on_done_received(60);
        assert!(t.drain_awaiting_done().is_empty());
    }

    #[test]
    fn drain_clears_state_so_it_isnt_reported_again() {
        let mut t = PendingFrameTracker::new();
        t.on_frame_requested(60);
        assert_eq!(t.drain_awaiting_done(), vec![60]);
        assert!(t.drain_awaiting_done().is_empty(), "a second drain should be empty");
    }

    #[test]
    fn tracks_multiple_outstanding_callbacks_independently() {
        let mut t = PendingFrameTracker::new();
        t.on_frame_requested(60);
        t.on_frame_requested(61);
        t.on_done_received(60);
        assert_eq!(t.drain_awaiting_done(), vec![61]);
    }
}
