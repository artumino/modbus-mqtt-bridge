use core::fmt::Write;
use heapless::String;
use thiserror::Error;

mod rmodbus_adapter;

pub enum ModbusReadRequestType {
    InputRegister,
    HoldingRegister,
}

#[derive(Debug, Clone, Copy, defmt::Format)]
#[non_exhaustive]
pub enum ModbusDataType {
    F32(f32),
}

impl ModbusDataType {
    pub fn count(&self) -> usize {
        match self {
            ModbusDataType::F32(_) => 2,
        }
    }

    pub fn try_parse(mut self, data: &[u8]) -> Result<Self, ModbusError> {
        match self {
            ModbusDataType::F32(_) => {
                if data.len() != 4 {
                    return Err(ModbusError::CannotParse);
                }
                let mut buf = [0u8; 4];
                buf.copy_from_slice(data);
                let value = f32::from_be_bytes(buf);
                self = ModbusDataType::F32(value);
            }
        }
        Ok(self)
    }

    pub fn dump_string<const N: usize>(&self, out: &mut String<N>) -> Result<(), ModbusError> {
        match self {
            ModbusDataType::F32(value) => {
                write!(out, "{}", value).map_err(|_| ModbusError::CannotConvertToString(N))
            }
        }
    }
}

pub struct ModbusReadRequest {
    device_id: u8,
    address: u16,
    request_type: ModbusReadRequestType,
    requested_data: ModbusDataType,
}

impl ModbusReadRequest {
    pub fn new(
        device_id: u8,
        address: u16,
        request_type: ModbusReadRequestType,
        requested_data: ModbusDataType,
    ) -> Self {
        Self {
            device_id,
            address,
            request_type,
            requested_data,
        }
    }
}

#[derive(Debug, Error, defmt::Format)]
pub enum ModbusError {
    #[error("Cannot write read request on modbus")]
    ModbusWriteError,
    #[error("Cannot build request")]
    CannotBuildRequest,
    #[error("Read error")]
    ModbusReadError,
    #[error("Parse error")]
    CannotParse,
    #[error("Cannot convert to string of length {0}")]
    CannotConvertToString(usize)
}

pub trait ModbusClient {
    async fn send_and_read(
        &mut self,
        request: &ModbusReadRequest,
    ) -> Result<ModbusDataType, ModbusError>;
}

pub struct ModbusRTUChannel<'a, T>
where
    T: embedded_io_async::Read + embedded_io_async::Write,
{
    connection: &'a mut T,
}

impl<'a, T> ModbusRTUChannel<'a, T>
where
    T: embedded_io_async::Read + embedded_io_async::Write,
{
    pub fn new(connection: &'a mut T) -> Self {
        Self { connection }
    }
}
