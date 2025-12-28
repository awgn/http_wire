# http_wire

A Rust library to serialize HTTP/1.1 requests and responses to their wire format (raw bytes).

> **Note**: This crate only supports HTTP/1.1. HTTP/2 is not supported due to its binary framing, HPACK header compression, and multiplexed nature which make single request/response serialization impractical.

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
http_wire = "0.1"
```

### Using the `ToWire` trait

```rust
use http_wire::ToWire;
use http::Request;
use http_body_util::Empty;
use bytes::Bytes;

async fn example() {
    let request = Request::builder()
        .method("GET")
        .uri("/api/users")
        .header("Host", "example.com")
        .body(Empty::<Bytes>::new())
        .unwrap();

    let bytes = request.to_bytes().await.unwrap();
    // bytes contains: "GET /api/users HTTP/1.1\r\nhost: example.com\r\n\r\n"
}
```

### Serializing responses

```rust
use http_wire::ToWire;
use http::Response;
use http_body_util::Full;
use bytes::Bytes;

async fn example() {
    let response = Response::builder()
        .status(200)
        .header("Content-Type", "text/plain")
        .body(Full::new(Bytes::from("Hello World")))
        .unwrap();

    let bytes = response.to_bytes().await.unwrap();
    // bytes contains the full HTTP/1.1 response
}
```

### Error handling

```rust
use http_wire::{ToWire, WireError};

async fn example() -> Result<(), WireError> {
    let request = http::Request::builder()
        .uri("/")
        .body(http_body_util::Empty::<bytes::Bytes>::new())
        .unwrap();

    let bytes = request.to_bytes().await?;
    println!("Serialized {} bytes", bytes.len());
    Ok(())
}
```

`WireError` has three variants:
- `Connection` - HTTP connection error (handshake or send failed)
- `Sync` - Internal synchronization error
- `UnsupportedVersion` - HTTP version not supported (only HTTP/1.0 and HTTP/1.1 are supported)

## License

MIT