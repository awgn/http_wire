# http_wire

A Rust library to serialize HTTP/1.1 requests and responses to their wire format (raw bytes).

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

## License

MIT