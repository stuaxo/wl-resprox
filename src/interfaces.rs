//! Maps a Wayland interface name (as it appears on the wire, e.g. in
//! `wl_registry.bind`) to its static protocol description.
//!
//! wayland-backend itself only ships `wl_display`, `wl_registry` and
//! `wl_callback` (the three interfaces frozen into the protocol bootstrap).
//! Everything else has to be supplied by us -- we get the rest of core
//! wayland.xml for free from the `wayland-client` dependency's generated
//! code, and `xdg-shell` (needed for any real GTK toplevel window) from
//! `wayland-protocols`. Anything not listed here (further xdg-shell
//! extensions, wl_shell, etc.) simply isn't relayable yet; see the
//! 2026-07-30 entry in docs/debugging-notes.md for what that means in
//! practice and why it's an acceptable gap for this boilerplate step.

use wayland_backend::protocol::Interface;

pub fn lookup_interface(name: &str) -> Option<&'static Interface> {
    use wayland_client::protocol::__interfaces as core;
    use wayland_protocols::xdg::shell::client::__interfaces as xdg_shell;

    Some(match name {
        "wl_display" => &core::WL_DISPLAY_INTERFACE,
        "wl_registry" => &core::WL_REGISTRY_INTERFACE,
        "wl_callback" => &core::WL_CALLBACK_INTERFACE,
        "wl_compositor" => &core::WL_COMPOSITOR_INTERFACE,
        "wl_shm_pool" => &core::WL_SHM_POOL_INTERFACE,
        "wl_shm" => &core::WL_SHM_INTERFACE,
        "wl_buffer" => &core::WL_BUFFER_INTERFACE,
        "wl_data_offer" => &core::WL_DATA_OFFER_INTERFACE,
        "wl_data_source" => &core::WL_DATA_SOURCE_INTERFACE,
        "wl_data_device" => &core::WL_DATA_DEVICE_INTERFACE,
        "wl_data_device_manager" => &core::WL_DATA_DEVICE_MANAGER_INTERFACE,
        "wl_shell" => &core::WL_SHELL_INTERFACE,
        "wl_shell_surface" => &core::WL_SHELL_SURFACE_INTERFACE,
        "wl_surface" => &core::WL_SURFACE_INTERFACE,
        "wl_seat" => &core::WL_SEAT_INTERFACE,
        "wl_pointer" => &core::WL_POINTER_INTERFACE,
        "wl_keyboard" => &core::WL_KEYBOARD_INTERFACE,
        "wl_touch" => &core::WL_TOUCH_INTERFACE,
        "wl_output" => &core::WL_OUTPUT_INTERFACE,
        "wl_region" => &core::WL_REGION_INTERFACE,
        "wl_subcompositor" => &core::WL_SUBCOMPOSITOR_INTERFACE,
        "wl_subsurface" => &core::WL_SUBSURFACE_INTERFACE,
        "wl_fixes" => &core::WL_FIXES_INTERFACE,

        "xdg_wm_base" => &xdg_shell::XDG_WM_BASE_INTERFACE,
        "xdg_positioner" => &xdg_shell::XDG_POSITIONER_INTERFACE,
        "xdg_surface" => &xdg_shell::XDG_SURFACE_INTERFACE,
        "xdg_toplevel" => &xdg_shell::XDG_TOPLEVEL_INTERFACE,
        "xdg_popup" => &xdg_shell::XDG_POPUP_INTERFACE,

        _ => return None,
    })
}
