//use core::iter;
use core::time::Duration;

#[cfg(feature = "defmt")]
use defmt::error;

#[cfg(feature = "log")]
use log::error;

use futures::future::Either;
use heapless::Vec;
//use rmodbus::guess_response_frame_len;
use rmodbus::{self, client::ModbusRequest, ModbusProto};

use crate::async_traits::{Flush, Read, ReadExact, Write};
use crate::modbus::ModbusReadRequestType;
use crate::tasks::select;
use crate::timing;

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
        let mut request_data = heapless::Vec::<u8, 256>::new();
        match &request.request_type {
            ModbusReadRequestType::InputRegister => {
                mreq.generate_get_inputs(request.address, count, &mut request_data)
                    .map_err(|_| ModbusError::CannotBuildRequest)?;
            }
            ModbusReadRequestType::HoldingRegister => {
                mreq.generate_get_holdings(request.address, count, &mut request_data)
                    .map_err(|_| ModbusError::CannotBuildRequest)?;
            }
        }

        self.connection
            .write(&request_data)
            .await
            .map_err(|_| ModbusError::ModbusWriteError)?;
        self.connection
            .flush()
            .await
            .map_err(|_| ModbusError::ModbusWriteError)?;
        //timing::after_duration(Duration::from_micros(self.t_1_char_us)).await;

        let mut response = heapless::Vec::<u8, 256>::new();
        read_rtu_frame(&mut response, self.connection, self.interframe_delay_us).await?;

        //const HEADER_LEN: usize = 8;
        //let mut buf = [0u8; HEADER_LEN];
        //self.connection
        //    .read_exact(&mut buf)
        //    .await
        //    .map_err(|_| ModbusError::ModbusReadError)?;
        //
        //let len = guess_response_frame_len(&buf, mreq.proto)
        //    .map_err(|_| ModbusError::ModbusReadError)? as usize;
        //
        //let mut response = heapless::Vec::<u8, 256>::from_slice(&buf).map_err(|_| ModbusError::ModbusReadError)?;
        //if len > HEADER_LEN {
        //    if len > response.capacity() {
        //        return Err(ModbusError::ModbusReadError);
        //    }
        //    response.extend(iter::repeat(0).take(len - HEADER_LEN));
        //    let slice = &mut response[(HEADER_LEN - 1)..len];
        //    self.connection
        //        .read_exact(slice)
        //        .await
        //        .map_err(|_| ModbusError::ModbusReadError)?;
        //}

        let result = mreq
            .parse_slice(&response)
            .map_err(|_| ModbusError::FrameIntegrityError)?;

        request
            .requested_data
            .try_parse(result)
            .map_err(|_| ModbusError::CannotParse)
    }
}

async fn read_rtu_frame<T, const MAX_SIZE: usize>(
    buf: &mut Vec<u8, MAX_SIZE>,
    connection: &mut T,
    interframe_time: u64,
) -> Result<(), ModbusError>
where
    T: Read + Write,
{
    let mut buff = [0u8; 32];
    loop {
        match select(
            connection.read(&mut buff),
            timing::after_duration(Duration::from_micros(interframe_time)),
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
    }
}
