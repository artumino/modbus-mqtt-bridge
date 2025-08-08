use heapless::Vec;
use rmodbus::{self, ModbusProto, client::ModbusRequest};

#[cfg(feature = "defmt")]
use defmt::{error, info};
use embedded_io_async::Write;
#[cfg(feature = "log")]
use log::{error, info};

use futures::future::Either;
use crate::async_traits::{Flush, Read, ReadExact};
use crate::modbus::ModbusReadRequestType;
use crate::tasks::select;

use super::{ModbusClient, ModbusDataType, ModbusError, ModbusRTUChannel, ModbusReadRequest};

impl From<&ModbusReadRequest> for ModbusRequest {
    fn from(request: &ModbusReadRequest) -> Self {
        ModbusRequest::new(request.device_id, ModbusProto::Rtu)
    }
}

impl<'a, T> ModbusClient for ModbusRTUChannel<'a, T>
where
    T: Read + Write + Flush + ReadExact,
{
    async fn send_and_read(
        &mut self,
        request: &ModbusReadRequest,
    ) -> Result<ModbusDataType, ModbusError> {
        let mut mreq: ModbusRequest = request.into();
        let count = request.requested_data.count() as u16;
        let mut request_data = Vec::<u8, 256>::new();

        // Ensure empty buffer
        let _ignore_buffer = read_rtu_frame(&mut request_data, self.connection, self.interframe_delay_us, 1)
            .await;
        request_data.clear();

        match &request.request_type {
            ModbusReadRequestType::InputRegister => {
                mreq.generate_get_inputs(request.address, count, &mut request_data)
                    .map_err(ModbusError::CannotBuildRequest)?;
            }
            ModbusReadRequestType::HoldingRegister => {
                mreq.generate_get_holdings(request.address, count, &mut request_data)
                    .map_err(ModbusError::CannotBuildRequest)?;
            }
        }
        info!("Request data: {:?}", request_data);

        self.connection
            .write_all(&request_data)
            .await
            .map_err(|_| ModbusError::ModbusWriteError)?;
        embedded_io_async::Write::flush(self.connection).await.map_err(|_| ModbusError::ModbusWriteError)?;

        let mut response = Vec::<u8, 256>::new();
        read_rtu_frame(&mut response, self.connection, self.interframe_delay_us, self.first_bit_variance)
            .await?;
        

        let result = mreq
            .parse_slice(&response)
            .map_err(ModbusError::FrameIntegrityError)?;

        request
            .requested_data
            .try_parse(result)
            .map_err(|_| ModbusError::CannotParse)
    }
}

async fn read_rtu_frame<T, const MAX_SIZE: usize>(
    buf: &mut Vec<u8, MAX_SIZE>,
    connection: &mut T,
    max_interframe_us: u64,
    first_bit_variance: u8,
) -> Result<(), ModbusError>
where
    T: Read + Write,
{
    use crate::timing::after_duration;
    use core::time::Duration;
    let mut buff = [0u8; 32];
    let mut first = true;

    loop {
        match select(
            connection.read(&mut buff),
            after_duration(Duration::from_micros(max_interframe_us * match first {
                true => first_bit_variance as u64,
                false => 1,
            }))
        )
        .await
        {
            Either::Left(response) => match response {
                Ok(size) => {
                    buf.extend_from_slice(&buff[..size])
                        .map_err(|_| ModbusError::ModbusReadOverflow)?;
                }
                Err(_) => {
                    error!("Got error reading from uart");
                    return Err(ModbusError::ModbusReadError);
                }
            },
            Either::Right(_) => {
                return if !buf.is_empty() {
                    Ok(())
                } else {
                    Err(ModbusError::ModbusReadTimeout)
                };
            }
        };
        first = false;
    }
}
