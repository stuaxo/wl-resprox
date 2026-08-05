//! Tracks which `wl_buffer`s are "in flight" -- attached and committed to
//! a surface, with the client waiting on the compositor's own
//! `wl_buffer.release` event before it's allowed to reuse that buffer's
//! memory for a new frame -- purely so a reconnect can synthesize a
//! release for any buffer left in-flight when the compositor died.
//! Recreating the buffer object alone isn't enough: a client whose whole
//! pool was checked out at crash time won't attempt a new frame while it
//! believes every buffer it owns is still busy, even though every
//! protocol object involved recovered cleanly. Same "unanswered promise"
//! shape as `pending_frames.rs`'s `wl_callback.done` tracking, for
//! `wl_buffer.release` instead.
//!
//! Deliberately narrow: only "is this specific buffer currently
//! attached+committed with no release seen since" is tracked, not general
//! buffer content/damage history.

use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub struct BufferFlowTracker {
    /// The most recently `wl_surface.attach`ed buffer's guest id per
    /// surface (0 = null, i.e. no buffer) -- applied to `in_flight` on the
    /// next `commit`, mirroring Wayland's own double-buffered
    /// attach-then-commit state model (a `commit` with no intervening
    /// `attach` reapplies the same pending buffer, so this is read, not
    /// consumed, by `on_commit`).
    pending_attach: HashMap<u32, u32>,
    /// Buffers currently attached+committed to some surface with no
    /// `wl_buffer.release` seen since.
    in_flight: HashSet<u32>,
}

impl BufferFlowTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// `wl_surface.attach(surface, buffer, x, y)` -- `buffer_guest_id` is
    /// `0` for a null (detaching) attach.
    pub fn on_attach(&mut self, surface_guest_id: u32, buffer_guest_id: u32) {
        self.pending_attach.insert(surface_guest_id, buffer_guest_id);
    }

    /// `wl_surface.commit(surface)` -- applies whatever buffer was last
    /// attached (if any, and if non-null) to `in_flight`.
    pub fn on_commit(&mut self, surface_guest_id: u32) {
        if let Some(&buffer_guest_id) = self.pending_attach.get(&surface_guest_id) {
            if buffer_guest_id != 0 {
                self.in_flight.insert(buffer_guest_id);
            }
        }
    }

    /// `wl_buffer.release(buffer)` -- the compositor is done with this
    /// buffer; the client is free to reuse it, and it's no longer this
    /// tracker's concern.
    pub fn on_release(&mut self, buffer_guest_id: u32) {
        self.in_flight.remove(&buffer_guest_id);
    }

    /// Every buffer still believed in-flight -- a compositor crash between
    /// `commit` and the `release` it would eventually have sent. Drains
    /// rather than just reads: none of these are this tracker's concern
    /// after a reconnect, whether or not the caller manages to synthesize
    /// a release for each one, and leaving stale entries around risks
    /// re-synthesizing against a later, unrelated reconnect.
    pub fn drain_in_flight(&mut self) -> Vec<u32> {
        self.in_flight.drain().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_then_commit_marks_the_buffer_in_flight() {
        let mut t = BufferFlowTracker::new();
        t.on_attach(6, 58);
        t.on_commit(6);
        assert_eq!(t.drain_in_flight(), vec![58]);
    }

    #[test]
    fn attach_without_commit_is_not_in_flight() {
        let mut t = BufferFlowTracker::new();
        t.on_attach(6, 58);
        assert!(t.drain_in_flight().is_empty());
    }

    #[test]
    fn release_clears_in_flight_state() {
        let mut t = BufferFlowTracker::new();
        t.on_attach(6, 58);
        t.on_commit(6);
        t.on_release(58);
        assert!(t.drain_in_flight().is_empty());
    }

    #[test]
    fn null_attach_never_becomes_in_flight() {
        let mut t = BufferFlowTracker::new();
        t.on_attach(6, 0);
        t.on_commit(6);
        assert!(t.drain_in_flight().is_empty());
    }

    #[test]
    fn commit_without_a_new_attach_reapplies_the_previous_buffer() {
        // Wayland's own double-buffered state model: a commit with no
        // intervening attach reuses whatever was last attached -- e.g. a
        // client re-committing the same surface for a damage-only update.
        let mut t = BufferFlowTracker::new();
        t.on_attach(6, 58);
        t.on_commit(6);
        t.on_release(58); // freed once...
        t.on_commit(6); // ...but re-committed without a new attach
        assert_eq!(t.drain_in_flight(), vec![58], "should still reapply buffer 58, not forget it");
    }

    #[test]
    fn drain_clears_state_so_it_isnt_reported_again() {
        let mut t = BufferFlowTracker::new();
        t.on_attach(6, 58);
        t.on_commit(6);
        assert_eq!(t.drain_in_flight(), vec![58]);
        assert!(t.drain_in_flight().is_empty(), "a second drain should be empty");
    }

    #[test]
    fn tracks_multiple_surfaces_independently() {
        let mut t = BufferFlowTracker::new();
        t.on_attach(6, 58);
        t.on_commit(6);
        t.on_attach(7, 99);
        t.on_commit(7);
        let mut in_flight = t.drain_in_flight();
        in_flight.sort();
        assert_eq!(in_flight, vec![58, 99]);
    }
}
