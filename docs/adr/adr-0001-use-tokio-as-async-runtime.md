ADR 0001: Use Tokio as the Async Runtime

Status

Accepted

Context

The Wayland proxy sits between a GTK client and a Wayland compositor. It is fundamentally an I/O-bound application that must multiplex bidirectional UNIX domain sockets. Furthermore, to survive compositor crashes, the proxy must manage a complex state machine: it must detect ECONNRESET on the server socket, intentionally pause reading/forwarding on the client socket, and handle synthetic timers to prevent the GTK client from hanging.

Implementing this natively in C or synchronous Rust requires manual epoll loops and timer file descriptors, which are highly error-prone.

Decision

We will use tokio as the asynchronous runtime to handle socket multiplexing, I/O streams, and sleep timers.

Consequences

Positive

Simplicity: The complex crash-recovery state machine can be handled using select! macros and structured concurrency rather than manual epoll state tracking.

First-Class UNIX Sockets: tokio::net::UnixListener and UnixStream provide robust primitives for Wayland's local IPC.

Ecosystem Compatibility: tokio is the de facto standard in the Rust ecosystem. Future integrations (e.g., D-Bus monitoring for GNOME Shell restarts or SCM_RIGHTS file descriptor passing crates) will seamlessly integrate without runtime compatibility layers.

Negative

Binary Size/Overhead: tokio introduces a heavier dependency footprint compared to a raw epoll implementation or lighter runtimes like smol. However, for a desktop daemon, this overhead is negligible compared to the stability benefits.
