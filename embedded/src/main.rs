//! This example uses the RP Pico W board Wifi chip (cyw43).
//! Connects to specified Wifi network and creates a TCP endpoint on port 1234.

#![no_std]
#![no_main]

mod ota;

use core::cell::RefCell;
use core::net::SocketAddr;
use core::num::NonZero;
use core::str::FromStr;
use core::sync::atomic::{AtomicU32, Ordering};

use cyw43::{Control, JoinOptions};
use cyw43_pio::{DEFAULT_CLOCK_DIVIDER, PioSpi};
use defmt::*;
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_net::tcp::client::{TcpClient, TcpClientState};
use embassy_net::{Config, Stack, StackResources};
use embassy_boot_rp::AlignedBuffer;
use embassy_rp::flash::{Blocking, Flash};
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIO0, UART1};
use embassy_rp::pio::Pio;
use embassy_rp::watchdog::Watchdog;
use embassy_rp::{bind_interrupts, dma, pio, uart};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use embedded_nal_async::TcpConnect;
use modbus_mqtt_bridge_core::bridge;
use modbus_mqtt_bridge_core::configuration::{Configuration, NetworkConfiguration};

use modbus_mqtt_bridge_core::configuration::Parity;
use modbus_mqtt_bridge_core::modbus::ModbusRTUChannel;
use modbus_mqtt_bridge_core::ota::{Artifact, MAX_SETTINGS_SIZE, validate_config, validate_registry};
use modbus_mqtt_bridge_core::registry_map::RegistryMap;
use rust_mqtt::buffer::BumpBuffer;
use rust_mqtt::client::Client;
use rust_mqtt::client::options::{ConnectOptions, DisconnectOptions};
use rust_mqtt::types::{MqttBinary, MqttString};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => pio::InterruptHandler<PIO0>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>;
    UART1_IRQ => uart::BufferedInterruptHandler<UART1>;
});

const WIFI_FIRMWARE: &cyw43::Aligned<cyw43::A4, [u8]> =
    cyw43::aligned_bytes!("../assets/firmwares/43439A0.bin");
const WIFI_NVRAM: &cyw43::Aligned<cyw43::A4, [u8]> =
    cyw43::aligned_bytes!("../assets/firmwares/nvram_rp2040.bin");
const WIFI_CLM: &[u8] = include_bytes!("../assets/firmwares/43439A0_clm.bin");

const REGISTRY_MAP: &str = include_str!("../assets/registry_map");
const CONFIGURATION_FILE: &str = include_str!("../assets/configuration.json");

const FLASH_SIZE: usize = 2 * 1024 * 1024;
/// Memory backing the MQTT client, must hold the largest packet we accept (see `ota::MAX_PACKET_SIZE`)
const MQTT_MEMORY_SIZE: usize = 2048;
/// Upper bound for downloading an image; every chunk also feeds the watchdog
const OTA_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// Hardware watchdog period, the RP2040 maximum is ~8.3s
const WATCHDOG_PERIOD: Duration = Duration::from_secs(8);
/// Uptime (in seconds) of the last fully successful polling cycle
static LAST_SUCCESS_SECS: AtomicU32 = AtomicU32::new(0);

/// Feeds the hardware watchdog as long as the main loop keeps completing polling cycles.
/// If no cycle succeeds within `stale_after_secs` the board is reset.
#[embassy_executor::task]
async fn watchdog_task(mut watchdog: Watchdog, stale_after_secs: u32) -> ! {
    // Keep debugging sessions (breakpoints, probe halts) from resetting the board
    watchdog.pause_on_debug(true);
    watchdog.start(WATCHDOG_PERIOD);
    loop {
        let now = Instant::now().as_secs() as u32;
        let last_success = LAST_SUCCESS_SECS.load(Ordering::Relaxed);
        if now.saturating_sub(last_success) < stale_after_secs {
            watchdog.feed(WATCHDOG_PERIOD);
        } else {
            error!("No successful polling cycle for {}s, waiting for watchdog reset", now - last_success);
        }
        Timer::after_secs(2).await;
    }
}

