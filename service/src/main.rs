use std::{net::SocketAddr, str::FromStr, time::Duration};

use log::{error, info};

use anyhow::Result;
use embedded_io_adapters::tokio_1::FromTokio;
use modbus_mqtt_bridge_core::modbus::ModbusRTUChannel;
use modbus_mqtt_bridge_core::{
    bridge,
    configuration::{Configuration, Parity},
    registry_map::RegistryMap,
};
use rust_mqtt::{
    client::{client::MqttClient, client_config::ClientConfig},
    utils::rng_generator::CountingRng,
};
use serde::Deserialize;
use tokio::{fs::File, io::AsyncReadExt};
use tokio_serial::SerialPortBuilder;

#[derive(Deserialize, Debug)]
pub struct PcConfiguration<'a> {
    #[serde(flatten)]
    pub bridge: Configuration<'a>,
    pub device_tty: &'a str,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut configuration_file = File::open("configuration.json").await?;
    let mut configuration_string = String::new();
    AsyncReadExt::read_to_string(&mut configuration_file, &mut configuration_string).await?;

    let mut registry_file = File::open("registry_map").await?;
    let mut registry_string = String::new();
    AsyncReadExt::read_to_string(&mut registry_file, &mut registry_string).await?;

    let configuration: PcConfiguration = serde_json::from_str(&configuration_string)?;
    let bridge_configuration = &configuration.bridge;

    let port_builder = tokio_serial::new(
        configuration.device_tty,
        bridge_configuration.serial.baud_rate,
    )
    .data_bits(match bridge_configuration.serial.data_bits {
        5 => tokio_serial::DataBits::Five,
        6 => tokio_serial::DataBits::Six,
        7 => tokio_serial::DataBits::Seven,
        8 => tokio_serial::DataBits::Eight,
        _ => tokio_serial::DataBits::Eight,
    })
    .parity(match bridge_configuration.serial.parity {
        Parity::Even => tokio_serial::Parity::Even,
        Parity::Odd => tokio_serial::Parity::Odd,
        _ => tokio_serial::Parity::None,
    })
    .stop_bits(match bridge_configuration.serial.stop_bits {
        2 => tokio_serial::StopBits::Two,
        _ => tokio_serial::StopBits::One,
    })
    .timeout(std::time::Duration::from_millis(10));

    loop {
        match run_bridge(
            bridge_configuration,
            &port_builder,
            registry_string.as_str(),
        )
        .await
        {
            Ok(_) => {}
            Err(e) => {
                error!("Error running bridge: {:?}", e);
            }
        }
    }
}

async fn run_bridge(
    bridge_config: &Configuration<'_>,
    port_builder: &SerialPortBuilder,
    registry_map_str: &str,
) -> Result<()> {
    'connection: loop {
        let registry_map = RegistryMap::new(registry_map_str);
        let mut serial_stream = FromTokio::new(tokio_serial::SerialStream::open(port_builder)?);
        let mut rtu_channel = ModbusRTUChannel::new(&mut serial_stream, &bridge_config.serial);
        let mqtt_endpoint = SocketAddr::from_str(bridge_config.mqtt.endpoint)?;
        let tpc_stream = FromTokio::new(tokio::net::TcpStream::connect(mqtt_endpoint).await?);

        //Setup MQTT Config
        let mut config = ClientConfig::new(
            rust_mqtt::client::client_config::MqttVersion::MQTTv5,
            CountingRng(20000),
        );
        config.add_max_subscribe_qos(rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS1);
        config.add_client_id(bridge_config.mqtt.device_id);
        config.add_username(bridge_config.mqtt.username);
        config.add_password(bridge_config.mqtt.password);
        config.max_packet_size = 2048;

        let mut tcp_recv_buffer = [0; 2048];
        let mut tcp_write_buffer = [0; 2048];
        let mut mqtt_client = MqttClient::<_, 30, _>::new(
            tpc_stream,
            &mut tcp_write_buffer,
            2048,
            &mut tcp_recv_buffer,
            2048,
            config,
        );

        if let Err(err) = mqtt_client.connect_to_broker().await {
            error!("Failed to connect to MQTT broker: {:?}", err);
            tokio::time::sleep(Duration::from_secs(bridge_config.reconnect_delay)).await;
            continue 'connection;
        }

        info!("Connected to MQTT broker");

        for entry in registry_map {
            if let Err(err) = bridge::read_and_send_entry(
                &mut mqtt_client,
                &mut rtu_channel,
                &entry,
                bridge_config.mqtt.device_id,
            )
            .await
            {
                error!("Failed to send {:?} message: {}", entry.topic, err);
            }

            if let Some(delay) = bridge_config.inter_request_delay {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
        }

        if let Err(err) = mqtt_client.disconnect().await {
            error!("Failed to disconnect from MQTT broker: {:?}", err);
        }

        tokio::time::sleep(Duration::from_secs(bridge_config.polling_interval)).await;
    }
}
