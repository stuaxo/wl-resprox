# Wayland Crash Resilience Proxy: Project Plan

## Phase 1: Environment Setup

- [ ] Spin up Distrobox container (`wayland-proxy-dev`).
- [ ] Run Ansible playbook (`playbook.yml`) to provision Rust, GTK4, and Wayland tools.
- [ ] Verify nested compositor (`labwc`) can be launched inside a host window.
- [ ] Verify `gtk4-demo` runs successfully inside `labwc`.
- [ ] Confirm independent crash behavior (killing `labwc` kills `gtk4-demo` but leaves host intact).

## Phase 2: Rust Proxy Foundation

- [ ] Scaffold standard `cargo` project.
- [ ] Create UNIX socket listener (mocking a Wayland display, e.g., `wayland-2`).
- [ ] Implement `tokio` async loop to accept incoming client connections.
- [ ] Connect out to the active compositor (e.g., `wayland-1`).

## Phase 3: The "Pass-Through" Prototype

- [ ] Pipe raw bytes between the Client (GTK) and the Server (`labwc`).
- [ ] Test: Launch `WAYLAND_DISPLAY=wayland-2 gtk4-demo` and ensure it appears in the `labwc` window.

## Phase 4: State Tracking & ID Translation

- [ ] Review [sommelier-rs](https://github.com/google/sommelier-rs)'s Shadow Table and codegen approach, and [waypipe](https://github.com/deepin-community/waypipe)'s ID-remap handling, before implementing — both solve this exact problem.
- [ ] Implement `wayland-backend` parsing to deserialize the byte stream into typed Wayland messages.
- [ ] Build the Shadow Table (using `bimap`) to track Object IDs.
- [ ] Intercept `wl_registry` requests to map globals.
- [ ] Rewrite Object IDs on-the-fly for all traversing messages.

## Phase 5: Crash Recovery Mechanics

- [ ] Detect Server socket drop (`ECONNRESET`).
- [ ] Pause proxying; keep Client socket open and suspend frame callbacks.
- [ ] Detect new Server socket (when `labwc` is restarted).
- [ ] Re-request `wl_registry` globals from the new Server.
- [ ] Re-create `wl_surface` and `xdg_toplevel` objects on the new Server based on tracked state.
- [ ] Synthesize `xdg_surface.configure` to trigger GTK repaint.

## References

- [GTK MR !4073](https://gitlab.gnome.org/GNOME/gtk/-/merge_requests/4073) — Draft toolkit-level reconnection support (draft, blocked on libwayland/mesa deps)
- [Waypipe](https://github.com/deepin-community/waypipe) — Wayland forwarding proxy; ID translation, buffer serialization
- [Sommelier-rs](https://github.com/google/sommelier-rs) — Rust Wayland VM proxy; Shadow Table + protocol codegen
- [stransky/wayland-proxy](https://github.com/stransky/wayland-proxy) — Firefox-motivated Wayland proxy (C++)
- [Mozilla Bugzilla #1743144](https://bugzilla.mozilla.org/show_bug.cgi?id=1743144) — motivating bug for the above
