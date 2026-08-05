//! The Shadow Table: bidirectional translation between the GTK client's
//! object IDs ("guest") and the compositor's ("host"), plus per-guest-id
//! interface tracking. See docs/architecture-notes.md for the design
//! discussion -- in short, real Wayland proxies (sommelier-rs's
//! `ShadowTable`, which this is modeled on) can't just reflect IDs 1:1
//! forever: the whole point of surviving a compositor crash is
//! reconnecting to a *new* compositor process, which will not agree to
//! reuse the old one's object numbering.
//!
//! Interfaces are tracked keyed by **guest** id, not host id: the guest id
//! is the stable identity the client always uses (in its own requests'
//! `sender_id`, and in every `Object`-typed argument it sends), so it's
//! the natural key. Looking up an event's sender's interface when the
//! event's `sender_id` is a host id means translating host -> guest first,
//! then looking up by that.

use std::collections::HashMap;

use bimap::BiMap;
use wayland_backend::protocol::Interface;

pub struct ShadowTable {
    guest_to_host: BiMap<u32, u32>,
    interfaces: HashMap<u32, &'static Interface>,
    /// Allocates ids for objects the *client* creates, to present to the
    /// host. Starts at 2 (id 1 is reserved for `wl_display`, matching both
    /// libwayland-client's own convention and sommelier-rs's
    /// `ShadowTable`) -- this is a real independent counter, not a mirror
    /// of whatever id the client itself chose.
    next_host_id: u32,
    /// Allocates ids for objects the *host* creates (server-initiated
    /// `new_id` events, e.g. `wl_data_device.data_offer`), to present to
    /// the client. Starts at `0xff000000`, the wire protocol's own
    /// reserved range for server-allocated ids.
    next_guest_server_id: u32,
    /// Bumped once per successful reconnect (see `bump_generation`).
    /// Paired with `mapped_in_generation`, this is what `host_id`/
    /// `guest_id`/`is_current_generation` use to answer "was this guest id
    /// (re)mapped against the *current* host connection, or is it a
    /// leftover from a previous one that was never refreshed?" Two
    /// distinct hazards this guards against:
    ///
    /// 1. A *fresh* host connection is a brand new `wl_client` from the
    ///    compositor's own perspective -- it has never seen any
    ///    client-allocated id before and expects the first one to be 2,
    ///    gapless from there (the same "new_id gap" rule that applies to
    ///    dropped messages, just now for `next_host_id` continuing to
    ///    count up across a reconnect instead of resetting).
    ///    `bump_generation` resets it.
    /// 2. Once it resets, freshly-allocated *low* host ids can numerically
    ///    coincide with a stale guest_to_host entry from the previous
    ///    generation that nothing ever refreshed (anything outside the
    ///    narrow recreation graph -- see recreation.rs -- e.g. `wl_seat`,
    ///    `wl_buffer`). `host_id`/`guest_id` refuse to translate a
    ///    stale-generation id at all (falling into the same "untracked,
    ///    drop the message" path as a truly-never-seen id), rather than
    ///    silently resolving it to whatever unrelated object now holds
    ///    that number.
    generation: u64,
    mapped_in_generation: HashMap<u32, u64>,
    /// Diagnostic-only: the interface *name* a guest id would have had, for
    /// ids whose `wl_registry.bind` (or other new_id request) was dropped
    /// because we have no compiled-in signature table for that interface
    /// (e.g. `gtk_shell1`) -- see the `None` branch in `lib.rs` where a
    /// `new_id` can't be tracked. Never used for translation/correctness,
    /// only to make a later "unknown object" warning name the interface
    /// instead of a bare id -- so unlike `interfaces`, no generation
    /// tracking: a stale name in a rare diagnostic line is a cosmetic
    /// non-issue, not worth the bookkeeping the real mapping needs.
    unresolvable_interfaces: HashMap<u32, String>,
}

impl ShadowTable {
    pub fn new() -> Self {
        let mut table = Self {
            guest_to_host: BiMap::new(),
            interfaces: HashMap::new(),
            next_host_id: 2,
            next_guest_server_id: 0xff000000,
            generation: 0,
            mapped_in_generation: HashMap::new(),
            unresolvable_interfaces: HashMap::new(),
        };
        // wl_display is id 1 on both sides unconditionally, by protocol
        // convention -- no bind/create_registry dance produces it, so
        // nothing would ever populate this mapping otherwise.
        table.map(1, 1, &wayland_client::protocol::__interfaces::WL_DISPLAY_INTERFACE);
        table
    }

