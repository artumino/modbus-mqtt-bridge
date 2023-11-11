use thiserror::Error;

use crate::{
    mqtt::{MqttError, MqttSender},
    registry_map::RegistryEntry,
};

#[derive(Debug, Error, defmt::Format)]
#[non_exhaustive]
pub enum ModBusMqttBridgeError {
    #[error("MQTT Error: {0}")]
    MqttError(MqttError),
    //#[error("ModBus error with reason {0}")]
    //ModBusError(u8),
    #[error("Error formatting topic, check that all topics are of max length {0}<{1}")]
    TopicFormatError(usize, usize),
}

impl From<MqttError> for ModBusMqttBridgeError {
    fn from(err: MqttError) -> Self {
        Self::MqttError(err)
    }
}

fn format_topic<const N: usize>(
    topic: &mut heapless::String<N>,
    base_topic: &str,
    specific_topic: &str,
) -> Result<(), ModBusMqttBridgeError> {
    let expected_size = base_topic.len() + specific_topic.len() + 1;
    topic
        .push_str(base_topic)
        .map_err(|_| ModBusMqttBridgeError::TopicFormatError(expected_size, N))?;
    topic
        .push('/')
        .map_err(|_| ModBusMqttBridgeError::TopicFormatError(expected_size, N))?;
    topic
        .push_str(specific_topic)
        .map_err(|_| ModBusMqttBridgeError::TopicFormatError(expected_size, N))?;
    Ok(())
}

pub async fn read_and_send_entry<T>(
    mqtt_sender: &mut T,
    registry_entry: &RegistryEntry<'_>,
    base_topic: &str,
) -> Result<(), ModBusMqttBridgeError>
where
    T: MqttSender,
{
    let mut topic = heapless::String::<128>::new();
    format_topic(&mut topic, base_topic, registry_entry.topic)?;
    mqtt_sender.send(topic.as_str(), b"test").await?;
    Ok(())
}
