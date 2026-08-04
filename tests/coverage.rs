//! Scoped signature-drift test for ADR-0007's state-replay encoding.
//!
//! Not a full-protocol-coverage test (that was considered and rejected in
//! the ADR itself -- this proxy tracks a deliberately narrow set of
//! recreatable objects, not every message in every imported protocol, see
//! `recreation.rs`'s own top-of-file comment). Instead, for every request
//! or event `recover_state_after_reconnect` (in `src/lib.rs`) builds a
//! `WaylandValue` list for, this asserts that list's shape -- argument
//! count and type, in order -- still matches the *real* signature read
//! from the actual generated `Interface` tables (`wayland-client`/
//! `wayland-protocols`/etc.), not a hand-rolled stand-in.
//!
//! This exists because `wire::encode_arguments` only validates a value
//! list against whatever `&[ArgumentType]` slice it's handed at runtime --
//! it can't tell a stale assumption baked into a call site (e.g. "this
//! request takes 5 arguments") from a protocol update that changed the
//! real signature out from under it. This test is the check that would
//! catch exactly that: if a future `wayland-protocols` bump changes one of
//! these signatures, this fails here instead of only surfacing as a live,
//! silently malformed message.
//!
//! Each case's expected `ArgumentType` list is written out by hand,
//! deliberately mirroring the `WaylandValue` list built at the
//! corresponding call site in `src/lib.rs`'s `recover_state_after_reconnect`
//! -- keep the two in sync if either changes.

use wayland_backend::protocol::{AllowNull, ArgumentType, Interface};
use wayland_proxy::interfaces::lookup_interface;

fn interface(name: &str) -> &'static Interface {
    lookup_interface(name).unwrap_or_else(|| panic!("no compiled-in interface table for {name}"))
}

fn request_signature(interface_name: &str, request_name: &str) -> &'static [ArgumentType] {
    interface(interface_name)
        .requests
        .iter()
        .find(|m| m.name == request_name)
        .unwrap_or_else(|| panic!("{interface_name} has no {request_name} request in the compiled-in table"))
        .signature
}

fn event_signature(interface_name: &str, event_name: &str) -> &'static [ArgumentType] {
    interface(interface_name)
        .events
        .iter()
        .find(|m| m.name == event_name)
        .unwrap_or_else(|| panic!("{interface_name} has no {event_name} event in the compiled-in table"))
        .signature
}

/// `wp_viewport` and `zwp_linux_buffer_params_v1` are child objects, never
/// the target of a `wl_registry.bind` -- `lookup_interface` only tabulates
/// bindable globals (plus a handful of other directly-named core
/// interfaces), so these resolve through their creating request's
/// `child_interface` instead, the same way `resolve_child_interface` in
/// `src/lib.rs` does at runtime.
fn child_interface_of(parent_interface_name: &str, parent_request_name: &str) -> &'static Interface {
    interface(parent_interface_name)
        .requests
        .iter()
        .find(|m| m.name == parent_request_name)
        .unwrap_or_else(|| panic!("{parent_interface_name} has no {parent_request_name} request"))
        .child_interface
        .unwrap_or_else(|| panic!("{parent_interface_name}.{parent_request_name} has no static child_interface"))
}

