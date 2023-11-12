use thiserror::Error;

use crate::logging::Format;

#[cfg(feature = "embedded-io-async")]
mod embedded_io;

#[derive(Error, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[non_exhaustive]
pub enum MqttError {
    #[error("Send error with reason {0}")]
    SendError(rust_mqtt::packet::v5::reason_codes::ReasonCode),
}

impl Format for MqttError {}

pub trait MqttSender {
    fn send(&mut self, topic: &str, payload: &[u8]) -> impl futures::future::Future<Output = Result<(), MqttError>>;
}