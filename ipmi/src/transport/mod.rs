//! Local interface framing and state machines.

mod bt;
mod ipmb;
mod kcs;
mod smic;
mod ssif;

use crate::{
    Error, MAX_MESSAGE,
    backend::{Clock, Registers},
    config::Endpoint,
};
use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, Ordering};

pub(crate) struct Deadline<'a> {
    clock: &'a dyn Clock,
    start: u64,
    timeout: u64,
    stopped: Option<&'a AtomicBool>,
}

impl<'a> Deadline<'a> {
    pub(crate) fn new(clock: &'a dyn Clock, timeout: u64, stopped: Option<&'a AtomicBool>) -> Self {
        Self { clock, start: clock.now_us(), timeout, stopped }
    }

    pub(crate) fn remaining(&self) -> Result<u64, Error> {
        if self.stopped.is_some_and(|s| s.load(Ordering::Acquire)) {
            return Err(Error::Stopped);
        }
        let elapsed = self.clock.now_us().wrapping_sub(self.start);
        if elapsed >= self.timeout {
            return Err(Error::Timeout);
        }
        Ok(self.timeout - elapsed)
    }

    pub(crate) fn pause(&self) -> Result<(), Error> {
        self.remaining()?;
        self.clock.relax();
        Ok(())
    }
}

pub(crate) trait Transport: Send {
    fn initialize(&mut self, _deadline: &Deadline<'_>) -> Result<(), Error> {
        Ok(())
    }
    fn exchange(
        &mut self,
        request: &[u8],
        response: &mut [u8; MAX_MESSAGE],
        deadline: &Deadline<'_>,
    ) -> Result<usize, Error>;
}

pub(crate) fn create(endpoint: Endpoint) -> Box<dyn Transport> {
    match endpoint {
        Endpoint::Kcs { registers, .. } => Box::new(kcs::Kcs(registers)),
        Endpoint::Smic { registers, .. } => Box::new(smic::Smic(registers)),
        Endpoint::Bt { registers, .. } => Box::new(bt::Bt::new(registers)),
        Endpoint::Ssif { bus, address, .. } => Box::new(ssif::Ssif::new(bus, address)),
        Endpoint::Ipmb { bus, bmc_address, local_address, .. } => {
            Box::new(ipmb::Ipmb::new(bus, bmc_address, local_address))
        }
    }
}

pub(crate) fn wait(
    registers: &mut dyn Registers,
    register: u8,
    mask: u8,
    expected: u8,
    deadline: &Deadline<'_>,
) -> Result<u8, Error> {
    loop {
        deadline.remaining()?;
        let status = registers.read(register)?;
        if status == 0xff {
            return Err(Error::NotPresent);
        }
        if status & mask == expected {
            return Ok(status);
        }
        deadline.pause()?;
    }
}

pub(crate) fn validate_response(request: &[u8], response: &[u8]) -> Result<(), Error> {
    if response.len() < 3 || response[0] != (request[0] | 4) || response[1] != request[1] {
        return Err(Error::MalformedResponse);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