#[embassy_executor::task]
async fn wifi_task(
    runner: cyw43::Runner<'static, cyw43::SpiBus<Output<'static>, PioSpi<'static, PIO0, 0>>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
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
    info!("Booting...");

    let p = embassy_rp::init(Default::default());

    // The bootloader's DFU/state partitions and the settings files live on the same flash chip
    let flash = Mutex::<NoopRawMutex, _>::new(RefCell::new(Flash::<_, Blocking, FLASH_SIZE>::new_blocking(
        p.FLASH,
    )));
    let mut settings = ota::settings_store(&flash);

    // Configuration and registry map: the newest signed files received over the air that work,
    // the ones compiled into the firmware otherwise
    let mut config_buffer = [0u8; MAX_SETTINGS_SIZE];
    let config_bytes = ota::load_settings(
        &mut settings,
        Artifact::Config,
        &mut config_buffer,
        CONFIGURATION_FILE.as_bytes(),
        |data| validate_config(data).is_ok(),
    );
    let mut registry_buffer = [0u8; MAX_SETTINGS_SIZE];
    let registry_text = validate_registry(ota::load_settings(
        &mut settings,
        Artifact::Registry,
        &mut registry_buffer,
        REGISTRY_MAP.as_bytes(),
        |data| validate_registry(data).is_ok(),
    ))
    .unwrap_or(REGISTRY_MAP);

    let try_parse_config = serde_json_core::from_slice::<Configuration>(config_bytes);
    if let Err(err) = try_parse_config {
        error!("Failed to parse configuration: {}", err);
        return;
    }
    let (bridge_config, _) = try_parse_config.unwrap();

    // Started before anything can hang (joining the Wi-Fi with a wrong password never returns):
    // a configuration that can't get a polling cycle through gets the device reset and, on the next
    // boot, replaced by the previous one
    let session_timeout = session_timeout(&bridge_config, registry_text);
    let stale_after_secs = (3 * (session_timeout.as_millis() + bridge_config.polling_interval_ms) / 1000) as u32;
    spawner.spawn(unwrap!(watchdog_task(Watchdog::new(p.WATCHDOG), stale_after_secs)));

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        DEFAULT_CLOCK_DIVIDER,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        dma::Channel::new(p.DMA_CH0, Irqs),
    );

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, WIFI_FIRMWARE, WIFI_NVRAM).await;
    spawner.spawn(unwrap!(wifi_task(runner)));

    control.init(WIFI_CLM).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    let config = Config::dhcpv4(Default::default());

    // Generate random seed
    let seed = 0x0123_4567_89ab_cdef; // chosen by fair dice roll. guarenteed to be random.

    // Init network stack
    static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        net_device,
        config,
        RESOURCES.init(StackResources::new()),
        seed,
    );

    spawner.spawn(unwrap!(net_task(runner)));

    ensure_network(&mut control, stack, &bridge_config.network).await;

    //Init uart connection
    let mut uart_recv_buffer = [0u8; 256];
    let mut uart_write_buffer = [0u8; 256];
    let mut uart_bus = uart::BufferedUart::new(
        p.UART1,
        p.PIN_8,
        p.PIN_9,
        Irqs,
        &mut uart_write_buffer,
        &mut uart_recv_buffer,
        parse_config(&bridge_config),
    );
    let mut rtu_channel = ModbusRTUChannel::new(&mut uart_bus, &bridge_config.serial);

    // And now we can use it!
    let state: TcpClientState<1, 1024, 1024> = TcpClientState::new();
    let mut client = TcpClient::new(stack, &state);
    // Without a timeout a half-open connection (broker restart, NAT/AP drop) blocks reads/writes forever
    client.set_timeout(Some(Duration::from_millis(bridge_config.reconnect_delay_ms.max(10_000))));
    let mqtt_endpoint = SocketAddr::from_str(bridge_config.mqtt.endpoint).unwrap();

    let mut updater_buffer = AlignedBuffer([0u8; 1]);
    let mut slot = ota::firmware_slot(&flash, updater_buffer.as_mut());
    info!("Firmware version {:#08x}", ota::FIRMWARE_VERSION);

    loop {
        ensure_network(&mut control, stack, &bridge_config.network).await;

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

        let mut mqtt_memory = [0u8; MQTT_MEMORY_SIZE];
        let mut mqtt_buffer = BumpBuffer::new(&mut mqtt_memory);
        let mut mqtt_client: ota::MqttClient = Client::new(&mut mqtt_buffer);

        // Bound the polling cycle so a stuck connect/publish cannot block the loop
        let polling = async {
            let connect_options = ConnectOptions::new()
                .clean_start()
                .maximum_packet_size(unwrap!(NonZero::new(ota::MAX_PACKET_SIZE)))
                .user_name(unwrap!(MqttString::try_from(bridge_config.mqtt.username).ok()))
                .password(unwrap!(MqttBinary::try_from(bridge_config.mqtt.password).ok()));

            if let Err(err) = mqtt_client
                .connect(
                    connection,
                    &connect_options,
                    Some(unwrap!(MqttString::try_from(bridge_config.mqtt.device_id).ok())),
                )
                .await
            {
                error!("Failed to connect to MQTT broker: {:?}", err);
                return Err(());
            }
            // The CONNACK contents are no longer referenced, free the buffer for the next packet
            unsafe { mqtt_client.buffer_mut().reset() };

            info!("Connected to MQTT broker");

            let mut result = Ok(());
            let registry_map = RegistryMap::new(registry_text);
            for entry in registry_map {
                control.gpio_set(0, true).await;
                info!("Reading entry topic={} at address={} of type={}", entry.topic, entry.address, entry.reg_value_type);
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
                    result = Err(());
                    break;
                }
                control.gpio_set(0, false).await;

                if let Some(delay) = bridge_config.inter_request_delay_ms {
                    Timer::after(Duration::from_millis(delay)).await;
                }
            }
            control.gpio_set(0, false).await;
            result
        };
        let polling = with_timeout(session_timeout, polling).await;

        match polling {
            Ok(Ok(())) => {
                mark_success();
                // This image just completed a full cycle: stop the bootloader from reverting it
                slot.confirm_boot();
                settings.confirm();

                let update = with_timeout(
                    OTA_TIMEOUT,
                    ota::check_and_update(
                        &mut mqtt_client,
                        &mut slot,
                        &mut settings,
                        bridge_config.mqtt.device_id,
                        mark_success
                    ),
                )
                .await;
                let update_ready = match update {
                    Ok(Ok(ota::Outcome::Ready)) => true,
                    Ok(Ok(ota::Outcome::NoUpdate)) => false,
                    Ok(Err(_)) => {
                        light_led_exception_pattern(&mut control, 4).await;
                        false
                    }
                    Err(_) => {
                        error!("Firmware update timed out after {}s", OTA_TIMEOUT.as_secs());
                        light_led_exception_pattern(&mut control, 4).await;
                        false
                    }
                };

                match with_timeout(Duration::from_secs(5), mqtt_client.disconnect(&DisconnectOptions::new())).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(err)) => error!("Failed to disconnect from MQTT broker: {:?}", err),
                    Err(_) => error!("Timed out disconnecting from MQTT broker"),
                }

                if update_ready {
                    // Let the log flush, the bootloader swaps the verified image in on reset
                    Timer::after_millis(200).await;
                    cortex_m::peripheral::SCB::sys_reset();
                }
                Timer::after(Duration::from_millis(bridge_config.polling_interval_ms)).await;
            }
            Ok(Err(())) => {
                Timer::after(Duration::from_millis(bridge_config.reconnect_delay_ms)).await;
            }
            Err(_) => {
                error!("MQTT session timed out after {}ms", session_timeout.as_millis());
                control.gpio_set(0, false).await;
                light_led_exception_pattern(&mut control, 5).await;
                Timer::after(Duration::from_millis(bridge_config.reconnect_delay_ms)).await;
            }
        }
    }
}

