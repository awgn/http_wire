//! Example comparing synchronous vs asynchronous HTTP encoding.
//!
//! This example demonstrates both `WireEncodeSync` and `WireEncode` to show
//! when to use each approach.
//!
//! Run with: cargo run --example sync_vs_async

use bytes::Bytes;
use http::Request;
use http_body_util::Full;
use http_wire::{WireEncode, WireEncodeSync};

fn main() {
    println!("=== Sync vs Async HTTP Encoding ===\n");

    // Scenario 1: Synchronous encoding in regular functions
    println!("Scenario 1: Using WireEncodeSync in a regular function");
    println!("-----------------------------------------------------------");
    synchronous_example();
    println!();

    // Scenario 2: Async encoding in async context
    println!("Scenario 2: Using WireEncode in an async context");
    println!("-----------------------------------------------------------");
    async_example();
}

/// Example of synchronous encoding - no async runtime needed
fn synchronous_example() {
    println!("This function is NOT async and doesn't require Tokio runtime");

    let request = Request::builder()
        .method("GET")
        .uri("/api/data")
        .header("Host", "example.com")
        .body(Full::new(Bytes::from("request data")))
        .unwrap();

    // Use encode_sync() - works in regular synchronous code
    match request.encode_sync() {
        Ok(bytes) => {
            println!("✓ Encoded {} bytes synchronously", bytes.len());
            println!("  First 50 chars: {:?}", &String::from_utf8_lossy(&bytes[..50.min(bytes.len())]));
        }
        Err(e) => eprintln!("✗ Error: {}", e),
    }

    println!("\nUse WireEncodeSync when:");
    println!("  • You're in synchronous code (no async runtime)");
    println!("  • You're writing CLI tools or scripts");
    println!("  • You're in a library that needs to support both sync and async users");
    println!("  • You want simple, straightforward blocking behavior");
}

/// Example of async encoding - requires async runtime
fn async_example() {
    println!("This function uses async/await with Tokio runtime");

    // Create a Tokio runtime for this example
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let request = Request::builder()
            .method("POST")
            .uri("/api/submit")
            .header("Host", "example.com")
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(r#"{"key":"value"}"#)))
            .unwrap();

        // Use encode() - async version
        match request.encode().await {
            Ok(bytes) => {
                println!("✓ Encoded {} bytes asynchronously", bytes.len());
                println!("  First 50 chars: {:?}", &String::from_utf8_lossy(&bytes[..50.min(bytes.len())]));
            }
            Err(e) => eprintln!("✗ Error: {}", e),
        }

        println!("\nUse WireEncode (async) when:");
        println!("  • You're already in an async runtime (e.g., existing Tokio app)");
        println!("  • You're building async web servers or clients");
        println!("  • You need to integrate with other async operations");
        println!("  • You want non-blocking behavior in concurrent applications");
    });
}

/// Performance comparison example
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn benchmark_sync_encoding() {
        let start = std::time::Instant::now();
        
        for _ in 0..1000 {
            let request = Request::builder()
                .method("GET")
                .uri("/test")
                .header("Host", "example.com")
                .body(Full::new(Bytes::from("test")))
                .unwrap();

            let _ = request.encode_sync().unwrap();
        }

        let duration = start.elapsed();
        println!("Sync encoding 1000 requests: {:?}", duration);
    }

    #[tokio::test]
    async fn benchmark_async_encoding() {
        let start = std::time::Instant::now();
        
        for _ in 0..1000 {
            let request = Request::builder()
                .method("GET")
                .uri("/test")
                .header("Host", "example.com")
                .body(Full::new(Bytes::from("test")))
                .unwrap();

            let _ = request.encode().await.unwrap();
        }

        let duration = start.elapsed();
        println!("Async encoding 1000 requests: {:?}", duration);
    }
}