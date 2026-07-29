# Implementation Constraints

This is a stopgap until upstream toolkits handle compositor crashes natively
([GTK MR !4073](https://gitlab.gnome.org/GNOME/gtk/-/merge_requests/4073) is
the closest attempt so far, still draft/unmergeable). Don't over-engineer it.
Follow these rules exactly.

## ID Translation
- Every object ID crossing the proxy MUST be rewritten via the shadow `bimap`
  before forwarding, in both directions. No exceptions, no "just this one."

## On Server Disconnect (`ECONNRESET`)
- Freeze the client socket. Do NOT close it.
- Buffer or drop outgoing client requests. Do NOT forward them anywhere.
- Do NOT forward a connection error to the client. The client must believe
  nothing happened.

## On Server Reconnect
- Re-bind all `wl_registry` globals from scratch. Do not assume any global
  persists across compositor instances.
- Recreate `wl_surface` / `xdg_toplevel` on the new server using the shadow
  table's tracked state.
- Synthesize `xdg_surface.configure` immediately after recreation to force a
  GTK repaint.

## Grab State (mid-interaction crash)
- If a pointer or keyboard grab was active at disconnect, synthesize
  `wl_pointer.leave` and/or a fake button-release BEFORE resuming traffic.
  A stuck grab is worse than a dropped click. Never skip this.

## Buffer Lifetimes
- Never forward `wl_buffer.release` for a buffer the new compositor doesn't
  know about yet. Drop it silently.

## Non-Goals
- Do not attempt to preserve drag-and-drop state across a crash. Losing DND
  is acceptable. Losing app state is not.
- Do not attempt to fix this in GTK/Mutter. This proxy exists because that
  path is blocked upstream (see
  [GTK MR !4073](https://gitlab.gnome.org/GNOME/gtk/-/merge_requests/4073)).
  Don't relitigate it in code review — just make the proxy work.
