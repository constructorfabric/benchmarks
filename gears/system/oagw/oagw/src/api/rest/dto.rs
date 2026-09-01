//! Request/response DTO bindings for OAGW managed entities.
//!
//! The domain model doubles as the wire model (no field renaming between the
//! two), so the only thing these types need is the toolkit's marker traits and
//! the `utoipa::ToSchema` derives already on the domain structs.

use toolkit::api::api_dto::{RequestApiDto, ResponseApiDto};

use crate::domain::model::{Plugin, Route, Upstream};

impl RequestApiDto for Upstream {}
impl ResponseApiDto for Upstream {}

impl RequestApiDto for Route {}
impl ResponseApiDto for Route {}

impl RequestApiDto for Plugin {}
impl ResponseApiDto for Plugin {}
