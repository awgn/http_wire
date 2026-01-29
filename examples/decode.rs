//! Example demonstrating HTTP request and response decoding.
//!
//! This example shows how to use `WireDecode` to parse raw HTTP bytes
//! into structured request and response objects.
//!
//! Run with: cargo run --example decode

use http_wire::WireDecode;
use http_wire::request::FullRequest;
use http_wire::response::FullResponse;
use std::mem::MaybeUninit;

fn main() {
    println!("=== HTTP Decoding Examples ===\n");

    // Example 1: Decode a simple GET request
    println!("1. Decoding a simple GET request:");
    println!("-----------------------------------");
    decode_simple_request();
    println!();

    // Example 2: Decode a POST request with body
    println!("2. Decoding a POST request with body:");
    println!("--------------------------------------");
    decode_post_request();
    println!();

    // Example 3: Decode using optimized uninit headers (Request only)
    println!("3. Decoding with uninitialized headers (optimized):");
    println!("---------------------------------------------------");
    decode_request_optimized();
    println!();

    // Example 4: Decode a chunked request
    println!("4. Decoding a chunked transfer-encoding request:");
    println!("------------------------------------------------");
    decode_chunked_request();
    println!();

    // Example 5: Decode HTTP responses
    println!("5. Decoding HTTP responses:");
    println!("---------------------------");
    decode_responses();
    println!();

    // Example 6: Handling incomplete messages
    println!("6. Handling incomplete messages:");
    println!("--------------------------------");
    handle_incomplete_messages();
    println!();

    println!("=== All examples completed successfully ===");
}

fn decode_simple_request() {
    let raw = b"GET /api/users HTTP/1.1\r\nHost: example.com\r\nUser-Agent: curl/7.68.0\r\n\r\n";

    // Allocate headers storage
    let mut headers = [httparse::EMPTY_HEADER; 16];

    match FullRequest::decode(raw, &mut headers) {
        Ok((request, total_len)) => {
            println!("✓ Successfully decoded request");
            println!("  Method: {}", request.head.method.unwrap());
            println!("  Path: {}", request.head.path.unwrap());
            println!("  Version: HTTP/1.{}", request.head.version.unwrap());
            println!("  Headers:");
            for header in request.head.headers {
                println!(
                    "    {}: {}",
                    header.name,
                    String::from_utf8_lossy(header.value)
                );
            }
            println!("  Body length: {} bytes", request.body.len());
            println!("  Total message length: {} bytes", total_len);
        }
        Err(e) => eprintln!("✗ Error: {:?}", e),
    }
}

fn decode_post_request() {
    let raw = b"POST /api/users HTTP/1.1\r\nHost: example.com\r\nContent-Type: application/json\r\nContent-Length: 24\r\n\r\n{\"name\":\"John\",\"age\":30}";

    let mut headers = [httparse::EMPTY_HEADER; 16];

    match FullRequest::decode(raw, &mut headers) {
        Ok((request, total_len)) => {
            println!("✓ Successfully decoded POST request");
            println!("  Method: {}", request.head.method.unwrap());
            println!("  Path: {}", request.head.path.unwrap());
            println!("  Headers:");
            for header in request.head.headers {
                println!(
                    "    {}: {}",
                    header.name,
                    String::from_utf8_lossy(header.value)
                );
            }
            println!("  Body: {}", String::from_utf8_lossy(request.body));
            println!("  Total length: {} bytes", total_len);
        }
        Err(e) => eprintln!("✗ Error: {:?}", e),
    }
}

fn decode_request_optimized() {
    let raw =
        b"GET /api/data HTTP/1.1\r\nHost: api.example.com\r\nAccept: application/json\r\n\r\n";

    // Use uninitialized headers for maximum performance
    // This avoids the overhead of initializing the headers array
    let mut headers = [const { MaybeUninit::uninit() }; 16];

    match FullRequest::decode_uninit(raw, &mut headers) {
        Ok((request, total_len)) => {
            println!("✓ Successfully decoded with uninit headers (faster!)");
            println!("  Method: {}", request.head.method.unwrap());
            println!("  Path: {}", request.head.path.unwrap());
            println!("  Headers count: {}", request.head.headers.len());
            println!("  Total length: {} bytes", total_len);
            println!();
            println!("  Note: decode_uninit() is faster because it skips header");
            println!("        initialization. Use this for performance-critical code.");
        }
        Err(e) => eprintln!("✗ Error: {:?}", e),
    }
}

