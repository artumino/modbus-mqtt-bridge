use heapless::Vec;
use rmodbus::{self, ModbusProto, client::ModbusRequest, guess_response_frame_len};

use crate::async_traits::{Flush, Read, ReadExact, Write};
use crate::modbus::ModbusReadRequestType;

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


        let mut response = Vec::<u8, 256>::new();
        let mut buf = [0u8; 6];
        self.connection.read_exact(&mut buf).await.map_err(|_| ModbusError::ModbusReadError)?;
        response.extend_from_slice(&buf).map_err(|_| ModbusError::ModbusReadOverflow)?;
        let len = guess_response_frame_len(&buf, ModbusProto::Rtu).map_err(|_| ModbusError::FrameIntegrityError)?;
        if len > 6 {
            self.connection.read_exact(&mut response[6..])
                .await
                .map_err(|_| ModbusError::ModbusReadError)?;
        }

        let result = mreq
            .parse_slice(&response)
            .map_err(|_| ModbusError::FrameIntegrityError)?;

        request
            .requested_data
            .try_parse(result)
            .map_err(|_| ModbusError::CannotParse)
    }
}
