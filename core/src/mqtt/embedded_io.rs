use rand_core::RngCore;
use rust_mqtt::{client::client::MqttClient, packet::v5::publish_packet::QualityOfService};

use crate::async_traits::{Read, Write};

use super::{MqttError, MqttSender};


impl<T, const N: usize, R> MqttSender for MqttClient<'_, T, N, R>
where
    T: Read + Write,
    R: RngCore,
    T: embedded_io_async::Read + embedded_io_async::Write,
{
    async fn send(&mut self, topic: &str, payload: &[u8]) -> Result<(), MqttError> {
        if let Err(err) = self
            .send_message(topic, payload, QualityOfService::QoS1, true)
            .await
        {
            return Err(MqttError::SendError(err));
        }
        Ok(())
    }
}
