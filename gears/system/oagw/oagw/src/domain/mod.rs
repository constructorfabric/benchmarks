//! Domain layer of the OAGW gear: business logic and domain models, with no
//! dependency on transport or infrastructure concerns (DESIGN §3.2).

pub mod guards;
pub mod headers;
pub mod metrics;
pub mod policy;
pub mod routing;
pub mod services;
pub mod storage;
pub mod types;
