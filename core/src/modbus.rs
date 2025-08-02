use error_set::error_set;
use heapless::String;
use rmodbus::ErrorKind;
use crate::{
    async_traits::{Read, Write},
    configuration::{Parity, SerialConfiguration},
};

mod rtu;

pub enum ModbusReadRequestType {
    InputRegister,
    HoldingRegister,
}

#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
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
                buf.copy_from_slice(&data[0..4]);
                let value = f32::from_be_bytes(buf);
                self = ModbusDataType::F32(value);
            }
        }
        Ok(self)
    }

    pub fn dump_string<const N: usize>(&self, out: &mut String<N>) -> Result<(), ModbusError> {
        match self {
            ModbusDataType::F32(value) => {
                use core::fmt::Write;
                write!(out, "{value}")
                    .map_err(|_| ModbusError::CannotConvertToString { string_length: N })
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

error_set! {
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    ModbusError = {
       #[display("Cannot write read request on modbus")]
       ModbusWriteError,
       #[display("Cannot build request: {0}")]
       CannotBuildRequest(ErrorKind),
       #[display("Read error")]
       ModbusReadError,
       #[display("Read overflow")]
       ModbusReadOverflow,
       #[display("Read timeout")]
       ModbusReadTimeout,
       #[display("Parse error")]
       CannotParse,
       #[display("Underlying frame integrity error of kind: {0}")]
       FrameIntegrityError(ErrorKind),
       #[display("Error in header integrity check: {0}")]
       HeaderIntegrityError(ErrorKind),
       #[display("Cannot convert to string of length {string_length}")]
       CannotConvertToString {
           string_length: usize,
       },
    };
}

pub trait ModbusClient {
    fn send_and_read(
        &mut self,
        request: &ModbusReadRequest,
    ) -> impl Future<Output = Result<ModbusDataType, ModbusError>>;
}

pub struct ModbusRTUChannel<'a, T>
where
    T: Read + Write,
{
    connection: &'a mut T,
    interframe_delay_us: u64, // Maximum time between frames in us
}

impl<'a, T> ModbusRTUChannel<'a, T>
where
    T: Read + Write,
{
    pub fn new(connection: &'a mut T, config: &SerialConfiguration) -> Self {
        let bits = config.data_bits as u64 + config.stop_bits as u64 + match config.parity {
            Parity::None => 0,
            _ => 1,
        } as u64;

        Self {
            connection,
            interframe_delay_us: match config.baud_rate > 19200 {
                true => 1750,
                _ => (3_500_000 * bits) / config.baud_rate as u64,
            },
        }
    }
}
