//! Dependency-free micro-benchmarks. Run with: `cargo bench --bench wire`

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::{Empty, Full};
use http_wire::request::FullRequest;
use http_wire::response::FullResponse;
use http_wire::{WireDecode, WireEncode};
use std::hint::black_box;
use std::mem::MaybeUninit;
use std::time::Instant;

fn bench<F: FnMut()>(name: &str, iters: u32, mut f: F) {
    for _ in 0..iters / 10 {
        f();
    }
    let mut best = f64::MAX;
    for _ in 0..7 {
        let t = Instant::now();
        for _ in 0..iters {
            f();
        }
        best = best.min(t.elapsed().as_nanos() as f64 / iters as f64);
    }
    println!("{name:<42} {best:>9.1} ns/iter");
}

fn main() {
    const N: u32 = 300_000;

    // ---- Encoding ----
    bench("encode GET (3 headers, empty body)", N, || {
        let req = Request::builder()
            .method("GET")
            .uri("/api/users?id=42")
            .header("Host", "example.com")
            .header("Accept", "application/json")
            .header(
                "User-Agent",
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/120.0 Safari/537.36",
            )
            .body(Empty::<Bytes>::new())
            .unwrap();
        black_box(req.encode().unwrap());
    });

    let payload = Bytes::from(vec![b'x'; 1024]);
    bench("encode POST (1 KiB body)", N, || {
        let req = Request::builder()
            .method("POST")
            .uri("/api/users")
            .header("Host", "example.com")
            .header("Content-Type", "application/json")
            .header("Content-Length", "1024")
            .body(Full::new(payload.clone()))
            .unwrap();
        black_box(req.encode().unwrap());
    });

    bench("encode response 200 (2 headers)", N, || {
        let res = Response::builder()
            .status(200)
            .header("Content-Type", "application/json")
            .header("Server", "http_wire")
            .body(Full::new(Bytes::from_static(b"{\"status\":\"ok\"}")))
            .unwrap();
        black_box(res.encode().unwrap());
    });

    // Baseline: cost of building the message only (subtract from the above).
    bench("  (baseline: build GET request only)", N, || {
        let req = Request::builder()
            .method("GET")
            .uri("/api/users?id=42")
            .header("Host", "example.com")
            .header("Accept", "application/json")
            .header(
                "User-Agent",
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/120.0 Safari/537.36",
            )
            .body(Empty::<Bytes>::new())
            .unwrap();
        black_box(req);
    });

    // ---- Decoding ----
    let get: &[u8] = b"GET /api/users?id=42 HTTP/1.1\r\nHost: example.com\r\nAccept: application/json\r\nUser-Agent: Mozilla/5.0 (X11; Linux x86_64)\r\nContent-Length: 0\r\n\r\n";
    bench("decode GET", N * 4, || {
        let mut h = [httparse::EMPTY_HEADER; 16];
        black_box(FullRequest::decode(black_box(get), &mut h).unwrap().1);
    });
    bench("decode_uninit GET", N * 4, || {
        let mut h = [const { MaybeUninit::uninit() }; 16];
        black_box(FullRequest::decode_uninit(black_box(get), &mut h).unwrap().1);
    });

    let post: &[u8] = b"POST /api HTTP/1.1\r\nHost: example.com\r\nContent-Length: 11\r\n\r\nhello world";
    bench("decode POST (content-length)", N * 4, || {
        let mut h = [httparse::EMPTY_HEADER; 16];
        black_box(FullRequest::decode(black_box(post), &mut h).unwrap().1);
    });

    let chunked_small: &[u8] = b"POST /u HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
    bench("decode chunked (2 chunks)", N * 4, || {
        let mut h = [httparse::EMPTY_HEADER; 16];
        black_box(FullRequest::decode(black_box(chunked_small), &mut h).unwrap().1);
    });

    let mut many = Vec::new();
    many.extend_from_slice(b"POST /u HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n");
    for _ in 0..64 {
        many.extend_from_slice(b"10\r\n0123456789abcdef\r\n");
    }
    many.extend_from_slice(b"0\r\nX-Trailer: 1\r\n\r\n");
    bench("decode chunked (64 chunks + trailer)", N, || {
        let mut h = [httparse::EMPTY_HEADER; 16];
        black_box(FullRequest::decode(black_box(&many), &mut h).unwrap().1);
    });

    let resp: &[u8] =
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\n\r\n{\"status\":\"ok\"}";
    bench("decode response 200", N * 4, || {
        let mut h = [httparse::EMPTY_HEADER; 16];
        black_box(FullResponse::decode(black_box(resp), &mut h).unwrap().1);
    });
}
