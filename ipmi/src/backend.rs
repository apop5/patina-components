//! Platform hardware adapters. Bus operations must honor the supplied timeout.

use crate::Error;
use alloc::boxed::Box;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mmio_window_uses_spacing_width_and_bounds() {
        let mut data = [0u32; 3];
        // SAFETY: The aligned test allocation is exclusively owned for the
        // window's lifetime, and u32 accesses match its layout.
        let mut window =
            unsafe { RegisterWindow::new(data.as_mut_ptr() as usize, 4, 3, AccessWidth::Dword, AddressSpace::Mmio) }
                .unwrap();
        window.write(1, 0xab).unwrap();
        assert_eq!(window.read(1), Ok(0xab));
        assert_eq!(window.read(3), Err(Error::InvalidConfig));
        assert_eq!(window.write(3, 0), Err(Error::InvalidConfig));
        assert_eq!(data, [0, 0xab, 0]);
    }

    #[test]
    fn invalid_layouts_are_rejected_before_hardware_access() {
        for (base, spacing, count, width, space) in [
            (0, 1, 2, AccessWidth::Byte, AddressSpace::Mmio),
            (1, 4, 2, AccessWidth::Dword, AddressSpace::Mmio),
            (4, 1, 2, AccessWidth::Dword, AddressSpace::Mmio),
            (4, 4, 0, AccessWidth::Dword, AddressSpace::Mmio),
            (usize::MAX - 3, 4, 3, AccessWidth::Dword, AddressSpace::Mmio),
            (0xffff, 1, 2, AccessWidth::Byte, AddressSpace::IoPort),
        ] {
            // SAFETY: Invalid descriptions are rejected before accessing registers.
            let result = unsafe { RegisterWindow::new(base, spacing, count, width, space) };
            assert!(matches!(result, Err(Error::InvalidConfig)));
        }
    }

    #[test]
    fn explicit_mmio_addresses_support_nonuniform_and_descending_registers() {
        let mut data = [0u32; 5];
        let base = data.as_mut_ptr() as usize;
        let mut addresses = [base + 16, base, base + 8];
        // SAFETY: All three aligned registers belong to the live, exclusively
        // owned test allocation and support u32 accesses.
        let mut window =
            unsafe { RegisterWindow::from_addresses(&addresses, AccessWidth::Dword, AddressSpace::Mmio) }.unwrap();
        addresses.fill(0);
        for register in 0..3 {
            window.write(register, 0xa0 + register).unwrap();
            assert_eq!(window.read(register), Ok(0xa0 + register));
        }
        assert_eq!(window.address(0), Ok(base + 16));
        assert_eq!(window.address(1), Ok(base));
        assert_eq!(window.address(2), Ok(base + 8));
        assert_eq!(window.read(3), Err(Error::InvalidConfig));
        assert_eq!(window.write(3, 0), Err(Error::InvalidConfig));
        assert_eq!(data, [0xa1, 0, 0xa2, 0, 0xa0]);
    }

    #[test]
    fn explicit_addresses_validate_every_register_before_hardware_access() {
        let invalid: &[(&[usize], AccessWidth, AddressSpace)] = &[
            (&[], AccessWidth::Byte, AddressSpace::Mmio),
            (&[4, 0], AccessWidth::Byte, AddressSpace::Mmio),
            (&[4, 9], AccessWidth::Dword, AddressSpace::Mmio),
            (&[4, usize::MAX], AccessWidth::Dword, AddressSpace::Mmio),
            (&[0xca8, 0x10000], AccessWidth::Byte, AddressSpace::IoPort),
            (&[0xca8, 0xffff], AccessWidth::Word, AddressSpace::IoPort),
            (&[4; 257], AccessWidth::Byte, AddressSpace::Mmio),
        ];
        for &(addresses, width, space) in invalid {
            // SAFETY: Invalid descriptions are rejected without register access.
            let result = unsafe { RegisterWindow::from_addresses(addresses, width, space) };
            assert!(matches!(result, Err(Error::InvalidConfig)));
        }
    }
}

