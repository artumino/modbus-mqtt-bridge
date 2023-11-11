use embassy_futures::select::{select, Either};
use embassy_rp::uart;
use embassy_time::{Duration, Timer};
use embedded_io_async::{Error, ErrorType};
use thiserror::Error;

pub struct RpUartAsyncAdapter<'a, T>
where
    T: uart::Instance,
{
    uart_bus: uart::BufferedUart<'a, T>,
}

impl<'a, T> RpUartAsyncAdapter<'a, T>
where
    T: uart::Instance,
{
    pub fn new(uart_bus: uart::BufferedUart<'a, T>) -> Self {
        Self { uart_bus }
    }
}

#[derive(Debug, Error, defmt::Format)]
pub enum RpUartError {
    #[error("Read error {0:?}")]
    ReadError(uart::Error),
    #[error("Read timeout")]
    ReadTimeout,
    #[error("Write error {0:?}")]
    WriteError(uart::Error),
}

impl Error for RpUartError {
    fn kind(&self) -> embedded_io_async::ErrorKind {
        match self {
            RpUartError::ReadError(err) => err.kind(),
            RpUartError::ReadTimeout => embedded_io_async::ErrorKind::TimedOut,
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
        match select(
            self.uart_bus.read(buf),
            Timer::after(Duration::from_secs(2)),
        )
        .await
        {
            Either::First(result) => result.map_err(RpUartError::ReadError),
            Either::Second(_) => Err(RpUartError::ReadTimeout),
        }
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
            .map_err(RpUartError::WriteError)
    }

    async fn flush(&mut self) -> Result<(), RpUartError> {
        self.uart_bus
            .flush()
            .await
            .map_err(RpUartError::WriteError)
    }
}
