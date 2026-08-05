//! Tracks `wl_callback`s still awaiting their `done`, so a reconnect can
//! answer any the old compositor died holding a promise on. Recreating
//! the objects a callback depends on isn't enough on its own -- the
//! promise itself was never fulfilled, and two distinct requests create a
//! callback a client can block on forever if it isn't:
//!
//! - `wl_surface.frame()`: GTK's frame clock blocks on this one's `done`
//!   before rendering again.
//! - `wl_display.sync()`: Mesa's GL renderer sends one after every
//!   `wl_surface.commit()` on its own private queue, as an internal
//!   commit-confirmation roundtrip, and blocks uninterruptibly (a plain
//!   `poll()`, not dispatching anything else) until it's answered.
//!
//! Same "unanswered promise" shape as `buffer_flow.rs`'s
//! `wl_buffer.release` tracking.

use std::collections::HashSet;

#[derive(Default)]
pub struct PendingFrameTracker {
    /// Guest ids of `wl_callback`s created by `frame()`/`sync()` with no
    /// `done` seen since. An untranslatable sender is answered immediately
    /// elsewhere and never reaches this set at all.
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

    /// `wl_display.sync()` was processed -- same reasoning and same
    /// underlying tracking as `on_frame_requested`, kept as a separate
    /// method so each `relay_ready_messages` call site stays
    /// self-documenting about which request actually created the
    /// callback it's registering.
    pub fn on_sync_requested(&mut self, callback_guest_id: u32) {
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

    #[test]
    fn sync_request_is_tracked_the_same_way_as_a_frame_request() {
        let mut t = PendingFrameTracker::new();
        t.on_sync_requested(62);
        assert_eq!(t.drain_awaiting_done(), vec![62]);
    }

    #[test]
    fn frame_and_sync_callbacks_are_tracked_independently_of_each_other() {
        let mut t = PendingFrameTracker::new();
        t.on_frame_requested(60);
        t.on_sync_requested(62);
        t.on_done_received(62);
        assert_eq!(t.drain_awaiting_done(), vec![60]);
    }
}