fn decode_chunked_request() {
    let raw = b"POST /api/upload HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";

    let mut headers = [httparse::EMPTY_HEADER; 16];

    match FullRequest::decode(raw, &mut headers) {
        Ok((request, total_len)) => {
            println!("✓ Successfully decoded chunked request");
            println!("  Method: {}", request.head.method.unwrap());
            println!("  Transfer-Encoding: chunked");
            println!(
                "  Raw body (with chunk markers): {} bytes",
                request.body.len()
            );
            println!("  Total length (including chunks): {} bytes", total_len);
            println!();
            println!("  Note: request.body contains the raw chunked data including");
            println!("        chunk size markers. Use a chunked decoder to extract");
            println!("        the actual content if needed.");
        }
        Err(e) => eprintln!("✗ Error: {:?}", e),
    }
}

fn decode_responses() {
    // Example 1: 200 OK with body
    let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\n\r\n{\"status\":\"ok\"}";
    let mut headers = [httparse::EMPTY_HEADER; 16];

    match FullResponse::decode(raw, &mut headers) {
        Ok((response, total_len)) => {
            println!("✓ 200 OK Response:");
            println!("  Status: {}", response.head.code.unwrap());
            println!("  Reason: {}", response.head.reason.unwrap());
            println!("  Body: {}", String::from_utf8_lossy(response.body));
            println!("  Total length: {} bytes", total_len);
        }
        Err(e) => eprintln!("✗ Error: {:?}", e),
    }

    println!();

    // Example 2: 404 Not Found
    let raw = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nNot Found";
    let mut headers = [httparse::EMPTY_HEADER; 16];

    match FullResponse::decode(raw, &mut headers) {
        Ok((response, _)) => {
            println!("✓ 404 Response:");
            println!("  Status: {}", response.head.code.unwrap());
            println!("  Reason: {}", response.head.reason.unwrap());
            println!("  Body: {}", String::from_utf8_lossy(response.body));
        }
        Err(e) => eprintln!("✗ Error: {:?}", e),
    }

    println!();

    // Example 3: 204 No Content (no body)
    let raw = b"HTTP/1.1 204 No Content\r\nServer: nginx\r\n\r\n";
    let mut headers = [httparse::EMPTY_HEADER; 16];

    match FullResponse::decode(raw, &mut headers) {
        Ok((response, _)) => {
            println!("✓ 204 No Content Response:");
            println!("  Status: {}", response.head.code.unwrap());
            println!(
                "  Body length: {} bytes (correct for 204)",
                response.body.len()
            );
        }
        Err(e) => eprintln!("✗ Error: {:?}", e),
    }

    println!();

    // Note about decode_uninit for responses
    println!("  Note: FullResponse does NOT support decode_uninit() because");
    println!("        httparse::Response lacks parse_with_uninit_headers method.");
    println!("        Attempting to use it will panic with a clear error message.");
}

fn handle_incomplete_messages() {
    // Example 1: Incomplete headers
    let raw = b"GET /api/test HTTP/1.1\r\nHost: example.com\r\n";
    let mut headers = [httparse::EMPTY_HEADER; 16];

    match FullRequest::decode(raw, &mut headers) {
        Ok(_) => println!("✗ Should have failed!"),
        Err(e) => println!("✓ Correctly detected incomplete headers: {:?}", e),
    }

    // Example 2: Incomplete body
    let raw = b"POST /api/test HTTP/1.1\r\nHost: example.com\r\nContent-Length: 100\r\n\r\nshort";
    let mut headers = [httparse::EMPTY_HEADER; 16];

    match FullRequest::decode(raw, &mut headers) {
        Ok(_) => println!("✗ Should have failed!"),
        Err(e) => println!("✓ Correctly detected incomplete body: {:?}", e),
    }

    println!();
    println!("  These errors allow you to buffer more data and retry parsing");
    println!("  when working with streaming sockets or incremental data.");
}
