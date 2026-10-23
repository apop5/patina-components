use super::{Deadline, Transport};
use crate::{Error, MAX_MESSAGE, backend::IpmbBus};
use alloc::boxed::Box;

pub(super) struct Ipmb {
    bus: Box<dyn IpmbBus>,
    bmc: u8,
    local: u8,
    sequence: u8,
}

fn checksum(bytes: &[u8]) -> u8 {
    0u8.wrapping_sub(bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)))
}

impl Ipmb {
    pub(super) fn new(bus: Box<dyn IpmbBus>, bmc: u8, local: u8) -> Self {
        Self { bus, bmc, local, sequence: 0 }
    }
}

impl Transport for Ipmb {
    fn exchange(
        &mut self,
        request: &[u8],
        response: &mut [u8; MAX_MESSAGE],
        deadline: &Deadline<'_>,
    ) -> Result<usize, Error> {
        // IPMB messages include both addresses, two checksums and rqSeq.
        if request.len() + 5 > 32 {
            return Err(Error::RequestTooLarge);
        }
        let mut frame = [0u8; 32];
        frame[0] = self.bmc << 1;
        frame[1] = request[0];
        frame[2] = checksum(&frame[..2]);
        frame[3] = self.local << 1;
        let sequence = self.sequence << 2;
        self.sequence = (self.sequence + 1) & 0x3f;
        frame[4] = sequence;
        frame[5..request.len() + 4].copy_from_slice(&request[1..]);
        let length = request.len() + 5;
        frame[length - 1] = checksum(&frame[3..length - 1]);
        self.bus.send(&frame[..length], deadline.remaining()?)?;
        loop {
            let length = self.bus.receive(&mut frame, deadline.remaining()?)?;
            if !(8..=32).contains(&length) {
                return Err(Error::MalformedResponse);
            }
            if checksum(&frame[..3]) != 0 || checksum(&frame[3..length]) != 0 {
                return Err(Error::MalformedResponse);
            }
            if frame[0] != self.local << 1
                || frame[3] != self.bmc << 1
                || frame[4] != (sequence | (request[0] & 3))
                || frame[1] != ((request[0] & 0xfc) | 4)
                || frame[5] != request[1]
            {
                // Discard stale responses; never replay the request.
                deadline.pause()?;
                continue;
            }
            response[0] = frame[1] | (request[0] & 3);
            response[1..length - 5].copy_from_slice(&frame[5..length - 1]);
            return Ok(length - 5);
        }
    }
}
