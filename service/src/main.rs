use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let serial = ::new("/dev/ttyUSB0", 9600)
        .timeout(std::time::Duration::from_millis(10))
        .open_native()?;

    Ok(())
}