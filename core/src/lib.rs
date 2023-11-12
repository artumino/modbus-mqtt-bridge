#![no_std]
#![feature(error_in_core)]

#[cfg(feature = "std")]
extern crate std;

pub mod async_traits;
pub mod bridge;
pub mod configuration;
pub mod modbus;
pub mod registry_map;

mod mqtt;
mod tasks;
mod timing;

#[cfg(feature = "log")]
mod logging;

#[cfg(feature = "defmt")]
pub mod logging;
