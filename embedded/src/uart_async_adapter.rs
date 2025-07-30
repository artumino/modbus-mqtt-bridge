use embassy_rp::uart;
use embedded_io_async::{Error, ErrorType};
use error_set::error_set;

pub struct RpUartAsyncAdapter {
    uart_bus: uart::BufferedUart,
}

impl RpUartAsyncAdapter {
    pub fn new(uart_bus: uart::BufferedUart) -> Self {
        Self { uart_bus }
    }
}

error_set! {
    #[derive(defmt::Format)]
    RpUartError = {
        #[display("Read error {kind:?}")]
        ReadError {
                kind: uart::Error,
            },
        #[display("Write error {kind:?}")]
        WriteError {
                kind: uart::Error,
            },
    };
}

impl Error for RpUartError {
    fn kind(&self) -> embedded_io_async::ErrorKind {
        match self {
            RpUartError::ReadError { kind } => kind.kind(),
            RpUartError::WriteError { kind } => kind.kind(),
        }
    }
}

impl ErrorType for RpUartAsyncAdapter {
    type Error = RpUartError;
}

impl embedded_io_async::Read for RpUartAsyncAdapter {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, RpUartError> {
        self.uart_bus
            .read(buf)
            .await
            .map_err(|err| RpUartError::ReadError { kind: err })
    }
}

impl embedded_io_async::Write for RpUartAsyncAdapter {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, RpUartError> {
        self.uart_bus
            .write(buf)
            .await
            .map_err(|err| RpUartError::WriteError { kind: err })
    }

    async fn flush(&mut self) -> Result<(), RpUartError> {
        self.uart_bus
            .flush()
            .await
            .map_err(|err| RpUartError::WriteError { kind: err })
    }
}
