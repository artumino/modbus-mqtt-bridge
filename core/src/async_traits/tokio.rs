use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{Flush, Read, ReadExact, Write};

impl<T> Read for T
where
    T: AsyncReadExt + Unpin,
{
    type Error = std::io::Error;
    #[inline]
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        self.read(buf).await
    }
}

impl<T> ReadExact for T
where
    T: AsyncReadExt + Unpin,
{
    type Error = std::io::Error;
    #[inline]
    async fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), Self::Error> {
        if self.read_exact(buf).await.is_ok() {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "Unexpected EOF",
            ))
        }
    }
}

impl<T> Write for T
where
    T: AsyncWriteExt + Unpin,
{
    type Error = std::io::Error;
    #[inline]
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.write(buf).await
    }
}

impl<T> Flush for T
where
    T: AsyncWriteExt + Unpin,
{
    type Error = std::io::Error;
    #[inline]
    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.flush().await
    }
}
