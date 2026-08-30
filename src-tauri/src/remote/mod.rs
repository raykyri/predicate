//! Remote control of qmux from a paired device (docs/remote-control-plan.md).
//!
//! Everything here is dark until the user turns remote control on: no
//! endpoint is bound, nothing is advertised, and no relay connection exists.
//! The transport is iroh (QUIC): endpoint identity is an ed25519 key, and
//! dialing/accepting against a known [`iroh::EndpointId`] is what stands in
//! for the certificate pinning a hand-rolled TLS listener would need.
//!
//! The backend stays synchronous. The async side (this module's runtime
//! thread) owns connections and streams; it meets `AppState` only at
//! channels and at `spawn_blocking` calls, and never holds a `std::sync`
//! lock across an await.

pub mod devices;
pub mod endpoint;
pub mod fanout;
pub mod frames;
pub mod pairing;
pub mod probe;
pub mod session;