    /// Call once per successful reconnect, before replaying any recreation
    /// recipes -- everything (re)mapped from this point on belongs to the
    /// new generation; anything not yet touched stays behind in an older
    /// one (see the `generation` field's doc comment for why that
    /// matters). Also resets `next_host_id` back to 2 (a fresh host
    /// connection is a brand new `wl_client` with its own id-allocation
    /// expectations, unrelated to how many ids we'd handed out over the
    /// previous connection's lifetime) and immediately re-seeds
    /// `wl_display`'s 1<->1 mapping, which otherwise would itself count as
    /// stale from here on -- unlike everything else, it's never
    /// "recreated" by `recover_state_after_reconnect` (nothing needs to
    /// replay a request for it, id 1 is always both sides' `wl_display` by
    /// protocol convention), so nothing else would ever refresh it, and
    /// every single message of any kind references it (`sync`,
    /// `get_registry`, `error`, `delete_id`, ...).
    pub fn bump_generation(&mut self) {
        self.generation += 1;
        self.next_host_id = 2;
        self.map(1, 1, &wayland_client::protocol::__interfaces::WL_DISPLAY_INTERFACE);
        // Ids from a previous generation may be reallocated to unrelated
        // objects by the new connection -- an old, unrelated name lingering
        // here would be actively misleading rather than just stale.
        self.unresolvable_interfaces.clear();
    }

    /// Whether `guest_id` was mapped (or re-mapped) against the *current*
    /// generation -- false for an id that predates the last reconnect and
    /// was never refreshed (not part of the recreation graph). See the
    /// `generation` field's doc comment for why this matters.
    pub fn is_current_generation(&self, guest_id: u32) -> bool {
        self.mapped_in_generation.get(&guest_id) == Some(&self.generation)
    }

    pub fn allocate_host_id(&mut self) -> u32 {
        let id = self.next_host_id;
        self.next_host_id += 1;
        id
    }

    /// Gives back a host id that was allocated via `allocate_host_id` but
    /// never actually sent to the host -- e.g. the message carrying it as
    /// a `new_id` argument got dropped (its own sender turned out
    /// untranslatable) before ever being forwarded. Without this,
    /// `next_host_id` permanently drifts ahead of what the host has
    /// actually seen, and libwayland-server's own new_id validation
    /// (`wl_map_reserve_new`) rejects the next legitimate `new_id`
    /// outright once the gap is reached, since a real Wayland server
    /// requires a client's object ids to stay gapless (see ADR-0006's
    /// "Open issue" section). Silently a no-op unless `id` is exactly the
    /// most recently allocated one (`next_host_id - 1`) -- true by
    /// construction at every call site today (`relay_ready_messages`
    /// always allocates, then immediately either forwards or rolls back,
    /// within the same message's processing), but a guard against
    /// silently corrupting the counter if that invariant stops holding
    /// somewhere new.
    pub fn unallocate_host_id(&mut self, id: u32) {
        if id == self.next_host_id - 1 {
            self.next_host_id -= 1;
        }
    }

    pub fn allocate_guest_server_id(&mut self) -> u32 {
        let id = self.next_guest_server_id;
        self.next_guest_server_id += 1;
        id
    }

    /// Records that `guest_id` (on the client's side) and `host_id` (on
    /// the compositor's side) refer to the same logical object, of the
    /// given interface.
    pub fn map(&mut self, guest_id: u32, host_id: u32, interface: &'static Interface) {
        self.guest_to_host.insert(guest_id, host_id);
        self.interfaces.insert(guest_id, interface);
        self.mapped_in_generation.insert(guest_id, self.generation);
    }

    /// Translates a guest-side id to its host-side counterpart. `0` (the
    /// wire protocol's "null object") always maps to itself -- it's never
    /// an allocated id in either space, just "no object". Refuses to
    /// translate (returns `None`, same as an id that was never mapped at
    /// all) a mapping that predates the current generation -- see the
    /// `generation` field's doc comment for why a stale-but-still-present
    /// mapping is actively dangerous to trust, not just outdated.
    pub fn host_id(&self, guest_id: u32) -> Option<u32> {
        if guest_id == 0 {
            return Some(0);
        }
        if !self.is_current_generation(guest_id) {
            return None;
        }
        self.guest_to_host.get_by_left(&guest_id).copied()
    }

