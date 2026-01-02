use bytes::Bytes;
use std::future::Future;

mod error;
pub mod request;
pub mod response;
mod util;
mod wire;

pub use error::WireError;

pub trait WireEncode {
    fn encode(self) -> impl Future<Output = Result<Bytes, WireError>> + Send;
}

pub trait WireDecode: Sized {
    type Output;
    fn decode(bytes: &[u8]) -> Option<Self::Output>;
}
