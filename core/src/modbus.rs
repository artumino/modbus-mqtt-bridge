use core::fmt::Write;
use heapless::String;
use thiserror::Error;

use crate::{configuration::{Parity, SerialConfiguration}, logging::Format};

#[cfg(feature = "embedded-io-async")]
mod embedded_io;

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

impl Format for ModbusDataType {}

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

#[derive(Debug, Error)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum ModbusError {
    #[error("Cannot write read request on modbus")]
    ModbusWriteError,
    #[error("Cannot build request")]
    CannotBuildRequest,
    #[error("Read error")]
    ModbusReadError,
    #[error("Read overflow")]
    ModbusReadOverflow,
    #[error("Read timedout")]
    ModbusReadTimeout,
    #[error("Parse error")]
    CannotParse,
    #[error("Underlying frame integrity error")]
    FrameIntegrityError,
    #[error("Cannot convert to string of length {0}")]
    CannotConvertToString(usize),
}

impl Format for ModbusError {}

pub trait ModbusClient {
    fn send_and_read(
        &mut self,
        request: &ModbusReadRequest,
    ) -> impl futures::future::Future<Output = Result<ModbusDataType, ModbusError>>;
}

pub struct ModbusRTUChannel<'a, T>
where
    T: embedded_io_async::Read + embedded_io_async::Write,
{
    connection: &'a mut T,
    t_1_char_us: u64,        // Time to send one character in us
    interframe_delay_us: u64, // Maximum time between frames in us
}

impl<'a, T> ModbusRTUChannel<'a, T>
where
    T: embedded_io_async::Read + embedded_io_async::Write,
{
    pub fn new(connection: &'a mut T, config: &SerialConfiguration) -> Self {
        let t_1_char = ((1_000_000
            * (config.data_bits
                + config.stop_bits
                + match config.parity {
                    Parity::None => 0,
                    _ => 1,
                }
                + 2) as u64)
            / (config.baud_rate as u64))
            + 1;

        let interframe_delay = match config.baud_rate {
            ..=19200 => (3_500 * t_1_char) / 1_000,
            _ => 1750,
        };

        Self {
            connection,
            t_1_char_us: t_1_char,
            interframe_delay_us: interframe_delay,
        }
    }
}
