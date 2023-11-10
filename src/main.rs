//! This example uses the RP Pico W board Wifi chip (cyw43).
//! Connects to specified Wifi network and creates a TCP endpoint on port 1234.

#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]
#![allow(incomplete_features)]
mod registry_map;

use core::str::FromStr;

use cyw43_pio::PioSpi;
use defmt::*;
use embassy_executor::Spawner;
use embassy_net::tcp::client::{TcpClient, TcpClientState};
use embassy_net::{Config, Stack, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIN_23, PIN_25, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_time::{Duration, Timer};
use embedded_nal_async::{SocketAddr, TcpConnect};
use heapless::String;
use rust_mqtt::client::client::MqttClient;
use rust_mqtt::client::client_config::ClientConfig;
use rust_mqtt::packet::v5::publish_packet::QualityOfService;
use rust_mqtt::utils::rng_generator::CountingRng;
use static_cell::make_static;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
});

const WIFI_NETWORK: &str = include_str!("../secrets/network_name");
const WIFI_PASSWORD: &str = include_str!("../secrets/network_pass");
const REGISTRY_MAP: &str = include_str!("../assets/maps/registry_map");
const MQTT_ENDPOINT: &str = include_str!("../secrets/mqtt_endpoint");
const MQTT_PASSWORD: &str = include_str!("../secrets/mqtt_password");
const MQTT_USERNAME: &str = include_str!("../secrets/mqtt_user");
const MQTT_DEVICEID: &str = include_str!("../secrets/mqtt_deviceid");
const RECONNECTION_SECONDS: u64 = 5;
const POLLING_INTERVAL: u64 = 5;

#[embassy_executor::task]
async fn wifi_task(
    runner: cyw43::Runner<
        'static,
        Output<'static, PIN_23>,
        PioSpi<'static, PIN_25, PIO0, 0, DMA_CH0>,
    >,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<cyw43::NetDriver<'static>>) -> ! {
    stack.run().await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("Booting phonometer...");

    let p = embassy_rp::init(Default::default());

    let fw = include_bytes!("../assets/firmwares/43439A0.bin");
    let clm = include_bytes!("../assets/firmwares/43439A0_clm.bin");

    // To make flashing faster for development, you may want to flash the firmwares independently
    // at hardcoded addresses, instead of baking them into the program with `include_bytes!`:
    //     probe-rs download 43439A0.bin --format bin --chip RP2040 --base-address 0x10100000
    //     probe-rs download 43439A0_clm.bin --format bin --chip RP2040 --base-address 0x10140000
    //let fw = unsafe { core::slice::from_raw_parts(0x10100000 as *const u8, 224190) };
    //let clm = unsafe { core::slice::from_raw_parts(0x10140000 as *const u8, 4752) };

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        p.DMA_CH0,
    );

    let state = make_static!(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw).await;
    unwrap!(spawner.spawn(wifi_task(runner)));

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    let config = Config::dhcpv4(Default::default());

    // Generate random seed
    let seed = 0x0123_4567_89ab_cdef; // chosen by fair dice roll. guarenteed to be random.

    // Init network stack
    let stack = &*make_static!(Stack::new(
        net_device,
        config,
        make_static!(StackResources::<2>::new()),
        seed
    ));

    unwrap!(spawner.spawn(net_task(stack)));

    loop {
        //control.join_open(WIFI_NETWORK).await;
        match control.join_wpa2(WIFI_NETWORK, WIFI_PASSWORD).await {
            Ok(_) => break,
            Err(err) => {
                info!("join failed with status={}", err.status);
            }
        }
    }

    // And now we can use it!
    let mut led_status = false;

    let state: TcpClientState<1, 1024, 1024> = TcpClientState::new();
    let client = TcpClient::new(stack, &state);
    let mqtt_endpoint = SocketAddr::from_str(MQTT_ENDPOINT).unwrap();

    let mut recv_buffer = [0; 80];
    let mut write_buffer = [0; 80];

    loop {
        control.gpio_set(0, true).await;
        let connection = match client.connect(mqtt_endpoint).await {
            Ok(connection) => {
                info!("Connected to MQTT endpoint");
                connection
            }
            Err(err) => {
                error!("Failed to connect to MQTT endpoint: {:?}", err);
                control.gpio_set(0, false).await;
                Timer::after(Duration::from_secs(RECONNECTION_SECONDS)).await;
                continue;
            }
        };
        control.gpio_set(0, false).await;

        //Setup MQTT Config
        let mut config = ClientConfig::new(
            rust_mqtt::client::client_config::MqttVersion::MQTTv5,
            CountingRng(20000),
        );
        config.add_max_subscribe_qos(rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS1);
        config.add_client_id(MQTT_DEVICEID);
        config.add_username(MQTT_USERNAME);
        config.add_password(MQTT_PASSWORD);
        config.max_packet_size = 100;

        let mut mqtt_client = MqttClient::<_, 5, _>::new(
            connection,
            &mut write_buffer,
            80,
            &mut recv_buffer,
            80,
            config,
        );

        if let Err(err) = mqtt_client.connect_to_broker().await {
            error!("Failed to connect to MQTT broker: {:?}", err);
            Timer::after(Duration::from_secs(RECONNECTION_SECONDS)).await;
            continue;
        }

        info!("Connected to MQTT broker");

        loop {
            let registry_map = registry_map::RegistryMap::new(REGISTRY_MAP);
            for entry in registry_map {
                let mut topic = String::<128>::new();
                topic.push_str(MQTT_DEVICEID).unwrap();
                topic.push('/').unwrap();
                topic.push_str(entry.topic).unwrap();
                if let Err(err) = mqtt_client
                    .send_message(entry.topic, b"test", QualityOfService::QoS0, true)
                    .await
                {
                    error!("Failed to send message: {:?}", err);
                    break;
                }
            }

            led_status = !led_status;
            control.gpio_set(0, led_status).await;
            Timer::after(Duration::from_secs(POLLING_INTERVAL)).await;
        }
    }
}
