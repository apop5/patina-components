//! Trusted platform configuration, analogous to Patina's platform CpuInfo.

use crate::{
    Error,
    backend::{IpmbBus, Registers, Smbus},
};
use alloc::{boxed::Box, vec::Vec};

pub type EndpointId = &'static str;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interface {
    Kcs,
    Smic,
    Bt,
    Ssif,
    Ipmb,
}

/// Transport-specific resources. Register backends must expose all registers
/// required by the selected interface; SMBus/IPMB controllers remain platform-owned.
pub enum Endpoint {
    Kcs { id: EndpointId, registers: Box<dyn Registers> },
    Smic { id: EndpointId, registers: Box<dyn Registers> },
    Bt { id: EndpointId, registers: Box<dyn Registers> },
    Ssif { id: EndpointId, bus: Box<dyn Smbus>, address: u8 },
    Ipmb { id: EndpointId, bus: Box<dyn IpmbBus>, bmc_address: u8, local_address: u8 },
}

impl Endpoint {
    pub fn id(&self) -> EndpointId {
        match self {
            Self::Kcs { id, .. }
            | Self::Smic { id, .. }
            | Self::Bt { id, .. }
            | Self::Ssif { id, .. }
            | Self::Ipmb { id, .. } => id,
        }
    }

    pub fn interface(&self) -> Interface {
        match self {
            Self::Kcs { .. } => Interface::Kcs,
            Self::Smic { .. } => Interface::Smic,
            Self::Bt { .. } => Interface::Bt,
            Self::Ssif { .. } => Interface::Ssif,
            Self::Ipmb { .. } => Interface::Ipmb,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    pub initialization_us: u64,
    pub transaction_us: u64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self { initialization_us: 5_000_000, transaction_us: 5_000_000 }
    }
}

pub struct IpmiConfig {
    pub endpoints: Vec<Endpoint>,
    /// Only these endpoints, in this order, may be selected.
    pub preference: Vec<EndpointId>,
    pub timeouts: Timeouts,
}

impl IpmiConfig {
    pub fn validate(&self) -> Result<(), Error> {
        if self.endpoints.is_empty()
            || self.preference.is_empty()
            || self.timeouts.initialization_us == 0
            || self.timeouts.transaction_us == 0
            || self.timeouts.initialization_us > i64::MAX as u64
            || self.timeouts.transaction_us > i64::MAX as u64
        {
            return Err(Error::InvalidConfig);
        }
        for (i, endpoint) in self.endpoints.iter().enumerate() {
            if endpoint.id().is_empty() || self.endpoints[..i].iter().any(|e| e.id() == endpoint.id()) {
                return Err(Error::InvalidConfig);
            }
            match endpoint {
                Endpoint::Ssif { address, .. } if !valid_address(*address) => return Err(Error::InvalidConfig),
                Endpoint::Ipmb { bmc_address, local_address, .. }
                    if !valid_address(*bmc_address)
                        || !valid_address(*local_address)
                        || bmc_address == local_address =>
                {
                    return Err(Error::InvalidConfig);
                }
                _ => {}
            }
        }
        for (i, id) in self.preference.iter().enumerate() {
            if !self.endpoints.iter().any(|e| e.id() == *id) || self.preference[..i].contains(id) {
                return Err(Error::InvalidConfig);
            }
        }
        Ok(())
    }
}

fn valid_address(address: u8) -> bool {
    (0x08..=0x77).contains(&address)
}

/// Platform opt-in: register `IpmiComponent::<Platform>::new()` and the optional
/// `IpmiProtocolInstaller`. Resource ownership transfers to the component once.
pub trait IpmiInfo: 'static {
    fn config() -> Result<IpmiConfig, Error>;
}
