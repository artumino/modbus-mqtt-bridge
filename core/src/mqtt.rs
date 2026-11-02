use error_set::error_set;

mod client;

error_set! {
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    MqttError := {
        #[display("Invalid MQTT topic")]
        InvalidTopic,
        #[display("Failed to publish message")]
        SendError,
    }
}

pub trait MqttSender {
    fn send(
        &mut self,
        topic: &str,
        payload: &[u8],
    ) -> impl Future<Output = Result<(), MqttError>>;
}
