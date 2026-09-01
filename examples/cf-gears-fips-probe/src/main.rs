//! cf-gears-fips-probe — outbound HTTPS smoke for the toolkit / toolkit-http FIPS path.
//!
//! Boots the same crypto stack that cf-gears-example-server uses (`toolkit::bootstrap::
//! init_crypto_provider` → corecrypto on macOS+fips, Windows CNG on
//! Windows+fips, AWS-LC FIPS on Linux+fips, AWS-LC default otherwise),
//! constructs a `toolkit_http::HttpClient`, makes a single `GET` request,
//! and prints what the server saw.
//!
//! When pointed at `https://www.howsmyssl.com/a/check` (default), the
//! response JSON includes the negotiated TLS version, the selected cipher
//! suite, and the full `ClientHello.cipher_suites` list — enough to spot
//! a non-FIPS suite (e.g. ChaCha20-Poly1305) accidentally being offered.
//!
//! ## Usage
//!
//! ```sh
//! cargo run -p cf-gears-fips-probe                              # non-FIPS build
//! cargo run -p cf-gears-fips-probe --features fips              # FIPS-conformant build
//! cargo run -p cf-gears-fips-probe --features fips -- --url https://example.com
//! ```

use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use toolkit_http::HttpClientBuilder;

#[derive(Parser, Debug)]
#[command(
    name = "cf-gears-fips-probe",
    about = "Outbound HTTPS smoke for the toolkit FIPS path"
)]
struct Args {
    /// HTTPS URL to GET.
    #[arg(long, default_value = "https://www.howsmyssl.com/a/check")]
    url: String,

    /// Print up to N bytes of the response body to stdout.
    #[arg(long, default_value_t = 4096)]
    body_preview_bytes: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // === step 1: install the crypto provider via the canonical bootstrap. ===
    toolkit::bootstrap::init_crypto_provider().context("install rustls crypto provider")?;
    println!("[1] crypto provider installed");
    println!(
        "    build profile: {}",
        if cfg!(feature = "fips") {
            "FIPS-enabled"
        } else {
            "default (non-FIPS)"
        }
    );
    println!(
        "    target_os: {} -> {}",
        std::env::consts::OS,
        if cfg!(all(feature = "fips", target_os = "macos")) {
            "Apple corecrypto"
        } else if cfg!(all(feature = "fips", target_os = "windows")) {
            "Windows CNG (FIPS-approved set)"
        } else if cfg!(all(
            feature = "fips",
            not(any(target_os = "macos", target_os = "windows"))
        )) {
            "AWS-LC FIPS"
        } else {
            "AWS-LC (default, non-FIPS)"
        }
    );

    // === step 2: build a real toolkit-http client. ===
    let client = HttpClientBuilder::new()
        .timeout(Duration::from_secs(15))
        .user_agent("cf-gears-fips-probe/0.1")
        .build()
        .context("build HttpClient")?;
    println!("[2] toolkit-http client ready");

    // === step 3: outbound GET. ===
    let args = Args::parse();
    println!("[3] GET {}", args.url);
    let response = client
        .get(&args.url)
        .send()
        .await
        .context("HTTP request failed")?;

    let status = response.status();
    let headers = response.headers().clone();
    let body = response.bytes().await.context("read body")?;

    println!("    status: {status}");
    if let Some(server) = headers.get("server").and_then(|v| v.to_str().ok()) {
        println!("    server header: {server}");
    }
    println!("    bytes: {}", body.len());

    let preview_len = body.len().min(args.body_preview_bytes);
    let preview = &body[..preview_len];
    println!("--- body (first {preview_len} bytes) ---");
    match std::str::from_utf8(preview) {
        Ok(s) => println!("{s}"),
        Err(_) => println!("(non-UTF8 payload, {preview_len} bytes)"),
    }
    println!("--- end body ---");

    if !status.is_success() {
        anyhow::bail!("non-success status {status}");
    }

    // Heuristic FIPS-conformance hints when hitting howsmyssl.
    if args.url.contains("howsmyssl.com") {
        let body_str = String::from_utf8_lossy(&body);
        println!();
        println!("[probe] heuristics on howsmyssl JSON:");
        for needle in [
            "\"tls_version\"",
            "\"rating\"",
            "\"unknown_cipher_suite_supported\"",
            "\"beast_vuln\"",
            "\"session_ticket_supported\"",
        ] {
            if let Some(idx) = body_str.find(needle) {
                let end = body_str[idx..]
                    .find(',')
                    .map_or(body_str.len().min(idx + 80), |e| idx + e);
                println!("  {}", &body_str[idx..end]);
            }
        }
        // Quick "did the client OFFER ChaCha?" check on `given_cipher_suites`.
        if body_str.contains("CHACHA20") {
            println!("  [!] ChaCha20 cipher offered by ClientHello -- NOT FIPS-conformant");
        } else {
            println!("  [OK] No ChaCha20 in ClientHello cipher_suites");
        }
    }

    Ok(())
}