fn request_signature_of(interface: &'static Interface, request_name: &str) -> &'static [ArgumentType] {
    interface
        .requests
        .iter()
        .find(|m| m.name == request_name)
        .unwrap_or_else(|| panic!("{} has no {request_name} request in the compiled-in table", interface.name))
        .signature
}

/// `wl_registry.bind` needs no special-casing in `encode_arguments`:
/// wayland-scanner's codegen already expands an interface-less `new_id`
/// argument into `[Str(No), Uint, NewId]` directly in the static
/// signature (see `encode_arguments`'s doc comment), so `bind`'s real
/// generated signature is the full 4-entry shape actually on the wire --
/// not the 2-entry `[Uint, NewId]` its bare XML declaration (`name: uint`,
/// `id: new_id`) alone would suggest. This is the regression test for
/// that: if a future wayland-scanner/protocol change ever shrank it back
/// to 2 entries, `encode_arguments`'s strict length check would need a
/// bypass again, same as this project originally (wrongly) assumed it
/// already did.
#[test]
fn bind_signature_is_the_real_four_entry_expanded_shape() {
    assert_eq!(
        request_signature("wl_registry", "bind"),
        &[ArgumentType::Uint, ArgumentType::Str(AllowNull::No), ArgumentType::Uint, ArgumentType::NewId]
    );
}

/// End-to-end proof that `encode_arguments` handles `bind` with no
/// wrapper: the exact value list `recover_state_after_reconnect` builds
/// for a `Recreatable::Global` replay, round-tripped through the real
/// signature and compared byte-for-byte against `wire::build_message`'s
/// own hand-assembled equivalent (the same shape the pre-ADR-0007 code
/// built by hand).
#[test]
fn encode_arguments_reproduces_a_hand_built_bind_message() {
    let signature = request_signature("wl_registry", "bind");
    let values = vec![
        wayland_proxy::wire::WaylandValue::Uint(42),
        wayland_proxy::wire::WaylandValue::String("wl_compositor".to_string()),
        wayland_proxy::wire::WaylandValue::Uint(4),
        wayland_proxy::wire::WaylandValue::NewId(6),
    ];
    let (payload, fds) = wayland_proxy::wire::encode_arguments(signature, values).expect("bind's real signature should validate");
    assert!(fds.is_empty());

    let mut expected = Vec::new();
    wayland_proxy::wire::put_u32(&mut expected, 42);
    wayland_proxy::wire::put_str(&mut expected, "wl_compositor");
    wayland_proxy::wire::put_u32(&mut expected, 4);
    wayland_proxy::wire::put_u32(&mut expected, 6);
    assert_eq!(payload, expected);
}

#[test]
fn create_surface_signature_matches_the_surface_recipe() {
    assert_eq!(request_signature("wl_compositor", "create_surface"), &[ArgumentType::NewId]);
}

#[test]
fn get_xdg_surface_signature_matches_the_xdg_surface_recipe() {
    assert_eq!(
        request_signature("xdg_wm_base", "get_xdg_surface"),
        &[ArgumentType::NewId, ArgumentType::Object(AllowNull::No)]
    );
}

#[test]
fn get_toplevel_signature_matches_the_xdg_toplevel_recipe() {
    assert_eq!(request_signature("xdg_surface", "get_toplevel"), &[ArgumentType::NewId]);
}

#[test]
fn set_title_signature_matches_the_xdg_toplevel_title_replay() {
    assert_eq!(request_signature("xdg_toplevel", "set_title"), &[ArgumentType::Str(AllowNull::No)]);
}

#[test]
fn set_app_id_signature_matches_the_xdg_toplevel_app_id_replay() {
    assert_eq!(request_signature("xdg_toplevel", "set_app_id"), &[ArgumentType::Str(AllowNull::No)]);
}

#[test]
fn xdg_toplevel_configure_event_signature_matches_the_synthesized_repaint_event() {
    assert_eq!(
        event_signature("xdg_toplevel", "configure"),
        &[ArgumentType::Int, ArgumentType::Int, ArgumentType::Array]
    );
}

#[test]
fn xdg_surface_configure_event_signature_matches_the_synthesized_repaint_event() {
    assert_eq!(event_signature("xdg_surface", "configure"), &[ArgumentType::Uint]);
}

#[test]
fn create_pool_signature_matches_the_shm_pool_recipe() {
    assert_eq!(
        request_signature("wl_shm", "create_pool"),
        &[ArgumentType::NewId, ArgumentType::Fd, ArgumentType::Int]
    );
}

#[test]
fn create_buffer_signature_matches_the_shm_buffer_recipe() {
    assert_eq!(
        request_signature("wl_shm_pool", "create_buffer"),
        &[ArgumentType::NewId, ArgumentType::Int, ArgumentType::Int, ArgumentType::Int, ArgumentType::Int, ArgumentType::Uint]
    );
}

#[test]
fn create_params_signature_matches_the_dmabuf_buffer_recipe() {
    assert_eq!(request_signature("zwp_linux_dmabuf_v1", "create_params"), &[ArgumentType::NewId]);
}

#[test]
fn add_signature_matches_the_dmabuf_plane_replay() {
    let params = child_interface_of("zwp_linux_dmabuf_v1", "create_params");
    assert_eq!(
        request_signature_of(params, "add"),
        &[
            ArgumentType::Fd,
            ArgumentType::Uint,
            ArgumentType::Uint,
            ArgumentType::Uint,
            ArgumentType::Uint,
            ArgumentType::Uint
        ]
    );
}

#[test]
fn create_immed_signature_matches_the_dmabuf_buffer_recipe() {
    let params = child_interface_of("zwp_linux_dmabuf_v1", "create_params");
    assert_eq!(
        request_signature_of(params, "create_immed"),
        &[ArgumentType::NewId, ArgumentType::Int, ArgumentType::Int, ArgumentType::Uint, ArgumentType::Uint]
    );
}

#[test]
fn seat_device_signatures_match_the_seat_device_recipe() {
    for request in ["get_pointer", "get_keyboard", "get_touch"] {
        assert_eq!(request_signature("wl_seat", request), &[ArgumentType::NewId], "wl_seat.{request}");
    }
}

#[test]
fn get_viewport_signature_matches_the_viewport_recipe() {
    assert_eq!(
        request_signature("wp_viewporter", "get_viewport"),
        &[ArgumentType::NewId, ArgumentType::Object(AllowNull::No)]
    );
}

#[test]
fn set_destination_signature_matches_the_viewport_destination_replay() {
    let viewport = child_interface_of("wp_viewporter", "get_viewport");
    assert_eq!(request_signature_of(viewport, "set_destination"), &[ArgumentType::Int, ArgumentType::Int]);
}

#[test]
fn get_fractional_scale_signature_matches_the_fractional_scale_recipe() {
    assert_eq!(
        request_signature("wp_fractional_scale_manager_v1", "get_fractional_scale"),
        &[ArgumentType::NewId, ArgumentType::Object(AllowNull::No)]
    );
}
