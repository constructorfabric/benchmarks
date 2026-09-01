//! Throwaway probe: reproduces the connect failure outside the gear.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use http_body_util::Full;
use hyper::Request;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

#[tokio::test]
async fn probe_plain_http_connect() {
    // A listener that accepts and replies 200.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:19222").await.unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let mut sock = sock;
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nhi")
                    .await;
            });
        }
    });

    let mut http = HttpConnector::new();
    http.set_connect_timeout(Some(std::time::Duration::from_secs(2)));
    http.enforce_http(false);
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .unwrap()
        .https_or_http()
        .enable_all_versions()
        .wrap_connector(http);
    let mut builder = Client::builder(TokioExecutor::new());
    builder.pool_timer(hyper_util::rt::TokioTimer::default());
    let client: Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<bytes::Bytes>> =
        builder.build(connector);

    let req = Request::builder()
        .method("GET")
        .uri("http://127.0.0.1:19222/probe")
        .header("host", "127.0.0.1:19222")
        .body(Full::new(bytes::Bytes::new()))
        .unwrap();
    match client.request(req).await {
        Ok(resp) => println!("PROBE OK status={}", resp.status()),
        Err(err) => {
            println!("PROBE ERR display={err}");
            let mut source: Option<&dyn std::error::Error> =
                std::error::Error::source(&err);
            let mut depth = 0;
            while let Some(s) = source {
                println!("  source[{depth}] = {s}");
                source = s.source();
                depth += 1;
            }
            panic!("probe failed");
        }
    }
}
