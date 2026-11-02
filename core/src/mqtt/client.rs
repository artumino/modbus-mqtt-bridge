use rust_mqtt::{
    Bytes,
    buffer::BufferProvider,
    client::{Client, options::{PublicationOptions, TopicReference}},
    io::Transport,
    types::{MqttString, TopicName},
};

#[cfg(feature = "defmt")]
use defmt::error;
#[cfg(feature = "log")]
use log::error;

use super::{MqttError, MqttSender};

impl<
    'a,
    'c,
    N,
    B,
    const SUBSCRIBE_MAXIMUM: usize,
    const RECEIVE_MAXIMUM: usize,
    const SEND_MAXIMUM: usize,
    const MAX_SUBSCRIPTION_IDENTIFIERS: usize,
    const MAX_USER_PROPERTIES: usize,
    const MAX_INCOMING_TOPIC_ALIASES: usize,
    const MAX_OUTGOING_TOPIC_ALIASES: usize,
> MqttSender
    for Client<
        'a,
        'c,
        N,
        B,
        SUBSCRIBE_MAXIMUM,
        RECEIVE_MAXIMUM,
        SEND_MAXIMUM,
        MAX_SUBSCRIPTION_IDENTIFIERS,
        MAX_USER_PROPERTIES,
        MAX_INCOMING_TOPIC_ALIASES,
        MAX_OUTGOING_TOPIC_ALIASES,
    >
where
    N: Transport,
    B: BufferProvider<'c>,
{
    async fn send(&mut self, topic: &str, payload: &[u8]) -> Result<(), MqttError> {
        let topic = MqttString::try_from(topic)
            .ok()
            .and_then(TopicName::new)
            .ok_or(MqttError::InvalidTopic)?;
        let options = PublicationOptions::new(TopicReference::Name(topic));

        if let Err(_err) = self.publish(&options, Bytes::from(payload)).await {
            #[cfg(any(feature = "defmt", feature = "log"))]
            error!("MQTT publish failed: {:?}", _err);
            return Err(MqttError::SendError);
        }
        Ok(())
    }
}
