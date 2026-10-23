use super::{Deadline, Transport, wait};
use crate::{Error, MAX_MESSAGE, backend::Registers};
use alloc::boxed::Box;

pub(super) struct Smic(pub Box<dyn Registers>);

impl Smic {
    fn control(&mut self, code: u8, byte: Option<u8>, deadline: &Deadline<'_>) -> Result<u8, Error> {
        let flags = wait(&mut *self.0, 2, 1, 0, deadline)?;
        self.0.write(1, code)?;
        if let Some(byte) = byte {
            self.0.write(0, byte)?;
        }
        self.0.write(2, flags | 1)?;
        wait(&mut *self.0, 2, 1, 0, deadline)?;
        self.0.read(1)
    }

    fn expect(&mut self, code: u8, byte: Option<u8>, expected: u8, deadline: &Deadline<'_>) -> Result<(), Error> {
        if self.control(code, byte, deadline)? != expected {
            return Err(Error::NotReady);
        }
        Ok(())
    }
}

impl Transport for Smic {
    fn initialize(&mut self, deadline: &Deadline<'_>) -> Result<(), Error> {
        self.expect(0x40, None, 0xc0, deadline)
    }

    fn exchange(
        &mut self,
        request: &[u8],
        response: &mut [u8; MAX_MESSAGE],
        deadline: &Deadline<'_>,
    ) -> Result<usize, Error> {
        self.initialize(deadline)?;
        self.expect(0x41, Some(request[0]), 0xc1, deadline)?;
        for (i, byte) in request.iter().enumerate().skip(1) {
            wait(&mut *self.0, 2, 0x41, 0x40, deadline)?;
            let code = if i + 1 == request.len() { 0x43 } else { 0x42 };
            self.expect(code, Some(*byte), code | 0x80, deadline)?;
        }
        if self.0.read(0)? != 0 {
            return Err(Error::Device);
        }
        wait(&mut *self.0, 2, 0x81, 0x80, deadline)?;
        self.expect(0x44, None, 0xc4, deadline)?;
        let mut length = 0;
        loop {
            wait(&mut *self.0, 2, 0x81, 0x80, deadline)?;
            if length == response.len() {
                return Err(Error::MalformedResponse);
            }
            response[length] = self.0.read(0)?;
            length += 1;
            match self.control(0x45, None, deadline)? {
                0xc5 => {}
                0xc6 => {
                    if length == response.len() {
                        return Err(Error::MalformedResponse);
                    }
                    response[length] = self.0.read(0)?;
                    length += 1;
                    self.expect(0x46, None, 0xc0, deadline)?;
                    return Ok(length);
                }
                _ => return Err(Error::MalformedResponse),
            }
        }
    }
}