    /// Translates a host-side id to its guest-side counterpart. See
    /// `host_id`'s doc comment for the `0` special case and the
    /// generation check.
    pub fn guest_id(&self, host_id: u32) -> Option<u32> {
        if host_id == 0 {
            return Some(0);
        }
        let guest_id = self.guest_to_host.get_by_right(&host_id).copied()?;
        if !self.is_current_generation(guest_id) {
            return None;
        }
        Some(guest_id)
    }

    pub fn interface(&self, guest_id: u32) -> Option<&'static Interface> {
        self.interfaces.get(&guest_id).copied()
    }

    /// Records the interface name a guest id *would* have had, had its
    /// `new_id` request not been dropped for referencing an interface we
    /// have no compiled-in signature table for. Diagnostic-only -- see the
    /// `unresolvable_interfaces` field doc comment.
    pub fn remember_unresolvable_interface(&mut self, guest_id: u32, interface_name: String) {
        self.unresolvable_interfaces.insert(guest_id, interface_name);
    }

    /// Looks up a name recorded by `remember_unresolvable_interface`, for
    /// enriching an "unknown object" warning when the id matches an
    /// interface bind we watched get dropped rather than a truly
    /// never-seen id.
    pub fn unresolvable_interface_name(&self, guest_id: u32) -> Option<&str> {
        self.unresolvable_interfaces.get(&guest_id).map(String::as_str)
    }

    /// Finds the guest id of the (normally singular) object of a given
    /// interface -- used on reconnect to find the client's own original
    /// `wl_registry` guest id, so a freshly-fetched registry on the new
    /// host can be mapped onto it rather than needing a proxy-private id.
    /// A linear scan, not indexed: only ever called at reconnect time, not
    /// on the hot relay path.
    pub fn find_guest_id_by_interface_name(&self, name: &str) -> Option<u32> {
        self.interfaces.iter().find(|(_, iface)| iface.name == name).map(|(&id, _)| id)
    }

    /// Forgets a guest id entirely (both directions of the mapping, plus
    /// its interface) -- called on `wl_display.delete_id`, once the
    /// compositor has confirmed the object is gone.
    pub fn remove_guest(&mut self, guest_id: u32) {
        self.guest_to_host.remove_by_left(&guest_id);
        self.interfaces.remove(&guest_id);
        self.mapped_in_generation.remove(&guest_id);
        self.unresolvable_interfaces.remove(&guest_id);
    }
}

