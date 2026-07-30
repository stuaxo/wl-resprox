//! Tracks enough about a deliberately narrow set of objects to replay
//! their creation against a fresh host connection after a reconnect --
//! implementation-constraints.md's "On Server Reconnect" rule is
//! specifically "Recreate `wl_surface` / `xdg_toplevel`... using the
//! shadow table's tracked state", not "replay every object ever created".
//!
//! `xdg_surface` is tracked too, as the unavoidable link between the two
//! (`xdg_wm_base.get_xdg_surface(surface)` -> `xdg_surface.get_toplevel()`),
//! and `wl_compositor`/`xdg_wm_base` themselves, as the roots every other
//! entry here is ultimately created from. Nothing else -- not `wl_seat`,
//! not `wl_buffer`, not input devices -- gets a recipe: GTK re-obtains
//! those naturally by reacting to the freshly-fetched registry rather than
//! needing the proxy to resurrect them on its behalf.
//!
//! A fully generic "replay any object's creation from its raw wire bytes"
//! engine was considered and rejected here: since the request shapes
//! involved (`wl_registry.bind`, `create_surface`, `get_xdg_surface`,
//! `get_toplevel`) are few and fixed, hand-modeling each as a variant is
//! less code and more obviously correct than a byte-patching replay engine
//! that would need to *also* know which of a replayed message's arguments
//! are stale host ids needing re-translation vs. static data to replay
//! verbatim.

use wayland_backend::protocol::Interface;

/// What's needed to recreate one guest object on a fresh host connection,
/// once its parent (if any) has already been recreated.
pub enum Recreatable {
    /// A `wl_registry.bind` for a specific interface. The two roots
    /// (`wl_compositor`, `xdg_wm_base`) are recreated this way, against
    /// whatever name the *new* compositor happens to advertise them as --
    /// never assumed to match the pre-crash name.
    Global { interface: &'static Interface },
    /// `wl_compositor(parent).create_surface(new_id)`.
    Surface { parent_guest_id: u32 },
    /// `xdg_wm_base(parent).get_xdg_surface(new_id, surface)`.
    XdgSurface { parent_guest_id: u32, surface_guest_id: u32 },
    /// `xdg_surface(parent).get_toplevel(new_id)`.
    XdgToplevel { parent_guest_id: u32 },
}

/// Backed by a `Vec`, not a `HashMap`, specifically to preserve insertion
/// order: callers replaying these against a fresh host need a child's
/// parent already recreated (and re-mapped in the Shadow Table) before the
/// child itself is replayed, and insertion order is always
/// parent-before-child -- a child can only ever be recorded once its
/// parent already exists to be recorded as its `parent_guest_id`. A
/// `HashMap`'s iteration order is unspecified and would silently break
/// that guarantee. Lookups are a linear scan, which is fine at the scale
/// this ever holds (a handful of surfaces/toplevels for one client, not
/// thousands).
#[derive(Default)]
pub struct RecreationGraph {
    recipes: Vec<(u32, Recreatable)>,
}

impl RecreationGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records how to recreate `guest_id`. If `guest_id` already has a
    /// recipe (id reuse after a `delete_id`/re-`record`), the new one is
    /// pushed to the end rather than replacing it in place -- correct as
    /// long as the new recipe's own parent was itself recorded before this
    /// call, which `relay_ready_messages` (the only caller) guarantees by
    /// construction. `recipe_for` returns the most recent one either way.
    pub fn record(&mut self, guest_id: u32, recipe: Recreatable) {
        self.recipes.push((guest_id, recipe));
    }

    pub fn recipe_for(&self, guest_id: u32) -> Option<&Recreatable> {
        self.recipes.iter().rev().find(|(id, _)| *id == guest_id).map(|(_, r)| r)
    }

    /// Forgets a guest id -- called alongside `ShadowTable::remove_guest`
    /// on `wl_display.delete_id`, so a later reconnect doesn't try to
    /// recreate an object the client itself already destroyed.
    pub fn remove(&mut self, guest_id: u32) {
        self.recipes.retain(|(id, _)| *id != guest_id);
    }

    /// Every tracked (guest_id, recipe) pair, parent-before-child (see the
    /// struct doc comment).
    pub fn iter(&self) -> impl Iterator<Item = (u32, &Recreatable)> {
        self.recipes.iter().map(|(id, r)| (*id, r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static FAKE_INTERFACE: Interface =
        Interface { name: "fake", version: 1, requests: &[], events: &[], c_ptr: None };

    #[test]
    fn records_and_retrieves_a_recipe() {
        let mut graph = RecreationGraph::new();
        graph.record(5, Recreatable::Surface { parent_guest_id: 3 });
        match graph.recipe_for(5) {
            Some(Recreatable::Surface { parent_guest_id }) => assert_eq!(*parent_guest_id, 3),
            _ => panic!("expected a Surface recipe"),
        }
    }

    #[test]
    fn remove_forgets_the_recipe() {
        let mut graph = RecreationGraph::new();
        graph.record(5, Recreatable::Global { interface: &FAKE_INTERFACE });
        graph.remove(5);
        assert!(graph.recipe_for(5).is_none());
    }

    #[test]
    fn untracked_id_has_no_recipe() {
        let graph = RecreationGraph::new();
        assert!(graph.recipe_for(999).is_none());
    }
}
