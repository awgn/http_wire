use bytes::Bytes;

pub mod request;
pub mod response;
pub mod wire;

use core::future::Future;

pub trait Wire {
    type Error;
    type Future: Future<Output = Result<Bytes, Self::Error>>;
    fn to_bytes(&self) -> Self::Future;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {}
}
