use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct MqttConfiguration<'a> {
    #[serde(borrow)]
    pub endpoint: &'a str,
    #[serde(borrow)]
    pub username: &'a str,
    #[serde(borrow)]
    pub password: &'a str,
    #[serde(borrow)]
    pub device_id: &'a str,
}

#[derive(Deserialize, Debug)]
pub struct NetworkConfiguration<'a> {
    #[serde(borrow)]
    pub name: &'a str,
    #[serde(borrow)]
    pub password: &'a str,
}

#[derive(Deserialize, Debug)]
pub enum Parity {
    #[serde(rename = "N")]
    None,
    #[serde(rename = "O")]
    Odd,
    #[serde(rename = "E")]
    Even,
}

#[derive(Deserialize, Debug)]
pub struct SerialConfiguration {
    pub baud_rate: u32,
    pub data_bits: u8,
    pub parity: Parity,
    pub stop_bits: u8,
}

#[derive(Deserialize, Debug)]
pub struct Configuration<'a> {
    #[serde(borrow)]
    pub mqtt: MqttConfiguration<'a>,
    #[serde(borrow)]
    pub network: NetworkConfiguration<'a>,
    pub serial: SerialConfiguration,
    pub polling_interval: u64,
    pub reconnect_delay: u64,
    pub inter_request_delay: Option<u64>,
}
