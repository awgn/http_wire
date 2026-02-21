# http_wire

A Rust library to serialize and parse HTTP/1.x requests and responses to/from their wire format (raw bytes).

> **Note**: This crate only supports HTTP/1.0 and HTTP/1.1. HTTP/2 is not supported due to its binary framing, HPACK header compression, and multiplexed nature.

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
http_wire = "0.7"
```

## Encoding

Use the [`WireEncode`] trait to serialize `http::Request` and `http::Response` values to their wire-format bytes.

Encoding is performed via **direct serialization** — no async runtime, no Tokio, no HTTP pipeline. The request/response line, headers, and body are written sequentially into a pre-allocated buffer. The body is collected using `futures::executor::block_on`, which requires no runtime for in-memory body types such as `Full<Bytes>` and `Empty<Bytes>`.

### Encoding a request

```rust
use http_wire::WireEncode;
use http::Request;
use http_body_util::Empty;
use bytes::Bytes;

let request = Request::builder()
    .method("GET")
    .uri("/api/users")
    .header("Host", "example.com")
    .header("Accept", "application/json")
    .body(Empty::<Bytes>::new())
    .unwrap();

let bytes = request.encode().unwrap();
// b"GET /api/users HTTP/1.1\r\nhost: example.com\r\naccept: application/json\r\n\r\n"
```

### Encoding a request with body

```rust
use http_wire::WireEncode;
use http::Request;
use http_body_util::Full;
use bytes::Bytes;

let body = r#"{"name":"Alice"}"#;
let request = Request::builder()
    .method("POST")
    .uri("/api/users")
    .header("Host", "example.com")
    .header("Content-Type", "application/json")
    .header("Content-Length", body.len().to_string())
    .body(Full::new(Bytes::from(body)))
    .unwrap();

let bytes = request.encode().unwrap();
```

### Encoding a response

```rust
use http_wire::WireEncode;
use http::Response;
use http_body_util::Full;
use bytes::Bytes;

let response = Response::builder()
    .status(200)
    .header("Content-Type", "application/json")
    .body(Full::new(Bytes::from(r#"{"status":"ok"}"#)))
    .unwrap();

let bytes = response.encode().unwrap();
// b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"status\":\"ok\"}"
```

## Decoding

Use the [`WireDecode`] trait with [`FullRequest`] or [`FullResponse`] to parse raw bytes and determine complete message boundaries.

### Decoding a request

```rust
use http_wire::WireDecode;
use http_wire::request::FullRequest;

let raw = b"GET /api/users HTTP/1.1\r\nHost: example.com\r\n\r\n";
let mut headers = [httparse::EMPTY_HEADER; 16];
let (request, total_len) = FullRequest::decode(raw, &mut headers).unwrap();

assert_eq!(request.head.method, Some("GET"));
assert_eq!(request.head.path, Some("/api/users"));
assert_eq!(total_len, raw.len());
```

### Decoding a request (optimised — uninitialized headers)

For performance-critical code, `FullRequest` supports skipping the header buffer initialisation:

```rust
use http_wire::WireDecode;
use http_wire::request::FullRequest;
use std::mem::MaybeUninit;

let raw = b"GET /api/users HTTP/1.1\r\nHost: example.com\r\n\r\n";
let mut headers = [const { MaybeUninit::uninit() }; 16];
let (request, total_len) = FullRequest::decode_uninit(raw, &mut headers).unwrap();

assert_eq!(request.head.method, Some("GET"));
```

> `decode_uninit` is only available for `FullRequest`. `FullResponse` does not support it
> because the underlying `httparse::Response` lacks `parse_with_uninit_headers`.

### Decoding a response

```rust
use http_wire::WireDecode;
use http_wire::response::FullResponse;

let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
let mut headers = [httparse::EMPTY_HEADER; 16];
let (response, total_len) = FullResponse::decode(raw, &mut headers).unwrap();

assert_eq!(response.head.code, Some(200));
assert_eq!(response.body, b"hello");
assert_eq!(total_len, raw.len());
```

### Handling incomplete messages

Both decoders return structured errors rather than panicking on partial data:

```rust
use http_wire::{WireDecode, WireError};
use http_wire::request::FullRequest;

// Incomplete headers
let raw = b"GET /api/users HTTP/1.1\r\nHost: example.com\r\n";
let mut headers = [httparse::EMPTY_HEADER; 16];
assert!(matches!(
    FullRequest::decode(raw, &mut headers),
    Err(WireError::PartialHead)
));

// Headers complete but body truncated
let raw = b"POST / HTTP/1.1\r\nContent-Length: 100\r\n\r\nshort";
let mut headers = [httparse::EMPTY_HEADER; 16];
assert!(matches!(
    FullRequest::decode(raw, &mut headers),
    Err(WireError::IncompleteBody(_))
));
```

### Stream parsing

Decoders return the exact byte length of the complete message, making it straightforward to split a streaming buffer:

```rust
use http_wire::WireDecode;
use http_wire::request::FullRequest;

fn split_first_request<'h, 'b>(
    buf: &'b [u8],
    headers: &'h mut [httparse::Header<'b>],
) -> Option<(&'b [u8], &'b [u8])> {
    let (_, len) = FullRequest::decode(buf, headers).ok()?;
    Some(buf.split_at(len))
}
```

## Transfer encodings

Both `Content-Length` and `Transfer-Encoding: chunked` are fully supported, including:

- Multiple chunks
- Chunk extensions (ignored)
- Trailer headers
- Case-insensitive `chunked` detection
- Multi-value `Transfer-Encoding` headers (e.g. `gzip, chunked`)

Status codes that never carry a body (`1xx`, `204`, `304`) are handled automatically by `FullResponse`.

## Error handling

```rust
use http_wire::{WireEncode, WireError};

fn serialize() -> Result<(), WireError> {
    let request = http::Request::builder()
        .uri("/")
        .body(http_body_util::Empty::<bytes::Bytes>::new())
        .unwrap();

    let bytes = request.encode()?;
    println!("Serialized {} bytes", bytes.len());
    Ok(())
}
```

`WireError` variants:

| Variant | When |
|---|---|
| `Connection` | Body collection failed during encoding |
| `UnsupportedVersion` | HTTP version is not 1.0 or 1.1 |
| `PartialHead` | Headers section is incomplete |
| `IncompleteBody(n)` | Body is `n` bytes shorter than `Content-Length` |
| `InvalidChunkedBody` | Chunked encoding is malformed or incomplete |
| `HttparseError` | Header parsing failed (invalid characters, etc.) |

## Features

- Direct serialization — no Tokio runtime required for encoding
- Zero-copy decoding — parsed fields borrow directly from the input buffer
- Full `Transfer-Encoding: chunked` support (encode and decode)
- Case-insensitive header parsing
- HTTP/1.0 and HTTP/1.1 support
- Uninitialized header buffer optimisation for request decoding

## License

MIT OR Apache-2.0
