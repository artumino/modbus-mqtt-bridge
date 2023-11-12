#[cfg(all(feature = "defmt", feature = "log"))]
compile_error!("You may not enable both `defmt` and `log` features.");


#[cfg(feature = "defmt")]
pub trait Format : defmt::Format {}

#[cfg(feature = "log")]
pub trait Format : core::fmt::Debug {}