use super::{Read, ReadExact, Write};

impl<T> Read for T
where T: embedded_io_async::Read
{
    type Error = T::Error;
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        self.read(buf).await
    }
}

impl<T> ReadExact for T
where T: embedded_io_async::Read
{
    type Error = embedded_io_async::ReadExactError<T::Error>;
    async fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), Self::Error> {
        self.read_exact(buf).await
    }
}

impl<T> Write for T
where T: embedded_io_async::Write
{
    type Error = T::Error;
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.write(buf).await
    }
}