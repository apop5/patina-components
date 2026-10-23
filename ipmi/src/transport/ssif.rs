use super::{Deadline, Transport, validate_response};
use crate::{Error, MAX_MESSAGE, backend::Smbus};
use alloc::boxed::Box;

pub(super) struct Ssif {
    bus: Box<dyn Smbus>,
    address: u8,
    pec: bool,
    multipart: u8,
    input_size: usize,
    output_size: usize,
}

impl Ssif {
    pub(super) fn new(bus: Box<dyn Smbus>, address: u8) -> Self {
        Self { bus, address, pec: false, multipart: 0, input_size: 32, output_size: 32 }
    }

    fn read(&mut self, command: u8, block: &mut [u8; 32], deadline: &Deadline<'_>) -> Result<usize, Error> {
        loop {
            match self.bus.read_block(self.address, command, self.pec, block, deadline.remaining()?) {
                Ok(length) if (1..=32).contains(&length) => return Ok(length),
                Ok(_) => return Err(Error::MalformedResponse),
                Err(Error::NotReady) => deadline.pause()?,
                Err(error) => return Err(error),
            }
        }
    }
}

impl Transport for Ssif {
    fn initialize(&mut self, deadline: &Deadline<'_>) -> Result<(), Error> {
        let request = [0x18, 0x57, 0];
        let mut response = [0; MAX_MESSAGE];
        let length = self.exchange(&request, &mut response, deadline)?;
        validate_response(&request, &response[..length])?;
        if response[2] == 0xc1 {
            log::info!("SSIF capabilities unsupported; using single-part, non-PEC baseline");
            return Ok(());
        }
        if response[2] != 0 {
            return Err(Error::Completion(response[2]));
        }
        if length < 7 || response[5] < 3 || response[6] < 3 || response[4] >> 6 == 3 {
            return Err(Error::MalformedResponse);
        }
        self.multipart = response[4] >> 6;
        self.pec = response[4] & 8 != 0;
        let limit = match self.multipart {
            0 => 32,
            1 => 64,
            _ => MAX_MESSAGE,
        };
        self.input_size = usize::from(response[5]).min(limit);
        self.output_size = usize::from(response[6]).min(limit);
        Ok(())
    }

    fn exchange(
        &mut self,
        request: &[u8],
        response: &mut [u8; MAX_MESSAGE],
        deadline: &Deadline<'_>,
    ) -> Result<usize, Error> {
        if request.len() > self.input_size {
            return Err(Error::RequestTooLarge);
        }
        if request.len() <= 32 {
            self.bus.write_block(self.address, 2, self.pec, request, deadline.remaining()?)?;
        } else {
            self.bus.write_block(self.address, 6, self.pec, &request[..32], deadline.remaining()?)?;
            let mut offset = 32;
            while request.len() - offset > 32 {
                self.bus.write_block(
                    self.address,
                    7,
                    self.pec,
                    &request[offset..offset + 32],
                    deadline.remaining()?,
                )?;
                offset += 32;
            }
            self.bus.write_block(self.address, 8, self.pec, &request[offset..], deadline.remaining()?)?;
        }
        let mut block = [0; 32];
        let length = self.read(3, &mut block, deadline)?;
        if length < 2 || block[..2] != [0, 1] {
            if length > self.output_size {
                return Err(Error::MalformedResponse);
            }
            response[..length].copy_from_slice(&block[..length]);
            return Ok(length);
        }
        if self.multipart == 0 || length != 32 {
            return Err(Error::MalformedResponse);
        }
        response[..30].copy_from_slice(&block[2..]);
        let mut offset = 30;
        let mut sequence = 0;
        loop {
            let length = self.read(9, &mut block, deadline)?;
            let last = block[0] == 0xff;
            if length < 2
                || (!last && (block[0] != sequence || length != 32 || self.multipart == 1))
                || offset + length - 1 > self.output_size
            {
                return Err(Error::MalformedResponse);
            }
            response[offset..offset + length - 1].copy_from_slice(&block[1..length]);
            offset += length - 1;
            if last {
                return Ok(offset);
            }
            sequence += 1;
        }
    }
}
