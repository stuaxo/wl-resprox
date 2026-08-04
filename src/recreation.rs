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

use std::os::fd::OwnedFd;

use wayland_backend::protocol::Interface;

/// What's needed to recreate one guest object on a fresh host connection,
/// once its parent (if any) has already been recreated.
pub enum Recreatable {
    /// A `wl_registry.bind` for a specific interface. The roots
    /// (`wl_compositor`, `xdg_wm_base`, `wl_shm`) are recreated this way,
    /// against whatever name the *new* compositor happens to advertise them
    /// as -- never assumed to match the pre-crash name. `version` is the
    /// version the *client itself* originally requested in its own bind
    /// call, not our compiled-in `interface.version` (its static maximum)
    /// or the new compositor's own advertised maximum -- see the
    /// version-mismatch hazard documented where this is replayed, in
    /// `recover_state_after_reconnect`.
    Global { interface: &'static Interface, version: u32 },
    /// `wl_compositor(parent).create_surface(new_id)`.
    Surface { parent_guest_id: u32 },
    /// `xdg_wm_base(parent).get_xdg_surface(new_id, surface)`.
    XdgSurface { parent_guest_id: u32, surface_guest_id: u32 },
    /// `xdg_surface(parent).get_toplevel(new_id)`.
    XdgToplevel { parent_guest_id: u32 },
    /// `wl_shm(wl_shm_guest_id).create_pool(new_id, fd, size)` -- see
    /// ADR-0006. `fd` is the proxy's own retained copy of the client's
    /// backing memfd (SCM_RIGHTS hands every receiving end its own
    /// independent copy, so keeping this one doesn't affect the copy
    /// already sent on to the original host); closed automatically by
    /// `OwnedFd`'s `Drop` impl whenever this recipe is forgotten via
    /// `RecreationGraph::remove` (destroy/delete_id), which needs no
    /// separate cleanup bookkeeping as a result. `size` is updated in place
    /// by `RecreationGraph::update_shm_pool_size` on `wl_shm_pool.resize`,
    /// rather than recorded as a second recipe for the same guest id.
    ShmPool { wl_shm_guest_id: u32, fd: OwnedFd, size: i32 },
    /// `wl_shm_pool(pool_guest_id).create_buffer(new_id, offset, width,
    /// height, stride, format)` -- see ADR-0006. No fd of its own; draws
    /// from the pool's already-retained backing memfd.
    ShmBuffer { pool_guest_id: u32, offset: i32, width: i32, height: i32, stride: i32, format: u32 },
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

    /// `wl_shm_pool.resize(size)` doesn't get its own new_id (it's not a
    /// new object, just a mutation of an existing one), so it can't go
    /// through `record` the way every other recipe does -- update the
    /// pool's already-recorded `size` in place instead. A no-op if
    /// `guest_id` has no `ShmPool` recipe (e.g. `resize` on an id outside
    /// the recreation graph, which `relay_ready_messages` still forwards
    /// normally -- this call is purely about keeping a *tracked* pool's
    /// replay recipe accurate, not a correctness gate on the request
    /// itself).
    pub fn update_shm_pool_size(&mut self, guest_id: u32, new_size: i32) {
        if let Some((_, Recreatable::ShmPool { size, .. })) =
            self.recipes.iter_mut().rev().find(|(id, r)| *id == guest_id && matches!(r, Recreatable::ShmPool { .. }))
        {
            *size = new_size;
        }
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
        graph.record(5, Recreatable::Global { interface: &FAKE_INTERFACE, version: 1 });
        graph.remove(5);
        assert!(graph.recipe_for(5).is_none());
    }

    #[test]
    fn update_shm_pool_size_mutates_the_existing_recipe_in_place() {
        let mut graph = RecreationGraph::new();
        let fd: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        graph.record(5, Recreatable::ShmPool { wl_shm_guest_id: 2, fd, size: 4096 });
        graph.update_shm_pool_size(5, 8192);
        match graph.recipe_for(5) {
            Some(Recreatable::ShmPool { size, wl_shm_guest_id, .. }) => {
                assert_eq!(*size, 8192);
                assert_eq!(*wl_shm_guest_id, 2);
            }
            _ => panic!("expected a ShmPool recipe"),
        }
    }

    #[test]
    fn update_shm_pool_size_is_a_no_op_for_an_untracked_or_wrong_shaped_id() {
        let mut graph = RecreationGraph::new();
        graph.record(5, Recreatable::Surface { parent_guest_id: 3 });
        graph.update_shm_pool_size(5, 8192); // wrong recipe shape -- must not panic or touch it
        graph.update_shm_pool_size(999, 8192); // never recorded at all
        match graph.recipe_for(5) {
            Some(Recreatable::Surface { parent_guest_id }) => assert_eq!(*parent_guest_id, 3),
            _ => panic!("expected the original Surface recipe, untouched"),
        }
    }

    #[test]
    fn untracked_id_has_no_recipe() {
        let graph = RecreationGraph::new();
        assert!(graph.recipe_for(999).is_none());
    }
}
