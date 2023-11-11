use core::marker::PhantomData;

use thiserror::Error;

mod rmodbus_adapter;

pub enum ModBusReadRequest<T>
where
    T: for<'a> From<&'a [u8]> + Sized,
{
    InputRegister {
        devide_id: u8,
        address: u16,
        marker: PhantomData<T>,
    },
    HoldingRegister {
        devide_id: u8,
        address: u16,
        marker: PhantomData<T>,
    },
}

#[derive(Debug, Error, defmt::Format)]
pub enum ModBusError {
    #[error("Connection error")]
    ModbusNotConnected,
    #[error("Read error")]
    CannotRead,
    #[error("Parse error")]
    CannotParse,
}

pub trait ModbusClient {
    async fn send_and_read<T>(&mut self, request: ModBusReadRequest<T>) -> Result<T, ModBusError>
    where
        T: for<'a> From<&'a [u8]> + Sized;
}

pub struct ModbusClientImpl<'a, T>
where
    T: embedded_io_async::Read + embedded_io_async::Write,
{
    connection: &'a mut T,
}