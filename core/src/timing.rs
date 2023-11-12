use core::time::Duration;

#[cfg(feature = "embassy")]
pub async fn after_duration(duration: Duration) {
    use embassy_time::{Timer, Duration};

    Timer::after(Duration::from_micros(duration.as_micros() as u64)).await;
}