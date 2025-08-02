use error_set::error_set;

#[cfg(feature = "embedded-io-async")]
mod embedded_io;

error_set! {
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    MqttError = {
        #[display("Send error with reason {reason_code}")]
        SendError {
            reason_code: rust_mqtt::packet::v5::reason_codes::ReasonCode,
        },
    };
}

pub trait MqttSender {
    fn send(
        &mut self,
        topic: &str,
        payload: &[u8],
    ) -> impl Future<Output = Result<(), MqttError>>;
}
