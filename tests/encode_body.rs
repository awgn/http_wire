//! Tests for the body-draining paths of `WireEncode`:
//! multi-frame bodies, trailers, `Pending` (blocking fallback) and body errors.

use bytes::Bytes;
use http::{HeaderMap, Request, Response};
use http_body::{Body, Frame};
use http_wire::{WireEncode, WireError};
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

enum Step {
    Data(&'static [u8]),
    Trailers,
    /// Return `Poll::Pending` once, after scheduling a wake-up.
    Pending,
    Fail,
}

/// A scripted body: each poll consumes one step.
struct ScriptedBody(VecDeque<Step>);

impl ScriptedBody {
    fn new(steps: impl IntoIterator<Item = Step>) -> Self {
        Self(steps.into_iter().collect())
    }
}

impl Body for ScriptedBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.0.pop_front() {
            None => Poll::Ready(None),
            Some(Step::Data(d)) => Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(d))))),
            Some(Step::Trailers) => {
                let mut map = HeaderMap::new();
                map.insert("x-trailer", "1".parse().unwrap());
                Poll::Ready(Some(Ok(Frame::trailers(map))))
            }
            Some(Step::Pending) => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Some(Step::Fail) => Poll::Ready(Some(Err(std::io::Error::other("boom")))),
        }
    }
}

fn request(body: ScriptedBody) -> Request<ScriptedBody> {
    Request::builder()
        .method("POST")
        .uri("/x")
        .header("host", "h")
        .body(body)
        .unwrap()
}

const HEAD: &[u8] = b"POST /x HTTP/1.1\r\nhost: h\r\n\r\n";

#[test]
fn multi_frame_body_is_concatenated() {
    let body = ScriptedBody::new([Step::Data(b"foo"), Step::Data(b"-"), Step::Data(b"bar")]);
    let out = request(body).encode().unwrap();
    assert_eq!(&out[..], [HEAD, b"foo-bar"].concat());
}

#[test]
fn trailer_frames_are_ignored() {
    let body = ScriptedBody::new([Step::Data(b"abc"), Step::Trailers]);
    let out = request(body).encode().unwrap();
    assert_eq!(&out[..], [HEAD, b"abc"].concat());
}

#[test]
fn pending_body_falls_back_to_blocking_executor() {
    // The first poll uses a no-op waker and sees Pending; the fallback must
    // re-poll with a real waker and keep the data already written.
    let body = ScriptedBody::new([
        Step::Data(b"one"),
        Step::Pending,
        Step::Data(b"two"),
        Step::Pending,
        Step::Data(b"three"),
    ]);
    let out = request(body).encode().unwrap();
    assert_eq!(&out[..], [HEAD, b"onetwothree"].concat());
}

#[test]
fn body_error_is_reported_as_collection_error() {
    let body = ScriptedBody::new([Step::Data(b"partial"), Step::Fail]);
    match request(body).encode() {
        Err(WireError::Collection(e)) => assert_eq!(e.to_string(), "boom"),
        other => panic!("unexpected result: {other:?}"),
    }
}

#[test]
fn body_error_after_pending_is_reported() {
    let body = ScriptedBody::new([Step::Pending, Step::Fail]);
    assert!(matches!(
        request(body).encode(),
        Err(WireError::Collection(_))
    ));
}

#[test]
fn response_multi_frame_and_pending() {
    let body = ScriptedBody::new([Step::Data(b"he"), Step::Pending, Step::Data(b"llo")]);
    let res = Response::builder().status(200).body(body).unwrap();
    let out = res.encode().unwrap();
    assert_eq!(&out[..], b"HTTP/1.1 200 OK\r\n\r\nhello");
}
