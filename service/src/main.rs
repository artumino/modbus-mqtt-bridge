use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr,
};

use anyhow::Result;
use modbus_mqtt_bridge_core::{
    bridge,
    configuration::{self, Configuration, Parity},
};
use rust_mqtt::client::client::MqttClient;
use serde::Deserialize;
use tokio::{fs::File, io::AsyncReadExt};

#[derive(Deserialize, Debug)]
pub struct PcConfiguration<'a> {
    pub bridge: Configuration<'a>,
    pub device_tty: &'a str,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut configuration_file = File::open("configuration.json").await?;
    let mut configuration_string = String::new();
    AsyncReadExt::read_to_string(&mut configuration_file, &mut configuration_string).await?;
    let configuration: PcConfiguration = serde_json::from_str(&configuration_string)?;
    let bridge_configuration = configuration.bridge;
    let mqtt_endpoint = SocketAddr::from_str(bridge_configuration.mqtt.endpoint)?;
    let tpc_stream = tokio::net::TcpStream::connect(mqtt_endpoint).await?;

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

    let serial_stream = tokio_serial::SerialStream::open(&port_builder)?;

    Ok(())
}
