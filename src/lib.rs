use bytes::Bytes;

pub mod request;
pub mod response;
pub mod wire;

pub trait Wire {
    fn to_bytes(&self) -> Bytes;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {}
}
