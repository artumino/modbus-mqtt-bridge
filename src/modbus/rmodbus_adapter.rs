use core::iter;

use rmodbus::{self, client::ModbusRequest, guess_response_frame_len, ModbusProto};

use crate::modbus::ModbusReadRequestType;

use super::{ModbusClient, ModbusDataType, ModbusError, ModbusRTUChannel, ModbusReadRequest};

impl From<&ModbusReadRequest> for ModbusRequest {
    fn from(request: &ModbusReadRequest) -> Self {
        ModbusRequest::new(request.device_id, ModbusProto::Rtu)
    }
}

impl<'a, T> ModbusClient for ModbusRTUChannel<'a, T>
where
    T: embedded_io_async::Read + embedded_io_async::Write,
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
            .write_all(&request_data)
            .await
            .map_err(|_| ModbusError::ModbusWriteError)?;

        const HEADER_LEN: usize = 8;
        let mut buf = [0u8; HEADER_LEN];
        self.connection
            .read_exact(&mut buf)
            .await
            .map_err(|_| ModbusError::ModbusReadError)?;

        let len = guess_response_frame_len(&buf, mreq.proto)
            .map_err(|_| ModbusError::ModbusReadError)? as usize;

        let mut response =
            heapless::Vec::<u8, 256>::from_slice(&buf).map_err(|_| ModbusError::ModbusReadError)?;
        if len > HEADER_LEN {
            if len > response.capacity() {
                return Err(ModbusError::ModbusReadError);
            }
            response.extend(iter::repeat(0).take(len - HEADER_LEN));
            let slice = &mut response[(HEADER_LEN - 1)..len];
            self.connection
                .read_exact(slice)
                .await
                .map_err(|_| ModbusError::ModbusReadError)?;
        }

        let result = mreq
            .parse_slice(&response)
            .map_err(|_| ModbusError::CannotParse)?;

        request
            .requested_data
            .try_parse(result)
            .map_err(|_| ModbusError::CannotParse)
    }
}
