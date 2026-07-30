# Architectural Context: Crash-Resilient Wayland Proxy

## 1. The Core Problem & Political Context

- **The Issue:** Wayland clients crash instantly if the compositor (e.g., Mutter) crashes because the UNIX socket closes, destroying the server-side state.
- **The Ecosystem Divide:** Qt/KDE implemented robust toolkit-level reconnection (rebuilding globals/surfaces on the fly). GTK developers (notably Benjamin Otte) have rejected similar proposals (like [MR !4073, "Draft: Add support for compositor handoffs"](https://gitlab.gnome.org/GNOME/gtk/-/merge_requests/4073)).
- **GTK's Stance:** GTK maintainers argue that hiding a crash from the toolkit breaks the state machine (e.g., stuck keyboard grabs, lost drag-and-drop state, broken clipboard ownership) and prefer a "pure" API approach where compositors simply shouldn't crash.
- **Our Stance:** Pure pragmatism. The user losing a DND operation is acceptable; losing an hour of work in an application is not.

## 2. The Solution Strategy: A "tmux" for Wayland

Instead of patching GTK, we are building a standalone Rust daemon that sits between the GTK application and the compositor.

- **Crash Isolation:** The proxy catches the `ECONNRESET` when Mutter dies. It keeps the client-side socket alive, effectively freezing the GTK app.
- **Recovery:** When the compositor restarts, the proxy reconnects, re-requests globals, and rebuilds the `wl_surface` states.

## 3. Key Technical Mechanisms

To make this work seamlessly, the proxy must perform three critical tasks:

1. **ID Translation (Shadow Table):** Wayland is object-oriented with integer IDs. When reconnecting, the new compositor will assign different IDs to recreated objects. The proxy uses a `bimap` (bidirectional map) to rewrite IDs on the fly, ensuring GTK continues to see the original IDs it expects.
2. **Triggering Repaints (`xdg_surface.configure`):** To get GTK to draw onto the new compositor, the proxy must synthesize/forward a configure event. GTK will naturally respond by repainting its widgets and attaching the memory buffer to the new surface.
3. **Faking State (Handling the Edge Cases):** To satisfy the GTK state machine, the proxy may need to synthesize events during a crash (e.g., sending a `wl_pointer.leave` or fake button release) to ensure GTK doesn't get permanently stuck in a grab state if the crash happened mid-click.

## 4. Architectural Pivot: The "Byte Munger" vs "Endpoint" Model

We initially built the proxy on `wayland-backend` (the low-level crate behind
`wayland-client`/`wayland-server`), using its `ObjectData`/`GlobalHandler`
dispatch model. That worked (verified live against a real compositor and
`gtk4-demo`), but comparing it against actual prior art -- ChromeOS's
sommelier-rs and freedesktop's waypipe (see `reference/`, gitignored local
checkouts) -- surfaced a real problem: **neither of them uses a backend
library at all.** Both hand-parse the wire format directly and dispatch
through a codegen'd interface table, treating object IDs as plain `u32`s
the whole way through.

The reason: libraries like `wayland-backend`/`libwayland` are built for
**endpoints** -- a pure client or a pure server, not both stitched
together. They abstract object IDs into opaque handles and manage
allocation internally (servers allocate from `0xff000000`, clients from
`1`, per the wire protocol's own convention). A proxy needs the opposite:
direct manipulation of raw `u32` IDs across two *independent* sessions,
rewriting them as messages cross from one to the other. Endpoint
abstractions actively fight that -- this is exactly what produced the
friction we hit (the `wl_display` bootstrap failing with `InvalidId`
since it isn't retrievable as a normal object; needing two distinct
`ObjectId` types with a bridge between them just to satisfy the type
system).

Going forward, like sommelier-rs and waypipe, the proxy is a **"Byte
Munger"**: it hand-parses the 8-byte Wayland header
(`[Sender ID: u32][Opcode: u16][Length: u16]`, see `src/wire.rs`) directly
off the Unix socket, mutates the integer IDs in place via a `bimap`-based
Shadow Table (Phase 4), and forwards the modified byte stream. This
requires either hardcoding protocol byte-offsets or build-time XML codegen
to know which bytes in a payload are `object`/`new_id` typed -- not yet
decided, see `docs/plan/plan-0001-proxy-core-and-crash-recovery.md`'s
Phase 3.5/4.

## 5. Prior Art & Inspiration

- **Waypipe / Sommelier:** Network/VM proxies that heavily utilize Wayland ID translation and serialization.
  - [Waypipe](https://github.com/deepin-community/waypipe) (mirror; canonical upstream is `gitlab.freedesktop.org/mstoeckl/waypipe`)
  - [Sommelier-rs](https://github.com/google/sommelier-rs) — Rust rewrite of ChromeOS's Sommelier; explicitly uses a "Shadow Table" for object ID mapping, same approach as ours.
- **Stransky's Firefox Proxy:** An experimental middleman built by a Red Hat developer to prevent Mutter from killing Firefox during high-load Wayland message jams.
  - [stransky/wayland-proxy](https://github.com/stransky/wayland-proxy) (C++; itself a port of an earlier Rust project, `the8472/weyland-p5000`)
  - Motivating bug: [Mozilla Bugzilla #1743144](https://bugzilla.mozilla.org/show_bug.cgi?id=1743144)