/// Byte-oriented logical registers: data=0, control/status=1, flags=2 (SMIC).
/// BT uses control=0 and data=1.
pub trait Registers: Send {
    fn read(&mut self, register: u8) -> Result<u8, Error>;
    fn write(&mut self, register: u8, value: u8) -> Result<(), Error>;
}

/// SMBus block transactions. Addresses are seven-bit; block lengths exclude
/// the SMBus count byte. The controller generates/verifies PEC when requested.
pub trait Smbus: Send {
    fn write_block(&mut self, address: u8, command: u8, pec: bool, data: &[u8], timeout_us: u64) -> Result<(), Error>;
    fn read_block(
        &mut self,
        address: u8,
        command: u8,
        pec: bool,
        data: &mut [u8; 32],
        timeout_us: u64,
    ) -> Result<usize, Error>;
}

/// Direct IPMB master transmission and slave reception, not Send Message bridging.
/// Frames include the destination address as byte zero on both send and receive.
/// Implementations must receive responses sent to the configured local address.
pub trait IpmbBus: Send {
    fn send(&mut self, frame: &[u8], timeout_us: u64) -> Result<(), Error>;
    fn receive(&mut self, frame: &mut [u8], timeout_us: u64) -> Result<usize, Error>;
}

/// Monotonic microsecond counter; wrapping arithmetic is supported.
pub trait Clock: Send + Sync {
    fn now_us(&self) -> u64;
    fn relax(&self) {
        core::hint::spin_loop();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressSpace {
    Mmio,
    IoPort,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessWidth {
    Byte,
    Word,
    Dword,
}

impl AccessWidth {
    fn bytes(self) -> usize {
        match self {
            Self::Byte => 1,
            Self::Word => 2,
            Self::Dword => 4,
        }
    }
}

/// Exclusive, platform-described registers in logical register order.
pub struct RegisterWindow {
    addresses: Box<[usize]>,
    width: AccessWidth,
    space: AddressSpace,
}

impl RegisterWindow {
    /// # Safety
    ///
    /// The complete register window must be mapped as device memory (MMIO) or
    /// decoded as I/O ports, live until ExitBootServices, and exclusively owned.
    /// The hardware must support the given width and little-endian low-byte
    /// accesses. No other driver, SMM agent, or processor may race these registers.
    pub unsafe fn new(
        base: usize,
        spacing: usize,
        count: u8,
        width: AccessWidth,
        space: AddressSpace,
    ) -> Result<Self, Error> {
        let bytes = width.bytes();
        if count == 0 || spacing < bytes || !spacing.is_multiple_of(bytes) {
            return Err(Error::InvalidConfig);
        }
        let last = base
            .checked_add(spacing.checked_mul(usize::from(count - 1)).ok_or(Error::InvalidConfig)?)
            .ok_or(Error::InvalidConfig)?;
        Self::validate_address(base, width, space)?;
        Self::validate_address(last, width, space)?;
        let addresses = (0..count).map(|register| base + usize::from(register) * spacing).collect();
        Ok(Self { addresses, width, space })
    }

    /// Creates a window with an explicit address for each logical register.
    ///
    /// Addresses need not be adjacent, uniformly spaced, or ascending. For KCS
    /// the order is data, status/command; BT uses control, data; SMIC uses data,
    /// status/control, flags. The address list is copied into the window.
    ///
    /// # Safety
    ///
    /// Every register must be mapped as device memory (MMIO) or decoded as an
    /// I/O port, live until ExitBootServices, and exclusively owned. The hardware
    /// must support the given width and little-endian low-byte accesses at each
    /// address. No other driver, SMM agent, or processor may race these registers.
    pub unsafe fn from_addresses(addresses: &[usize], width: AccessWidth, space: AddressSpace) -> Result<Self, Error> {
        if addresses.is_empty() || addresses.len() > usize::from(u8::MAX) + 1 {
            return Err(Error::InvalidConfig);
        }
        for &address in addresses {
            Self::validate_address(address, width, space)?;
        }
        Ok(Self { addresses: addresses.into(), width, space })
    }

    fn validate_address(address: usize, width: AccessWidth, space: AddressSpace) -> Result<(), Error> {
        let bytes = width.bytes();
        if !address.is_multiple_of(bytes) {
            return Err(Error::InvalidConfig);
        }
        let end = address.checked_add(bytes - 1).ok_or(Error::InvalidConfig)?;
        if (space == AddressSpace::Mmio && address == 0)
            || (space == AddressSpace::IoPort && (end > usize::from(u16::MAX) || !cfg!(target_arch = "x86_64")))
        {
            return Err(Error::InvalidConfig);
        }
        Ok(())
    }

    fn address(&self, register: u8) -> Result<usize, Error> {
        self.addresses.get(usize::from(register)).copied().ok_or(Error::InvalidConfig)
    }
}

impl Registers for RegisterWindow {
    fn read(&mut self, register: u8) -> Result<u8, Error> {
        let address = self.address(register)?;
        // SAFETY: Construction guarantees a live, aligned, exclusively owned
        // register window; address() bounds the logical index.
        unsafe {
            match (self.space, self.width) {
                (AddressSpace::Mmio, AccessWidth::Byte) => Ok((address as *const u8).read_volatile()),
                (AddressSpace::Mmio, AccessWidth::Word) => Ok((address as *const u16).read_volatile() as u8),
                (AddressSpace::Mmio, AccessWidth::Dword) => Ok((address as *const u32).read_volatile() as u8),
                #[cfg(target_arch = "x86_64")]
                (AddressSpace::IoPort, width) => {
                    let value: u32;
                    match width {
                        AccessWidth::Byte => {
                            let byte: u8;
                            core::arch::asm!("in al, dx", in("dx") address as u16, out("al") byte, options(nomem, nostack, preserves_flags));
                            return Ok(byte);
                        }

                        AccessWidth::Word => {
                            let word: u16;
                            core::arch::asm!("in ax, dx", in("dx") address as u16, out("ax") word, options(nomem, nostack, preserves_flags));
                            return Ok(word as u8);
                        }
                        AccessWidth::Dword => {
                            core::arch::asm!("in eax, dx", in("dx") address as u16, out("eax") value, options(nomem, nostack, preserves_flags))
                        }
                    }
                    Ok(value as u8)
                }
                #[cfg(not(target_arch = "x86_64"))]
                (AddressSpace::IoPort, _) => Err(Error::Unsupported),
            }
        }
    }

    fn write(&mut self, register: u8, value: u8) -> Result<(), Error> {
        let address = self.address(register)?;
        // SAFETY: Same bounded exclusive register window as read().
        unsafe {
            match (self.space, self.width) {
                (AddressSpace::Mmio, AccessWidth::Byte) => (address as *mut u8).write_volatile(value),
                (AddressSpace::Mmio, AccessWidth::Word) => (address as *mut u16).write_volatile(u16::from(value)),
                (AddressSpace::Mmio, AccessWidth::Dword) => (address as *mut u32).write_volatile(u32::from(value)),
                #[cfg(target_arch = "x86_64")]
                (AddressSpace::IoPort, AccessWidth::Byte) => {
                    core::arch::asm!("out dx, al", in("dx") address as u16, in("al") value, options(nomem, nostack, preserves_flags))
                }
                #[cfg(target_arch = "x86_64")]
                (AddressSpace::IoPort, AccessWidth::Word) => {
                    core::arch::asm!("out dx, ax", in("dx") address as u16, in("ax") u16::from(value), options(nomem, nostack, preserves_flags))
                }
                #[cfg(target_arch = "x86_64")]
                (AddressSpace::IoPort, AccessWidth::Dword) => {
                    core::arch::asm!("out dx, eax", in("dx") address as u16, in("eax") u32::from(value), options(nomem, nostack, preserves_flags))
                }
                #[cfg(not(target_arch = "x86_64"))]
                (AddressSpace::IoPort, _) => return Err(Error::Unsupported),
            }
        }
        Ok(())
    }
}
