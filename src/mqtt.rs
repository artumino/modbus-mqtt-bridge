use thiserror::Error;

mod rust_mqtt_adapter;

#[derive(Error, Debug, defmt::Format)]
pub enum MqttError {
    #[error("Send error with reason {0}")]
    SendError(rust_mqtt::packet::v5::reason_codes::ReasonCode),
}

pub trait MqttSender {
    async fn send(&mut self, topic: &str, payload: &[u8]) -> Result<(), MqttError>;
}
