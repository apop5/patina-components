#![doc = include_str!("../README.md")]
#![no_std]

extern crate alloc;

pub mod backend;
pub mod component;
pub mod config;
pub mod protocol;
pub mod service;
mod transport;

pub use component::{IpmiComponent, IpmiProtocolInstaller};
pub use config::{Endpoint, EndpointId, Interface, IpmiConfig, IpmiInfo, Timeouts};
pub use service::{Availability, EndpointStatus, Error, Ipmi, Request, Response};

/// Maximum complete local IPMI message, including transport-independent headers.
pub const MAX_MESSAGE: usize = 255;
