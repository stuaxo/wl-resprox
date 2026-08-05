//! Maps a Wayland interface name (as it appears on the wire, e.g. in
//! `wl_registry.bind`) to its static protocol description.
//!
//! wayland-backend itself only ships `wl_display`, `wl_registry` and
//! `wl_callback` (the three interfaces frozen into the protocol bootstrap).
//! Everything else has to be supplied by us. Four crates cover it between
//! them, each generated (via wayland-scanner) from a different upstream XML
//! source:
//!   - `wayland-client`: the rest of core wayland.xml.
//!   - `wayland-protocols`: freedesktop's official extensions (xdg-shell,
//!     linux-dmabuf, presentation-time, ...).
//!   - `wayland-protocols-wlr`: wlroots' own extensions (layer-shell,
//!     screencopy, ...) -- every wlroots-based compositor (labwc, sway, ...)
//!     advertises these, but they never went through freedesktop's official
//!     process, hence the separate crate.
//!   - `wayland-protocols-plasma` / `wayland-protocols-misc`: KDE's
//!     extensions, and a handful of orphaned/unofficial-but-widely-used ones
//!     (input-method-v2, virtual-keyboard-v1) that don't fit either bucket.
//!
//! Only interfaces that can be the *target* of a `wl_registry.bind` call
//! (i.e. ones a compositor might actually advertise as a global) need an
//! entry here. Child objects created by a *typed* request (e.g.
//! `zwp_linux_dmabuf_v1.get_default_feedback` -> `zwp_linux_dmabuf_feedback_v1`)
//! resolve automatically through that request's `MessageDesc::child_interface`
//! -- a pointer baked in at codegen time -- without ever consulting this
//! table; see `resolve_child_interface` in lib.rs.
//!
//! This list is necessarily a snapshot of "every global we've seen a real
//! compositor advertise plus everything else in these four crates' covered
//! protocol sets" -- not "every Wayland extension that exists". A global
//! whose interface isn't listed here gets silently dropped by the relay,
//! which isn't just a coverage gap but a correctness bug: it desyncs the
//! client's new_id sequence from the compositor's. The
//! `resolves_every_global_observed_from_real_*` tests below are the
//! regression check for "did we forget to wire up something these crates
//! already generated for us."

use wayland_backend::protocol::Interface;

