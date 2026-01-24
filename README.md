# http_wire

A Rust library to serialize and parse HTTP/1.x requests and responses to/from their wire format (raw bytes).

> **Note**: This crate only supports HTTP/1.0 and HTTP/1.1. HTTP/2 is not supported due to its binary framing, HPACK header compression, and multiplexed nature which make single request/response serialization impractical.

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
http_wire = "0.2"
```

## Encoding (Serialization)

The library provides two ways to encode HTTP messages:

1. **Async encoding** with `WireEncode` - requires an async runtime (Tokio)
2. **Sync encoding** with `WireEncodeSync` - works in synchronous code without requiring an async runtime

### Synchronous Encoding (Recommended for non-async code)

Use `WireEncodeSync` to encode HTTP messages in synchronous contexts without needing to set up an async runtime:

```rust
use http_wire::WireEncodeSync;
use http::Request;
use http_body_util::Full;
use bytes::Bytes;

fn main() {
    let request = Request::builder()
        .method("POST")
        .uri("/api/users")
        .header("Host", "example.com")
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(r#"{"name":"John"}"#)))
        .unwrap();

    let bytes = request.encode_sync().unwrap();
    // bytes contains the full HTTP/1.1 request with body
}
```

This works for both requests and responses:

```rust
use http_wire::WireEncodeSync;
use http::Response;
use http_body_util::Full;
use bytes::Bytes;

fn main() {
    let response = Response::builder()
        .status(200)
        .header("Content-Type", "text/plain")
        .body(Full::new(Bytes::from("Hello World")))
        .unwrap();

    let bytes = response.encode_sync().unwrap();
}
```

**Note:** `WireEncodeSync` creates a minimal single-threaded Tokio runtime internally and blocks until encoding completes. This provides the convenience of synchronous code while still leveraging Hyper's correct HTTP serialization.

### Async Encoding Requests

```rust
use http_wire::WireEncode;
use http::Request;
use http_body_util::Empty;
use bytes::Bytes;

#[tokio::main]
async fn main() {
    let request = Request::builder()
        .method("GET")
        .uri("/api/users")
        .header("Host", "example.com")
        .body(Empty::<Bytes>::new())
        .unwrap();

    let bytes = request.encode().await.unwrap();
    // bytes contains: "GET /api/users HTTP/1.1\r\nhost: example.com\r\n\r\n"
}
```

### Encoding Requests with Body

```rust
use http_wire::WireEncode;
use http::Request;
use http_body_util::Full;
use bytes::Bytes;

#[tokio::main]
async fn main() {
    let body = r#"{"name":"John"}"#;
    let request = Request::builder()
        .method("POST")
        .uri("/api/users")
        .header("Host", "example.com")
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap();

    let bytes = request.encode().await.unwrap();
    // bytes contains the full HTTP/1.1 request with body
}
```

### Encoding Responses

```rust
use http_wire::WireEncode;
use http::Response;
use http_body_util::Full;
use bytes::Bytes;

#[tokio::main]
async fn main() {
    let response = Response::builder()
        .status(200)
        .header("Content-Type", "text/plain")
        .body(Full::new(Bytes::from("Hello World")))
        .unwrap();

    let bytes = response.encode().await.unwrap();
    // bytes contains: "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n..."
}
```

## Decoding (Parsing)

Use the `WireDecode` trait to parse raw HTTP bytes and determine message boundaries.

### Parsing Request Length

Use `RequestLength` to determine the total length of an HTTP request, including its body, in a byte buffer:

```rust
use http_wire::WireDecode;
use http_wire::request::RequestLength;

fn main() {
    let raw = b"GET /api/users HTTP/1.1\r\nHost: example.com\r\n\r\n";
    
    if let Some(length) = RequestLength::decode(raw) {
        println!("Request is {} bytes", length);
        // Use the length to slice the buffer if there's more data
        let request_bytes = &raw[..length];
    } else {
        println!("Incomplete request");
    }
}
```

### Parsing Response Status and Length

Use `ResponseStatusCode` to get both the HTTP status code and total length of a response:

```rust
use http_wire::WireDecode;
use http_wire::response::ResponseStatusCode;
use http::StatusCode;

fn main() {
    let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
    
    if let Some((status, length)) = ResponseStatusCode::decode(raw) {
        println!("Status: {}, Length: {} bytes", status, length);
        assert_eq!(status, StatusCode::OK);
        // Use the length to extract just the response
        let response_bytes = &raw[..length];
    } else {
        println!("Incomplete response");
    }
}
```

### Handling Chunked Transfer Encoding

Both decoders fully support chunked transfer encoding:

```rust
use http_wire::WireDecode;
use http_wire::response::ResponseStatusCode;

fn main() {
    let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
    
    if let Some((status, length)) = ResponseStatusCode::decode(raw) {
        println!("Chunked response complete: {} bytes", length);
    }
}
```

### Stream Parsing Example

Use decoders to handle streaming data:

```rust
use http_wire::WireDecode;
use http_wire::request::RequestLength;

fn parse_stream(buffer: &[u8]) -> Option<(&[u8], &[u8])> {
    // Try to parse a complete request
    if let Some(length) = RequestLength::decode(buffer) {
        // Split buffer into complete request and remaining data
        let (request, remaining) = buffer.split_at(length);
        Some((request, remaining))
    } else {
        // Need more data
        None
    }
}
```

## Error Handling

Both sync and async encoding use the same error types:

```rust
// Async version
use http_wire::{WireEncode, WireError};

#[tokio::main]
async fn main() -> Result<(), WireError> {
    let request = http::Request::builder()
        .uri("/")
        .body(http_body_util::Empty::<bytes::Bytes>::new())
        .unwrap();

    let bytes = request.encode().await?;
    println!("Serialized {} bytes", bytes.len());
    Ok(())
}
```

```rust
// Sync version
use http_wire::{WireEncodeSync, WireError};

fn main() -> Result<(), WireError> {
    let request = http::Request::builder()
        .uri("/")
        .body(http_body_util::Empty::<bytes::Bytes>::new())
        .unwrap();

    let bytes = request.encode_sync()?;
    println!("Serialized {} bytes", bytes.len());
    Ok(())
}
```

`WireError` has three variants:
- `Connection` - HTTP connection error (handshake or send failed)
- `Sync` - Internal synchronization error
- `UnsupportedVersion` - HTTP version not supported (only HTTP/1.0 and HTTP/1.1 are supported)

## Features

- Full support for chunked transfer encoding
- Handles requests and responses with or without bodies
- Case-insensitive header parsing
- Zero-copy parsing for determining message boundaries
- HTTP/1.0 and HTTP/1.1 support

## License

MIT OR Apache-2.0
