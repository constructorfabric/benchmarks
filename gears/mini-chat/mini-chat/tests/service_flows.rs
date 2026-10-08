//! In-process flow tests of the domain service (temporary SQLite, fake OAGW).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names
)]

mod common;

use common::*;

#[tokio::test]
async fn smoke_send_message() {
    let h = Harness::new().await;
    let c = ctx(TENANT_A, USER_A1);
    let chat = h.svc.create_chat(&c, Some("t".into()), None).await.unwrap();
    assert_eq!(chat.chat.model, "prem");
    h.stop().await;
}