/// Tells the watchdog task the bridge is making progress
fn mark_success() {
    LAST_SUCCESS_SECS.store(Instant::now().as_secs() as u32, Ordering::Relaxed);
}

/// Upper bound for one full polling cycle: every registry entry exhausting its modbus retries,
/// plus a fixed margin for the MQTT connect/disconnect handshakes.
fn session_timeout(config: &Configuration, registry_text: &str) -> Duration {
    let entries = RegistryMap::new(registry_text).count() as u64;
    let attempts = config.serial.retry_count.unwrap_or(1).max(1) as u64;
    let per_entry_ms = attempts * (config.serial.retry_delay_ms.unwrap_or(0) + 1_000)
        + config.inter_request_delay_ms.unwrap_or(0);
    Duration::from_millis(30_000 + entries * per_entry_ms)
}

/// (Re)joins the Wi-Fi network if the link is down and waits for a DHCP configuration.
async fn ensure_network(control: &mut Control<'_>, stack: Stack<'_>, network: &NetworkConfiguration<'_>) {
    if stack.is_link_up() && stack.is_config_up() {
        return;
    }

    if !stack.is_link_up() {
        info!("Wi-Fi link down, joining {}...", network.name);
        while let Err(err) = control
            .join(network.name, JoinOptions::new(network.password.as_bytes()))
            .await
        {
            info!("join failed: {:?}", err);
            Timer::after_secs(1).await;
        }

        info!("waiting for link...");
        stack.wait_link_up().await;
    }

    info!("waiting for DHCP...");
    stack.wait_config_up().await;
}

async fn light_led_exception_pattern(control: &mut Control<'_>, blinks: u8) {
    for _ in 0..blinks {
        control.gpio_set(0, true).await;
        Timer::after(Duration::from_millis(100)).await;
        control.gpio_set(0, false).await;
        Timer::after(Duration::from_millis(100)).await;
    }
}
