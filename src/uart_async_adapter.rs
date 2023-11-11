use embassy_rp::uart;
use embedded_io_async::{Error, ErrorType};
use thiserror::Error;

pub struct RpUartAsyncAdapter<'a, T>
where
    T: uart::Instance,
{
    uart_bus: uart::Uart<'a, T, uart::Async>,
}

impl<'a, T> RpUartAsyncAdapter<'a, T>
where
    T: uart::Instance,
{
    pub fn new(uart_bus: uart::Uart<'a, T, uart::Async>) -> Self {
        Self { uart_bus }
    }
}

#[derive(Debug, Error, defmt::Format)]
pub enum RpUartError {
    #[error("Read error {0:?}")]
    ReadError(uart::Error),
    #[error("Write error {0:?}")]
    WriteError(uart::Error),
}

impl Error for RpUartError {
    fn kind(&self) -> embedded_io_async::ErrorKind {
        match self {
            RpUartError::ReadError(err) => err.kind(),
            RpUartError::WriteError(err) => err.kind(),
        }
    }
}

impl<'a, T> ErrorType for RpUartAsyncAdapter<'a, T>
where
    T: uart::Instance,
{
    type Error = RpUartError;
}

impl<'a, T> embedded_io_async::Read for RpUartAsyncAdapter<'a, T>
where
    T: uart::Instance,
{
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, RpUartError> {
        self.uart_bus
            .read(buf)
            .await
            .map_err(RpUartError::ReadError)?;
        Ok(buf.len())
    }
}

impl<'a, T> embedded_io_async::Write for RpUartAsyncAdapter<'a, T>
where
    T: uart::Instance,
{
    async fn write(&mut self, buf: &[u8]) -> Result<usize, RpUartError> {
        self.uart_bus
            .write(buf)
            .await
            .map_err(RpUartError::WriteError)?;
        Ok(buf.len())
    }

    async fn flush(&mut self) -> Result<(), RpUartError> {
        self.uart_bus
            .blocking_flush()
            .map_err(RpUartError::WriteError)
    }
}
