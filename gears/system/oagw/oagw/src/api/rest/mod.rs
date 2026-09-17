//! REST modules for the OAGW gear.

pub mod dto;
pub mod handlers;
pub mod routes;

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, RwLock};

/// Shared cell holding the data-plane bridge listening port and its relay
/// secret.
///
/// Both are written by `OagwGear::serve` once the pingora bridge is bound;
/// they are read by the proxy handler on every proxied request (the port so
/// it can relay, the secret so it can authenticate the internal request).
/// Port `0` means the data plane is not (yet) running, which the handler
/// answers with `503`. Kept separate from the gear struct so the axum surface
/// receives a cheap `Clone` handle.
#[derive(Clone, Default)]
pub struct ProxyPort {
    port: Arc<AtomicU16>,
    relay_secret: Arc<RwLock<Option<String>>>,
}

impl ProxyPort {
    /// Creates an unset port cell.
    #[must_use]
    pub fn new() -> Self {
        Self {
            port: Arc::new(AtomicU16::new(0)),
            relay_secret: Arc::new(RwLock::new(None)),
        }
    }

    /// Records the bound bridge port.
    pub fn bind(&self, port: u16) {
        self.port.store(port, Ordering::SeqCst);
    }

    /// The currently bound bridge port (0 = not running).
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port.load(Ordering::SeqCst)
    }

    /// Records the per-bridge relay secret the surface must stamp on every
    /// internal request.
    pub fn bind_relay_secret(&self, secret: String) {
        if let Ok(mut slot) = self.relay_secret.write() {
            *slot = Some(secret);
        }
    }

    /// The relay secret the surface must present to the bridge, if bound.
    #[must_use]
    pub fn relay_secret(&self) -> Option<String> {
        self.relay_secret.read().ok().and_then(|s| s.clone())
    }
}
