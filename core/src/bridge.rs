use thiserror::Error;

use crate::{
    modbus::{ModbusClient, ModbusDataType, ModbusError, ModbusReadRequest, ModbusReadRequestType},
    mqtt::{MqttError, MqttSender},
    registry_map::{RegistryEntry, RegistryType, RegistryValueType},
};

#[derive(Debug, Error, defmt::Format)]
#[non_exhaustive]
pub enum ModBusMqttBridgeError {
    #[error("MQTT Error: {0}")]
    MqttError(MqttError),
    #[error("Modbus error with reason {0}")]
    ModbusError(ModbusError),
    #[error("Cannot convert to string of length {0}")]
    CannotConvertToString(usize),
    #[error("Error formatting topic, check that all topics are of max length {0}<{1}")]
    TopicOverflow(usize, usize),
    #[error("Registry parse error")]
    CannotParseRegistry,
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
        .map_err(|_| ModBusMqttBridgeError::TopicOverflow(expected_size, N))?;
    topic
        .push('/')
        .map_err(|_| ModBusMqttBridgeError::TopicOverflow(expected_size, N))?;
    topic
        .push_str(specific_topic)
        .map_err(|_| ModBusMqttBridgeError::TopicOverflow(expected_size, N))?;
    Ok(())
}

impl TryFrom<&RegistryEntry<'_>> for ModbusReadRequest {
    type Error = ModBusMqttBridgeError;

    fn try_from(value: &RegistryEntry<'_>) -> Result<Self, Self::Error> {
        Ok(ModbusReadRequest::new(
            value.device_id,
            value.address,
            match value.reg_type {
                RegistryType::Holding => ModbusReadRequestType::HoldingRegister,
                RegistryType::Input => ModbusReadRequestType::InputRegister,
            },
            match value.reg_value_type {
                RegistryValueType::Float32 => ModbusDataType::F32(0.0),
                _ => return Err(ModBusMqttBridgeError::CannotParseRegistry),
            },
        ))
    }
}

pub async fn read_and_send_entry<T, M>(
    mqtt_sender: &mut T,
    modbus_client: &mut M,
    registry_entry: &RegistryEntry<'_>,
    base_topic: &str,
) -> Result<(), ModBusMqttBridgeError>
where
    T: MqttSender,
    M: ModbusClient,
{
    let mut topic = heapless::String::<128>::new();
    format_topic(&mut topic, base_topic, registry_entry.topic)?;

    let modbus_request = ModbusReadRequest::try_from(registry_entry)?;
    let value = match modbus_client
        .send_and_read(&modbus_request)
        .await
        .map_err(ModBusMqttBridgeError::ModbusError)
    {
        Ok(value) => value,
        Err(err) => {
            defmt::error!("Error reading from modbus: {:?}", err);
            return Ok(());
        }
    };

    let mut payload = heapless::String::<32>::new();
    value
        .dump_string(&mut payload)
        .map_err(|_| ModBusMqttBridgeError::CannotConvertToString(32))?;

    mqtt_sender.send(topic.as_str(), payload.as_bytes()).await?;
    Ok(())
}
