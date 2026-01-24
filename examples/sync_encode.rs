//! Example demonstrating synchronous HTTP encoding without requiring an async runtime.
//!
//! This example shows how to use `WireEncodeSync` to encode HTTP requests and responses
//! in a synchronous context (e.g., in a non-async main function or library).
//!
//! Run with: cargo run --example sync_encode

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::{Empty, Full};
use http_wire::WireEncodeSync;

fn main() {
    println!("=== Synchronous HTTP Encoding Example ===\n");

    // Example 1: Simple GET request
    println!("1. Simple GET request:");
    let request = Request::builder()
        .method("GET")
        .uri("/api/users")
        .header("Host", "example.com")
        .header("User-Agent", "http_wire/0.2.5")
        .body(Empty::<Bytes>::new())
        .unwrap();

    match request.encode_sync() {
        Ok(bytes) => {
            let output = String::from_utf8_lossy(&bytes);
            println!("{}", output);
            println!("Total bytes: {}\n", bytes.len());
        }
        Err(e) => eprintln!("Error: {}\n", e),
    }

    // Example 2: POST request with JSON body
    println!("2. POST request with JSON body:");
    let body = r#"{"name":"John Doe","email":"john@example.com"}"#;
    let request = Request::builder()
        .method("POST")
        .uri("/api/users")
        .header("Host", "example.com")
        .header("Content-Type", "application/json")
        .header("Content-Length", body.len().to_string())
        .body(Full::new(Bytes::from(body)))
        .unwrap();

    match request.encode_sync() {
        Ok(bytes) => {
            let output = String::from_utf8_lossy(&bytes);
            println!("{}", output);
            println!("Total bytes: {}\n", bytes.len());
        }
        Err(e) => eprintln!("Error: {}\n", e),
    }

    // Example 3: Request with query parameters
    println!("3. GET request with query parameters:");
    let request = Request::builder()
        .method("GET")
        .uri("/api/search?q=rust&limit=10")
        .header("Host", "api.example.com")
        .body(Empty::<Bytes>::new())
        .unwrap();

    match request.encode_sync() {
        Ok(bytes) => {
            let output = String::from_utf8_lossy(&bytes);
            println!("{}", output);
            println!("Total bytes: {}\n", bytes.len());
        }
        Err(e) => eprintln!("Error: {}\n", e),
    }

    // Example 4: HTTP Response - 200 OK
    println!("4. HTTP Response - 200 OK:");
    let response = Response::builder()
        .status(200)
        .header("Content-Type", "application/json")
        .header("Server", "http_wire/0.2.5")
        .body(Full::new(Bytes::from(r#"{"status":"success"}"#)))
        .unwrap();

    match response.encode_sync() {
        Ok(bytes) => {
            let output = String::from_utf8_lossy(&bytes);
            println!("{}", output);
            println!("Total bytes: {}\n", bytes.len());
        }
        Err(e) => eprintln!("Error: {}\n", e),
    }

    // Example 5: HTTP Response - 404 Not Found
    println!("5. HTTP Response - 404 Not Found:");
    let response = Response::builder()
        .status(404)
        .header("Content-Type", "text/plain")
        .body(Full::new(Bytes::from("Resource not found")))
        .unwrap();

    match response.encode_sync() {
        Ok(bytes) => {
            let output = String::from_utf8_lossy(&bytes);
            println!("{}", output);
            println!("Total bytes: {}\n", bytes.len());
        }
        Err(e) => eprintln!("Error: {}\n", e),
    }

    // Example 6: HTTP Response - 204 No Content
    println!("6. HTTP Response - 204 No Content:");
    let response = Response::builder()
        .status(204)
        .header("Server", "http_wire")
        .body(Empty::<Bytes>::new())
        .unwrap();

    match response.encode_sync() {
        Ok(bytes) => {
            let output = String::from_utf8_lossy(&bytes);
            println!("{}", output);
            println!("Total bytes: {}\n", bytes.len());
        }
        Err(e) => eprintln!("Error: {}\n", e),
    }

    println!("=== All examples completed successfully ===");
}