pub fn lookup_interface(name: &str) -> Option<&'static Interface> {
    use wayland_client::protocol::__interfaces as core;
    use wayland_protocols::xdg::shell::client::__interfaces as xdg_shell;

    // freedesktop's official extensions (wayland-protocols).
    use wayland_protocols::wp::linux_dmabuf::zv1::client::__interfaces as linux_dmabuf;
    use wayland_protocols::wp::presentation_time::client::__interfaces as presentation_time;
    use wayland_protocols::wp::viewporter::client::__interfaces as viewporter;
    use wayland_protocols::wp::tearing_control::v1::client::__interfaces as tearing_control;
    use wayland_protocols::wp::fractional_scale::v1::client::__interfaces as fractional_scale;
    use wayland_protocols::wp::pointer_constraints::zv1::client::__interfaces as pointer_constraints;
    use wayland_protocols::wp::pointer_gestures::zv1::client::__interfaces as pointer_gestures;
    use wayland_protocols::wp::primary_selection::zv1::client::__interfaces as primary_selection;
    use wayland_protocols::wp::relative_pointer::zv1::client::__interfaces as relative_pointer;
    use wayland_protocols::wp::idle_inhibit::zv1::client::__interfaces as idle_inhibit;
    use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::__interfaces as keyboard_shortcuts_inhibit;
    use wayland_protocols::wp::single_pixel_buffer::v1::client::__interfaces as single_pixel_buffer;
    use wayland_protocols::wp::cursor_shape::v1::client::__interfaces as cursor_shape;
    use wayland_protocols::wp::tablet::zv1::client::__interfaces as tablet_v1;
    use wayland_protocols::wp::tablet::zv2::client::__interfaces as tablet_v2;
    use wayland_protocols::wp::text_input::zv1::client::__interfaces as text_input_v1;
    use wayland_protocols::wp::text_input::zv3::client::__interfaces as text_input_v3;
    use wayland_protocols::wp::security_context::v1::client::__interfaces as security_context;
    use wayland_protocols::wp::alpha_modifier::v1::client::__interfaces as alpha_modifier;
    use wayland_protocols::wp::linux_drm_syncobj::v1::client::__interfaces as linux_drm_syncobj;
    use wayland_protocols::xdg::xdg_output::zv1::client::__interfaces as xdg_output;
    use wayland_protocols::xdg::decoration::zv1::client::__interfaces as xdg_decoration;
    use wayland_protocols::xdg::activation::v1::client::__interfaces as xdg_activation;
    use wayland_protocols::xdg::toplevel_icon::v1::client::__interfaces as toplevel_icon;
    use wayland_protocols::xdg::foreign::zv1::client::__interfaces as xdg_foreign_v1;
    use wayland_protocols::xdg::foreign::zv2::client::__interfaces as xdg_foreign_v2;
    use wayland_protocols::xdg::dialog::v1::client::__interfaces as xdg_dialog;
    use wayland_protocols::xdg::system_bell::v1::client::__interfaces as xdg_system_bell;
    use wayland_protocols::wp::color_management::v1::client::__interfaces as color_management;
    use wayland_protocols::wp::color_representation::v1::client::__interfaces as color_representation;
    use wayland_protocols::wp::fifo::v1::client::__interfaces as fifo;
    use wayland_protocols::wp::commit_timing::v1::client::__interfaces as commit_timing;
    use wayland_protocols::ext::idle_notify::v1::client::__interfaces as idle_notify;
    use wayland_protocols::ext::session_lock::v1::client::__interfaces as session_lock;
    use wayland_protocols::ext::foreign_toplevel_list::v1::client::__interfaces as foreign_toplevel_list;
    use wayland_protocols::ext::image_capture_source::v1::client::__interfaces as image_capture_source;
    use wayland_protocols::ext::image_copy_capture::v1::client::__interfaces as image_copy_capture;
    use wayland_protocols::ext::data_control::v1::client::__interfaces as ext_data_control;
    use wayland_protocols::ext::workspace::v1::client::__interfaces as ext_workspace;

    // wlroots' own extensions (wayland-protocols-wlr) -- see module doc.
    use wayland_protocols_wlr::data_control::v1::client::__interfaces as wlr_data_control;
    use wayland_protocols_wlr::export_dmabuf::v1::client::__interfaces as export_dmabuf;
    use wayland_protocols_wlr::foreign_toplevel::v1::client::__interfaces as wlr_foreign_toplevel;
    use wayland_protocols_wlr::gamma_control::v1::client::__interfaces as gamma_control;
    use wayland_protocols_wlr::layer_shell::v1::client::__interfaces as layer_shell;
    use wayland_protocols_wlr::output_management::v1::client::__interfaces as wlr_output_management;
    use wayland_protocols_wlr::output_power_management::v1::client::__interfaces as output_power_management;
    use wayland_protocols_wlr::screencopy::v1::client::__interfaces as screencopy;
    use wayland_protocols_wlr::virtual_pointer::v1::client::__interfaces as virtual_pointer;

    // KDE's extensions, plus the handful of orphaned-but-widely-used ones
    // that don't fit either official bucket -- see module doc.
    use wayland_protocols_plasma::server_decoration::client::__interfaces as kde_server_decoration;
    use wayland_protocols_misc::zwp_input_method_v2::client::__interfaces as input_method_v2;
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::__interfaces as virtual_keyboard;

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

        // freedesktop's official extensions (wayland-protocols).
        "zwp_linux_dmabuf_v1" => &linux_dmabuf::ZWP_LINUX_DMABUF_V1_INTERFACE,
        "wp_presentation" => &presentation_time::WP_PRESENTATION_INTERFACE,
        "wp_viewporter" => &viewporter::WP_VIEWPORTER_INTERFACE,
        "wp_tearing_control_manager_v1" => &tearing_control::WP_TEARING_CONTROL_MANAGER_V1_INTERFACE,
        "wp_fractional_scale_manager_v1" => &fractional_scale::WP_FRACTIONAL_SCALE_MANAGER_V1_INTERFACE,
        "zwp_pointer_constraints_v1" => &pointer_constraints::ZWP_POINTER_CONSTRAINTS_V1_INTERFACE,
        "zwp_pointer_gestures_v1" => &pointer_gestures::ZWP_POINTER_GESTURES_V1_INTERFACE,
        "zwp_primary_selection_device_manager_v1" => &primary_selection::ZWP_PRIMARY_SELECTION_DEVICE_MANAGER_V1_INTERFACE,
        "zwp_relative_pointer_manager_v1" => &relative_pointer::ZWP_RELATIVE_POINTER_MANAGER_V1_INTERFACE,
        "zwp_idle_inhibit_manager_v1" => &idle_inhibit::ZWP_IDLE_INHIBIT_MANAGER_V1_INTERFACE,
        "zwp_keyboard_shortcuts_inhibit_manager_v1" => &keyboard_shortcuts_inhibit::ZWP_KEYBOARD_SHORTCUTS_INHIBIT_MANAGER_V1_INTERFACE,
        "wp_single_pixel_buffer_manager_v1" => &single_pixel_buffer::WP_SINGLE_PIXEL_BUFFER_MANAGER_V1_INTERFACE,
        "wp_cursor_shape_manager_v1" => &cursor_shape::WP_CURSOR_SHAPE_MANAGER_V1_INTERFACE,
        "zwp_tablet_manager_v1" => &tablet_v1::ZWP_TABLET_MANAGER_V1_INTERFACE,
        "zwp_tablet_manager_v2" => &tablet_v2::ZWP_TABLET_MANAGER_V2_INTERFACE,
        "zwp_text_input_manager_v1" => &text_input_v1::ZWP_TEXT_INPUT_MANAGER_V1_INTERFACE,
        "zwp_text_input_manager_v3" => &text_input_v3::ZWP_TEXT_INPUT_MANAGER_V3_INTERFACE,
        "wp_security_context_manager_v1" => &security_context::WP_SECURITY_CONTEXT_MANAGER_V1_INTERFACE,
        "wp_alpha_modifier_v1" => &alpha_modifier::WP_ALPHA_MODIFIER_V1_INTERFACE,
        "wp_linux_drm_syncobj_manager_v1" => &linux_drm_syncobj::WP_LINUX_DRM_SYNCOBJ_MANAGER_V1_INTERFACE,
        "zxdg_output_manager_v1" => &xdg_output::ZXDG_OUTPUT_MANAGER_V1_INTERFACE,
        "zxdg_decoration_manager_v1" => &xdg_decoration::ZXDG_DECORATION_MANAGER_V1_INTERFACE,
        "xdg_activation_v1" => &xdg_activation::XDG_ACTIVATION_V1_INTERFACE,
        "xdg_toplevel_icon_manager_v1" => &toplevel_icon::XDG_TOPLEVEL_ICON_MANAGER_V1_INTERFACE,
        "zxdg_exporter_v1" => &xdg_foreign_v1::ZXDG_EXPORTER_V1_INTERFACE,
        "zxdg_importer_v1" => &xdg_foreign_v1::ZXDG_IMPORTER_V1_INTERFACE,
        "zxdg_exporter_v2" => &xdg_foreign_v2::ZXDG_EXPORTER_V2_INTERFACE,
        "zxdg_importer_v2" => &xdg_foreign_v2::ZXDG_IMPORTER_V2_INTERFACE,
        "ext_idle_notifier_v1" => &idle_notify::EXT_IDLE_NOTIFIER_V1_INTERFACE,
        "ext_session_lock_manager_v1" => &session_lock::EXT_SESSION_LOCK_MANAGER_V1_INTERFACE,
        "ext_foreign_toplevel_list_v1" => &foreign_toplevel_list::EXT_FOREIGN_TOPLEVEL_LIST_V1_INTERFACE,
        "ext_output_image_capture_source_manager_v1" => &image_capture_source::EXT_OUTPUT_IMAGE_CAPTURE_SOURCE_MANAGER_V1_INTERFACE,
        "ext_foreign_toplevel_image_capture_source_manager_v1" => &image_capture_source::EXT_FOREIGN_TOPLEVEL_IMAGE_CAPTURE_SOURCE_MANAGER_V1_INTERFACE,
        "ext_image_copy_capture_manager_v1" => &image_copy_capture::EXT_IMAGE_COPY_CAPTURE_MANAGER_V1_INTERFACE,
        "ext_data_control_manager_v1" => &ext_data_control::EXT_DATA_CONTROL_MANAGER_V1_INTERFACE,
        "ext_workspace_manager_v1" => &ext_workspace::EXT_WORKSPACE_MANAGER_V1_INTERFACE,
        // Staging protocols kwin/Plasma 6 advertises that no wlroots
        // compositor in this table's coverage did -- Gap 1 (already
        // generated by wayland-protocols' `staging` feature, just not
        // wired in) -- see architecture-notes.md.
        "xdg_wm_dialog_v1" => &xdg_dialog::XDG_WM_DIALOG_V1_INTERFACE,
        "xdg_system_bell_v1" => &xdg_system_bell::XDG_SYSTEM_BELL_V1_INTERFACE,
        "wp_color_manager_v1" => &color_management::WP_COLOR_MANAGER_V1_INTERFACE,
        "wp_color_representation_manager_v1" => &color_representation::WP_COLOR_REPRESENTATION_MANAGER_V1_INTERFACE,
        "wp_fifo_manager_v1" => &fifo::WP_FIFO_MANAGER_V1_INTERFACE,
        // mutter/gnome-shell -- same Gap 1 pattern, staging feature
        // already enabled.
        "wp_commit_timing_manager_v1" => &commit_timing::WP_COMMIT_TIMING_MANAGER_V1_INTERFACE,
        // gnome-shell also advertises `gtk_shell1` -- a GNOME/GTK-internal
        // protocol (startup notification, launch tracking) that never went
        // through freedesktop's official process and isn't generated by
        // any of the four crates this table draws from. A permanent Gap 2,
        // not a bindable fix (see architecture-notes.md's Gap 2 list) --
        // safe to keep dropping, a real client's toplevel chain comes up
        // and survives a crash/reconnect without it.

        // wlroots' own extensions (wayland-protocols-wlr).
        "zwlr_data_control_manager_v1" => &wlr_data_control::ZWLR_DATA_CONTROL_MANAGER_V1_INTERFACE,
        "zwlr_export_dmabuf_manager_v1" => &export_dmabuf::ZWLR_EXPORT_DMABUF_MANAGER_V1_INTERFACE,
        "zwlr_foreign_toplevel_manager_v1" => &wlr_foreign_toplevel::ZWLR_FOREIGN_TOPLEVEL_MANAGER_V1_INTERFACE,
        "zwlr_gamma_control_manager_v1" => &gamma_control::ZWLR_GAMMA_CONTROL_MANAGER_V1_INTERFACE,
        "zwlr_layer_shell_v1" => &layer_shell::ZWLR_LAYER_SHELL_V1_INTERFACE,
        "zwlr_output_manager_v1" => &wlr_output_management::ZWLR_OUTPUT_MANAGER_V1_INTERFACE,
        "zwlr_output_power_manager_v1" => &output_power_management::ZWLR_OUTPUT_POWER_MANAGER_V1_INTERFACE,
        "zwlr_screencopy_manager_v1" => &screencopy::ZWLR_SCREENCOPY_MANAGER_V1_INTERFACE,
        "zwlr_virtual_pointer_manager_v1" => &virtual_pointer::ZWLR_VIRTUAL_POINTER_MANAGER_V1_INTERFACE,

        // KDE's extensions, plus orphaned-but-widely-used ones.
        "org_kde_kwin_server_decoration_manager" => &kde_server_decoration::ORG_KDE_KWIN_SERVER_DECORATION_MANAGER_INTERFACE,
        "zwp_input_method_manager_v2" => &input_method_v2::ZWP_INPUT_METHOD_MANAGER_V2_INTERFACE,
        "zwp_virtual_keyboard_manager_v1" => &virtual_keyboard::ZWP_VIRTUAL_KEYBOARD_MANAGER_V1_INTERFACE,

        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_core_interfaces_resolve() {
        for name in ["wl_display", "wl_registry", "wl_callback", "wl_compositor", "wl_surface", "wl_shm", "wl_seat"] {
            let iface = lookup_interface(name).unwrap_or_else(|| panic!("{name} should resolve"));
            assert_eq!(iface.name, name);
        }
    }

    #[test]
    fn known_xdg_shell_interfaces_resolve() {
        for name in ["xdg_wm_base", "xdg_surface", "xdg_toplevel", "xdg_popup", "xdg_positioner"] {
            let iface = lookup_interface(name).unwrap_or_else(|| panic!("{name} should resolve"));
            assert_eq!(iface.name, name);
        }
    }

    #[test]
    fn unknown_interface_returns_none() {
        assert!(lookup_interface("not_a_real_interface").is_none());
        assert!(lookup_interface("not_a_real_but_zwlr_shaped_v1").is_none());
    }

    /// Every global a real, fully-featured wlroots compositor (labwc
    /// 0.9.3 / wlroots 0.19.2) actually advertised in a live run against
    /// this proxy must resolve. A bare `cargo test` run, no live
    /// compositor needed, fails loudly the moment a real desktop's global
    /// list outpaces this table -- instead of the silent per-message drop
    /// that desyncs the new_id sequence otherwise.
    #[test]
    fn resolves_every_global_observed_from_real_labwc() {
        const OBSERVED_GLOBALS: &[&str] = &[
            "wl_shm",
            "zwp_linux_dmabuf_v1",
            "wp_linux_drm_syncobj_manager_v1",
            "ext_workspace_manager_v1",
            "zwlr_gamma_control_manager_v1",
            "zxdg_output_manager_v1",
            "zwlr_output_manager_v1",
            "wl_compositor",
            "wl_subcompositor",
            "wl_data_device_manager",
            "zwp_primary_selection_device_manager_v1",
            "zwp_input_method_manager_v2",
            "zwp_text_input_manager_v3",
            "wl_seat",
            "zwlr_virtual_pointer_manager_v1",
            "zwp_virtual_keyboard_manager_v1",
            "zwp_pointer_gestures_v1",
            "wp_cursor_shape_manager_v1",
            "xdg_wm_base",
            "xdg_activation_v1",
            "xdg_toplevel_icon_manager_v1",
            "org_kde_kwin_server_decoration_manager",
            "zxdg_decoration_manager_v1",
            "wp_presentation",
            "zwlr_export_dmabuf_manager_v1",
            "zwlr_screencopy_manager_v1",
            "ext_image_copy_capture_manager_v1",
            "ext_output_image_capture_source_manager_v1",
            "zwlr_data_control_manager_v1",
            "ext_data_control_manager_v1",
            "wp_security_context_manager_v1",
            "wp_viewporter",
            "wp_single_pixel_buffer_manager_v1",
            "wp_fractional_scale_manager_v1",
            "ext_idle_notifier_v1",
            "zwp_idle_inhibit_manager_v1",
            "zwp_relative_pointer_manager_v1",
            "zwp_pointer_constraints_v1",
            "zwlr_foreign_toplevel_manager_v1",
            "ext_foreign_toplevel_list_v1",
            "wp_alpha_modifier_v1",
            "ext_session_lock_manager_v1",
            "zwlr_output_power_manager_v1",
            "wp_tearing_control_manager_v1",
            "zwp_tablet_manager_v2",
            "zwlr_layer_shell_v1",
            "zxdg_exporter_v1",
            "zxdg_importer_v1",
            "zxdg_exporter_v2",
            "zxdg_importer_v2",
            "wl_output",
        ];
        for name in OBSERVED_GLOBALS {
            lookup_interface(name).unwrap_or_else(|| panic!("{name} (observed from a real compositor) should resolve"));
        }
    }

    /// Same idea as `resolves_every_global_observed_from_real_labwc`, but
    /// for kwin/Plasma 6 (6.4.5, `--virtual` backend) -- a different
    /// protocol family than labwc/sway's wlroots, so a distinct set of
    /// staging protocols showed up that wlroots compositors never
    /// advertised.
    #[test]
    fn resolves_every_global_observed_from_real_kwin() {
        const KWIN_OBSERVED_GLOBALS: &[&str] = &[
            "xdg_wm_dialog_v1",
            "xdg_system_bell_v1",
            "wp_color_manager_v1",
            "wp_color_representation_manager_v1",
            "wp_fifo_manager_v1",
        ];
        for name in KWIN_OBSERVED_GLOBALS {
            lookup_interface(name).unwrap_or_else(|| panic!("{name} (observed from a real kwin) should resolve"));
        }
    }

    /// Same idea again, for mutter/gnome-shell (50.1, `--headless --no-x11`).
    /// Only one new resolvable gap here (`wp_commit_timing_manager_v1`);
    /// the other new global mutter advertises, `gtk_shell1`, is a
    /// permanent Gap 2 (see the module doc) and deliberately has no entry
    /// here to test for.
    #[test]
    fn resolves_every_global_observed_from_real_mutter() {
        const MUTTER_OBSERVED_GLOBALS: &[&str] = &["wp_commit_timing_manager_v1"];
        for name in MUTTER_OBSERVED_GLOBALS {
            lookup_interface(name).unwrap_or_else(|| panic!("{name} (observed from a real mutter) should resolve"));
        }
    }
}
