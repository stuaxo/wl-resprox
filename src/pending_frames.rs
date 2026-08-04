//! Tracks `wl_callback`s that are still awaiting their `done` event,
//! purely so a reconnect can answer any that the OLD compositor died
//! holding a promise on. Two independent request shapes create such a
//! callback and both are tracked here, since neither's `done` arriving
//! late is survivable for a real client:
//!
//! - `wl_surface.frame()` -- found live 2026-08-04, one step past the
//!   `wl_buffer.release` synthesis fix (`buffer_flow.rs`) for the same
//!   live validation run: a real gtk4-demo-like client's LAST `frame()`
//!   before a crash reached the old compositor just fine (successfully
//!   forwarded, no drop, no synthesis -- the `relay_ready_messages`
//!   branch that already handles a *dropped* frame(), because the
//!   surface itself was momentarily untranslatable during the narrow
//!   post-`bump_generation()` recovery window, never fires for this case
//!   at all) -- but the compositor died before ever sending back the
//!   matching `wl_callback.done`. Recreating every object involved
//!   (surface, buffer, even a synthesized `wl_buffer.release` for the
//!   buffer that commit was carrying) still left the client stalled
//!   forever, since GTK's own frame clock blocks specifically on that one
//!   `wl_callback.done`, and nothing about object recreation answers a
//!   promise that was never fulfilled in the first place.
//! - `wl_display.sync()` -- found live 2026-08-04 chasing ADR-0008's
//!   dmabuf client-wedge bug: Mesa's GL renderer sends one of these after
//!   *every* `wl_surface.commit()` on its own private queue, as an
//!   internal commit-confirmation roundtrip, and blocks (a plain,
//!   uninterruptible `poll()`, confirmed via `/proc/<pid>/stack` and a
//!   `WAYLAND_DEBUG=1` trace on a real hang) until that specific
//!   callback's `done` arrives. The one sent immediately before a crash
//!   reaches the old compositor the same way a `frame()` can -- forwarded
//!   fine, never answered -- and this tracker had no hook for it at all
//!   (only `wl_surface.frame()` was tracked), so recreating everything
//!   else still left the client permanently wedged on this one
//!   unanswered `sync()`.
//!
//! Same "unanswered promise" class as `buffer_flow.rs`, and the
//! *dropped*-frame() case already handled inline in `relay_ready_messages`
//! -- both entries here are the variant where a callback-creating request
//! was neither dropped nor answered, just abandoned mid-flight.

use std::collections::HashSet;

#[derive(Default)]
pub struct PendingFrameTracker {
    /// Guest ids of `wl_callback` objects created by a `wl_surface.frame()`
    /// or `wl_display.sync()` that was forwarded (or dropped only because
    /// the connection was frozen, not because the sender was
    /// untranslatable -- that case synthesizes done+delete_id immediately
    /// and never reaches this tracker at all) with no `done` event seen
    /// since.
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
