ADR 0002: Adopt "Byte Munger" Architecture for Wayland Proxying

Status

Accepted

Context

Wayland is an object-oriented protocol where entities (surfaces, buffers, keyboards) are tracked via integer IDs (u32). To survive a compositor crash, a proxy must maintain a bidirectional translation table (a Shadow Table) of these IDs between the client (GTK) and the newly spawned server (Compositor), as the new server will assign different IDs to recreated objects.

Initially, we attempted to use the standard Rust wayland-backend library. However, this library (like libwayland in C) is strictly designed for Endpoints (a pure Client or a pure Server). It abstracts integer Object IDs into opaque handles and internally manages ID allocation namespaces (e.g., servers auto-allocate from 0xff000000, clients from 1).

Trying to build a proxy using Endpoint libraries requires instantiating a full Server and a full Client and somehow bridging their opaque, internally managed state machines. This creates immense friction and fights the library's design. Prior art in the ecosystem, such as ChromeOS's sommelier-rs and freedesktop's waypipe, independently arrived at the same conclusion: they bypass endpoint libraries and manually parse the wire format.

Decision

We will adopt a "Byte Munger" architecture for the proxy.

Direct Wire Parsing: Instead of endpoint abstractions, the proxy will manually read the 8-byte Wayland header ([Sender ID: u32][Opcode: u16][Length: u16]) and the raw byte payload from the UNIX socket.

Raw ID Translation: Object IDs will be treated as raw u32 integers and translated on-the-fly directly within the byte buffer using a bidirectional map (bimap).

The Hybrid Approach for Signatures: Instead of writing a custom XML parser/code-generator to know where nested IDs are located inside a message payload, we will retain wayland-client and wayland-server strictly as static data dictionaries. We will use their generated Interface and MessageDesc types to look up message signatures (&[ArgumentType]) at runtime, completely ignoring their I/O and state management machinery.

Consequences

Positive

Absolute Control: We gain exact control over the mapping and lifecycles of u32 IDs, which is the foundational requirement for successfully faking GTK's state after a crash.

Reduced Overhead: We avoid the memory and CPU overhead of running full Wayland Endpoint state machines for both sides of the connection.

Scope Containment: By reusing wayland-backend for static signature lookups, we avoid the massive scope creep of writing a custom Wayland XML protocol code-generator.

Negative

Manual Buffer Manipulation: We are responsible for safely slicing byte buffers, advancing pointers based on the static signature array, and mutating raw bytes safely. This introduces potential parsing bugs if payload lengths do not strictly align with the static signatures.
