//! `OData` filter field declarations of the list endpoints.

#![allow(dead_code)]

use toolkit_odata_macros::ODataFilterable;

/// `GET /v1/chats` filter / order fields.
#[derive(ODataFilterable)]
pub struct ChatQueryFields {
    #[odata(filter(kind = "DateTimeUtc"))]
    pub updated_at: String,
    #[odata(filter(kind = "Uuid"))]
    pub id: String,
    #[odata(filter(kind = "String"))]
    pub title: String,
}

/// `GET /v1/chats/{id}/messages` filter / order fields.
#[derive(ODataFilterable)]
pub struct MessageQueryFields {
    #[odata(filter(kind = "DateTimeUtc"))]
    pub created_at: String,
    #[odata(filter(kind = "Uuid"))]
    pub id: String,
    #[odata(filter(kind = "String"))]
    pub role: String,
}
