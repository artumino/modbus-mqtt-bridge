use core::time::Duration;

#[cfg(all(feature = "embassy", feature = "tokio"))]
compile_error!("Only one of the features 'embassy' and 'tokio' can be enabled at the same time.");

#[cfg(feature = "embassy")]
pub async fn after_duration(duration: Duration) {
    use embassy_time::{Duration, Timer};

    Timer::after(Duration::from_micros(duration.as_micros() as u64)).await;
}

#[cfg(feature = "tokio")]
pub async fn after_duration(duration: Duration) {
    tokio::time::sleep(duration).await;
}