impl Default for ShadowTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static FAKE_INTERFACE: Interface =
        Interface { name: "fake", version: 1, requests: &[], events: &[], c_ptr: None };

    #[test]
    fn wl_display_is_preseeded_as_1_to_1() {
        let table = ShadowTable::new();
        assert_eq!(table.host_id(1), Some(1));
        assert_eq!(table.guest_id(1), Some(1));
        assert_eq!(table.interface(1).unwrap().name, "wl_display");
    }

    #[test]
    fn translates_in_both_directions_even_when_ids_differ() {
        // Deliberately mismatched ids -- the whole point is proving the
        // lookup logic itself, independent of whether a live run's
        // allocators happen to produce matching numbers (see
        // docs/architecture-notes.md for why they currently often will).
        let mut table = ShadowTable::new();
        table.map(5, 12345, &FAKE_INTERFACE);
        assert_eq!(table.host_id(5), Some(12345));
        assert_eq!(table.guest_id(12345), Some(5));
        assert_eq!(table.interface(5).unwrap().name, "fake");
    }

    #[test]
    fn null_object_id_translates_to_itself_both_ways() {
        let table = ShadowTable::new();
        assert_eq!(table.host_id(0), Some(0));
        assert_eq!(table.guest_id(0), Some(0));
    }

    #[test]
    fn untracked_id_fails_to_translate() {
        let table = ShadowTable::new();
        assert_eq!(table.host_id(999), None);
        assert_eq!(table.guest_id(999), None);
    }

    #[test]
    fn remove_guest_forgets_both_directions_and_interface() {
        let mut table = ShadowTable::new();
        table.map(5, 12345, &FAKE_INTERFACE);
        table.remove_guest(5);
        assert_eq!(table.host_id(5), None);
        assert_eq!(table.guest_id(12345), None);
        assert!(table.interface(5).is_none());
    }

    #[test]
    fn mapping_belongs_to_the_generation_it_was_mapped_in() {
        let mut table = ShadowTable::new();
        table.map(5, 12345, &FAKE_INTERFACE);
        assert!(table.is_current_generation(5), "just mapped, should be current");

        table.bump_generation();
        assert!(!table.is_current_generation(5), "predates the reconnect, now stale");

        // Re-mapping (e.g. recover_state_after_reconnect recreating it)
        // brings it current again.
        table.map(5, 99999, &FAKE_INTERFACE);
        assert!(table.is_current_generation(5));
    }

    #[test]
    fn stale_generation_mapping_refuses_to_translate() {
        // Not just is_current_generation -- host_id/guest_id themselves
        // must refuse, the same as an id that was never mapped at all,
        // since a stale-but-still-present entry can numerically coincide
        // with an unrelated fresh object on the new connection (a real,
        // confirmed hazard -- see the `generation` field's doc comment).
        let mut table = ShadowTable::new();
        table.map(5, 12345, &FAKE_INTERFACE);
        table.bump_generation();
        assert_eq!(table.host_id(5), None, "stale guest->host translation must fail");
        assert_eq!(table.guest_id(12345), None, "stale host->guest translation must fail");
    }

    #[test]
    fn wl_display_survives_a_generation_bump_without_being_remapped() {
        // wl_display is never touched by recover_state_after_reconnect
        // (nothing "recreates" it -- id 1 is always both sides' wl_display
        // by protocol convention) -- unlike everything else, bump_generation
        // itself must keep it current, or every message of any kind
        // (sync, get_registry, error, delete_id, ...) breaks after a
        // reconnect. Regression test for exactly that bug.
        let mut table = ShadowTable::new();
        table.bump_generation();
        assert_eq!(table.host_id(1), Some(1), "wl_display must stay translatable after a reconnect");
        assert_eq!(table.guest_id(1), Some(1));
        assert!(table.is_current_generation(1));
    }

    #[test]
    fn bump_generation_resets_the_host_id_allocator() {
        // A fresh host connection is a brand new wl_client from the
        // compositor's own perspective, requiring gapless allocation
        // starting at 2 -- continuing to count up from a previous
        // connection's lifetime would reproduce the same "new_id gap"
        // rejection as a dropped message, just against a fresh compositor
        // connection instead.
        let mut table = ShadowTable::new();
        table.allocate_host_id();
        table.allocate_host_id();
        table.allocate_host_id(); // next_host_id is now 5
        table.bump_generation();
        assert_eq!(table.allocate_host_id(), 2, "must restart from 2 after a reconnect");
    }

    #[test]
    fn untracked_id_is_never_current_generation() {
        let table = ShadowTable::new();
        assert!(!table.is_current_generation(999));
    }

    #[test]
    fn remove_guest_forgets_generation_too() {
        let mut table = ShadowTable::new();
        table.map(5, 12345, &FAKE_INTERFACE);
        table.remove_guest(5);
        assert!(!table.is_current_generation(5));
    }

    #[test]
    fn allocators_are_independent_and_start_at_protocol_conventional_values() {
        let mut table = ShadowTable::new();
        assert_eq!(table.allocate_host_id(), 2);
        assert_eq!(table.allocate_host_id(), 3);
        assert_eq!(table.allocate_guest_server_id(), 0xff000000);
        assert_eq!(table.allocate_guest_server_id(), 0xff000001);
    }

    #[test]
    fn unallocate_host_id_gives_back_the_most_recent_allocation() {
        let mut table = ShadowTable::new();
        assert_eq!(table.allocate_host_id(), 2);
        assert_eq!(table.allocate_host_id(), 3);
        table.unallocate_host_id(3);
        // 3 was given back -- the next allocation should reuse it, not
        // leave a gap at 3 by jumping straight to 4.
        assert_eq!(table.allocate_host_id(), 3);
    }

    #[test]
    fn unallocate_host_id_is_a_no_op_for_anything_but_the_most_recent_allocation() {
        let mut table = ShadowTable::new();
        table.allocate_host_id(); // 2
        table.allocate_host_id(); // 3
        table.allocate_host_id(); // 4 -- next_host_id is now 5
        // Not the most recent (4) -- must not silently rewind the
        // counter past ids that may already be legitimately in use.
        table.unallocate_host_id(2);
        table.unallocate_host_id(3);
        assert_eq!(table.allocate_host_id(), 5);
    }
}
