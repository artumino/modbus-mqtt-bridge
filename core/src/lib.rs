#![no_std]
#![feature(error_in_core)]

#[cfg(feature = "std")]
extern crate std;

pub mod configuration;
pub mod registry_map;
pub mod async_traits;
pub mod bridge;
pub mod modbus;

mod mqtt;
mod timing;
mod tasks;


#[cfg(feature = "log")]
mod logging;

#[cfg(feature = "defmt")]
pub mod logging;