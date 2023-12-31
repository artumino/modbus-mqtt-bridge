pub trait Read {
    type Error;
    fn read(
        &mut self,
        buf: &mut [u8],
    ) -> impl futures::future::Future<Output = Result<usize, Self::Error>>;
}

pub trait ReadExact {
    type Error;
    fn read_exact(
        &mut self,
        buf: &mut [u8],
    ) -> impl futures::future::Future<Output = Result<(), Self::Error>>;
}

pub trait Write {
    type Error;
    fn write(
        &mut self,
        buf: &[u8],
    ) -> impl futures::future::Future<Output = Result<usize, Self::Error>>;
}

pub trait Flush {
    type Error;
    fn flush(&mut self) -> impl futures::future::Future<Output = Result<(), Self::Error>>;
}

#[cfg(feature = "embedded-io-async")]
mod embedded_io;

#[cfg(all(feature = "tokio", not(feature = "embedded-io-async")))]
mod tokio;
