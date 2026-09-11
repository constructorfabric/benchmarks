//! Domain services of the OAGW gear.
//!
//! Entry 2.2 (this feature) adds [`management`], the domain half of the ten
//! management flows; entry 2.4 adds the proxy service here, each consuming the
//! loaded [`OagwConfig`](crate::config::OagwConfig) stored on the gear struct.

pub mod management;
