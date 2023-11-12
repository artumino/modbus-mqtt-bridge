//! This example uses the RP Pico W board Wifi chip (cyw43).
//! Connects to specified Wifi network and creates a TCP endpoint on port 1234.

#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]
#![feature(error_in_core)]
#![allow(incomplete_features)]
mod uart_async_adapter;

use core::str::FromStr;

use cyw43::Control;
use cyw43_pio::PioSpi;
use defmt::*;
use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_net::tcp::client::{TcpClient, TcpClientState};
use embassy_net::{Config, Stack, StackResources};
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIN_23, PIN_25, PIO0, UART1};
use embassy_rp::pio::Pio;
use embassy_rp::{bind_interrupts, pio, uart};
use embassy_time::{Duration, Timer};
use embedded_nal_async::{SocketAddr, TcpConnect};
use modbus_mqtt_bridge_core::bridge;
use modbus_mqtt_bridge_core::configuration::Configuration;

use modbus_mqtt_bridge_core::configuration::Parity;
use modbus_mqtt_bridge_core::modbus::ModbusRTUChannel;
use modbus_mqtt_bridge_core::registry_map::RegistryMap;
use rust_mqtt::client::client::MqttClient;
use rust_mqtt::client::client_config::ClientConfig;
use rust_mqtt::utils::rng_generator::CountingRng;
use static_cell::make_static;

use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => pio::InterruptHandler<PIO0>;
    UART1_IRQ => uart::BufferedInterruptHandler<UART1>;
});

const WIFI_FIRMWARE: &[u8] = include_bytes!("../assets/firmwares/43439A0.bin");
const WIFI_CLM: &[u8] = include_bytes!("../assets/firmwares/43439A0_clm.bin");

const REGISTRY_MAP: &str = include_str!("../../registry_map");
const CONFIGURATION_FILE: &str = include_str!("../assets/configuration.json");

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

fn parse_config(config: &Configuration) -> uart::Config {
    let mut uart_config = uart::Config::default();
    uart_config.baudrate = config.serial.baud_rate;
    uart_config.parity = match config.serial.parity {
        Parity::Even => uart::Parity::ParityEven,
        Parity::Odd => uart::Parity::ParityOdd,
        _ => uart::Parity::ParityNone,
    };
    uart_config.stop_bits = match config.serial.stop_bits {
        2 => uart::StopBits::STOP2,
        _ => uart::StopBits::STOP1,
    };
    uart_config.data_bits = match config.serial.data_bits {
        7 => uart::DataBits::DataBits7,
        6 => uart::DataBits::DataBits6,
        5 => uart::DataBits::DataBits5,
        _ => uart::DataBits::DataBits8,
    };
    uart_config
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("Booting phonometer...");

    let (bridge_config, _) =
        serde_json_core::from_str::<Configuration>(CONFIGURATION_FILE).unwrap();

    let p = embassy_rp::init(Default::default());

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
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, WIFI_FIRMWARE).await;
    unwrap!(spawner.spawn(wifi_task(runner)));

    control.init(WIFI_CLM).await;
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
        match control
            .join_wpa2(bridge_config.network.name, bridge_config.network.password)
            .await
        {
            Ok(_) => break,
            Err(err) => {
                info!("join failed with status={}", err.status);
            }
        }
    }

    //Init uart connection
    let mut uart_recv_buffer = [0u8; 256];
    let mut uart_write_buffer = [0u8; 256];
    let uart_bus = uart::BufferedUart::new(
        p.UART1,
        Irqs,
        p.PIN_8,
        p.PIN_9,
        &mut uart_write_buffer,
        &mut uart_recv_buffer,
        parse_config(&bridge_config),
    );
    let mut rp_uart_bus = uart_async_adapter::RpUartAsyncAdapter::new(uart_bus);
    let mut rtu_channel = ModbusRTUChannel::new(&mut rp_uart_bus, &bridge_config.serial);

    // And now we can use it!
    let state: TcpClientState<1, 1024, 1024> = TcpClientState::new();
    let client = TcpClient::new(stack, &state);
    let mqtt_endpoint = SocketAddr::from_str(bridge_config.mqtt.endpoint).unwrap();

    let mut tcp_recv_buffer = [0; 256];
    let mut tcp_write_buffer = [0; 256];

    'connection_loop: loop {
        control.gpio_set(0, true).await;
        let connection = match select(
            client.connect(mqtt_endpoint),
            Timer::after(Duration::from_millis(bridge_config.reconnect_delay_ms)),
        )
        .await
        {
            Either::First(response) => match response {
                Ok(connection) => {
                    info!("Connected to MQTT endpoint");
                    connection
                }
                Err(err) => {
                    error!("Failed to connect to MQTT endpoint: {:?}", err);
                    control.gpio_set(0, false).await;
                    Timer::after(Duration::from_millis(bridge_config.reconnect_delay_ms)).await;
                    continue;
                }
            },
            _ => {
                error!("Failed to connect to MQTT endpoint: Timeout");
                control.gpio_set(0, false).await;
                Timer::after(Duration::from_millis(bridge_config.reconnect_delay_ms)).await;
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
        config.add_client_id(bridge_config.mqtt.device_id);
        config.add_username(bridge_config.mqtt.username);
        config.add_password(bridge_config.mqtt.password);
        config.max_packet_size = 256;

        let mut mqtt_client = MqttClient::<_, 30, _>::new(
            connection,
            &mut tcp_write_buffer,
            256,
            &mut tcp_recv_buffer,
            256,
            config,
        );

        if let Err(err) = mqtt_client.connect_to_broker().await {
            error!("Failed to connect to MQTT broker: {:?}", err);
            Timer::after(Duration::from_millis(bridge_config.reconnect_delay_ms)).await;
            continue 'connection_loop;
        }

        info!("Connected to MQTT broker");

        let registry_map = RegistryMap::new(REGISTRY_MAP);
        'read_loop: for entry in registry_map {
            control.gpio_set(0, true).await;
            if let Err(err) = bridge::read_and_send_entry(
                &mut mqtt_client,
                &mut rtu_channel,
                &entry,
                &bridge_config,
                bridge_config.mqtt.device_id,
            )
            .await
            {
                error!("Failed to send {:?} message: {}", entry.topic, err);
                light_led_exception_pattern(&mut control, 3).await;
                break 'read_loop;
            }
            control.gpio_set(0, false).await;

            if let Some(delay) = bridge_config.inter_request_delay_ms {
                Timer::after(Duration::from_millis(delay)).await;
            }
        }

        control.gpio_set(0, false).await;
        if let Err(err) = mqtt_client.disconnect().await {
            error!("Failed to disconnect from MQTT broker: {:?}", err);
        }

        Timer::after(Duration::from_millis(bridge_config.polling_interval_ms)).await;
    }
}

async fn light_led_exception_pattern(control: &mut Control<'_>, blinks: u8) {
    for _ in 0..blinks {
        control.gpio_set(0, true).await;
        Timer::after(Duration::from_millis(100)).await;
        control.gpio_set(0, false).await;
        Timer::after(Duration::from_millis(100)).await;
    }
}
