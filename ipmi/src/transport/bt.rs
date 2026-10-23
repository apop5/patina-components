use super::{Deadline, Transport, validate_response, wait};
use crate::{Error, MAX_MESSAGE, backend::Registers};
use alloc::boxed::Box;

pub(super) struct Bt {
    registers: Box<dyn Registers>,
    sequence: u8,
    input_size: usize,
    output_size: usize,
}

impl Bt {
    pub(super) fn new(registers: Box<dyn Registers>) -> Self {
        Self { registers, sequence: 0, input_size: 32, output_size: 32 }
    }
}

impl Transport for Bt {
    fn initialize(&mut self, deadline: &Deadline<'_>) -> Result<(), Error> {
        let status = wait(&mut *self.registers, 0, 0x84, 0, deadline)?;
        if status & 0x40 != 0 {
            self.registers.write(0, 0x40)?;
        }
        if status & 8 != 0 {
            self.registers.write(0, 8)?;
        }
        let request = [0x18, 0x36];
        let mut response = [0; MAX_MESSAGE];
        let length = self.exchange(&request, &mut response, deadline)?;
        validate_response(&request, &response[..length])?;
        if response[2] == 0xc1 {
            log::info!("BT capabilities command unsupported; using 32-byte baseline buffers");
            return Ok(());
        }
        if response[2] != 0 {
            return Err(Error::Completion(response[2]));
        }
        if length < 8 || response[4] < 8 || response[5] < 8 {
            return Err(Error::MalformedResponse);
        }
        self.input_size = usize::from(response[4]);
        self.output_size = usize::from(response[5]);
        Ok(())
    }

    fn exchange(
        &mut self,
        request: &[u8],
        response: &mut [u8; MAX_MESSAGE],
        deadline: &Deadline<'_>,
    ) -> Result<usize, Error> {
        if request.len() + 2 > self.input_size || request.len() + 1 > 255 {
            return Err(Error::RequestTooLarge);
        }
        wait(&mut *self.registers, 0, 0x84, 0, deadline)?;
        self.registers.write(0, 1)?;
        self.registers.write(1, (request.len() + 1) as u8)?;
        self.registers.write(1, request[0])?;
        let sequence = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        self.registers.write(1, sequence)?;
        for byte in &request[1..] {
            deadline.remaining()?;
            self.registers.write(1, *byte)?;
        }
        self.registers.write(0, 4)?;
        wait(&mut *self.registers, 0, 8, 8, deadline)?;
        self.registers.write(0, 0x40)?;
        let result = (|| {
            self.registers.write(0, 8)?;
            self.registers.write(0, 2)?;
            let length = usize::from(self.registers.read(1)?);
            if length < 4 || length + 1 > self.output_size {
                return Err(Error::MalformedResponse);
            }
            response[0] = self.registers.read(1)?;
            let received_sequence = self.registers.read(1)?;
            for byte in &mut response[1..length - 1] {
                deadline.remaining()?;
                *byte = self.registers.read(1)?;
            }
            if received_sequence != sequence {
                return Err(Error::MalformedResponse);
            }
            Ok(length - 1)
        })();
        let release = self.registers.write(0, 0x40);
        match (result, release) {
            (Err(error), Err(release_error)) => {
                log::error!("BT receive failed: {error:?}; releasing host busy failed: {release_error:?}");
                Err(error)
            }
            (Err(error), _) | (_, Err(error)) => Err(error),
            (Ok(length), Ok(())) => Ok(length),
        }
    }
}
