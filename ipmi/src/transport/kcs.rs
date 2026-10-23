use super::{Deadline, Transport, wait};
use crate::{Error, MAX_MESSAGE, backend::Registers};
use alloc::boxed::Box;

pub(super) struct Kcs(pub Box<dyn Registers>);

impl Kcs {
    fn status(&mut self, deadline: &Deadline<'_>) -> Result<u8, Error> {
        wait(&mut *self.0, 1, 2, 0, deadline)
    }

    fn drain(&mut self, status: u8) -> Result<(), Error> {
        if status & 1 != 0 {
            self.0.read(0)?;
        }
        Ok(())
    }

    fn output(&mut self, deadline: &Deadline<'_>) -> Result<u8, Error> {
        wait(&mut *self.0, 1, 3, 1, deadline)?;
        self.0.read(0)
    }

    fn writing(&mut self, deadline: &Deadline<'_>) -> Result<(), Error> {
        let status = self.status(deadline)?;
        if status & 0xc0 != 0x80 {
            return Err(Error::NotReady);
        }
        self.drain(status)
    }

    fn idle(&mut self, deadline: &Deadline<'_>) -> Result<(), Error> {
        let status = self.status(deadline)?;
        if status & 0xc0 == 0 {
            return self.drain(status);
        }
        // Abort only the host interface transaction, never reset the BMC.
        self.drain(status)?;
        self.0.write(1, 0x60)?;
        let status = self.status(deadline)?;
        self.drain(status)?;
        self.0.write(0, 0)?;
        let status = self.status(deadline)?;
        if status & 0xc0 != 0x40 {
            return Err(Error::NotReady);
        }
        self.output(deadline)?;
        self.0.write(0, 0x68)?;
        if self.status(deadline)? & 0xc0 != 0 {
            return Err(Error::NotReady);
        }
        self.output(deadline)?;
        Ok(())
    }
}

impl Transport for Kcs {
    fn initialize(&mut self, deadline: &Deadline<'_>) -> Result<(), Error> {
        self.idle(deadline)
    }

    fn exchange(
        &mut self,
        request: &[u8],
        response: &mut [u8; MAX_MESSAGE],
        deadline: &Deadline<'_>,
    ) -> Result<usize, Error> {
        self.idle(deadline)?;
        self.0.write(1, 0x61)?;
        self.writing(deadline)?;
        for byte in &request[..request.len() - 1] {
            self.0.write(0, *byte)?;
            self.writing(deadline)?;
        }
        self.0.write(1, 0x62)?;
        self.writing(deadline)?;
        self.0.write(0, request[request.len() - 1])?;
        let mut length = 0;
        loop {
            let status = self.status(deadline)?;
            match status & 0xc0 {
                0x40 => {
                    let byte = self.output(deadline)?;
                    if length == response.len() {
                        return Err(Error::MalformedResponse);
                    }
                    response[length] = byte;
                    length += 1;
                    self.0.write(0, 0x68)?;
                }
                0 => {
                    self.output(deadline)?;
                    return Ok(length);
                }
                _ => return Err(Error::NotReady),
            }
        }
    }
}
