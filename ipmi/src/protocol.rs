//! EDK II MdeModulePkg IPMI_PROTOCOL ABI (not IpmiTransportProtocol).

use crate::{Error, Ipmi, MAX_MESSAGE, service::Request};
use alloc::boxed::Box;
use core::{
    mem::{align_of, size_of},
    ptr,
};
use patina::{BinaryGuid, component::service::Service, protocol::ProtocolInterface, standard::efi};

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod tests;

pub const IPMI_PROTOCOL_GUID: BinaryGuid = BinaryGuid::from_string("dbc6381f-5554-4d14-8ffd-76d787b8acbf");

pub type SubmitCommand = unsafe extern "efiapi" fn(
    this: *mut IpmiProtocol,
    net_function: u8,
    command: u8,
    request_data: *mut u8,
    request_data_size: u32,
    response_data: *mut u8,
    response_data_size: *mut u32,
) -> efi::Status;

#[repr(C)]
pub struct IpmiProtocol {
    pub submit_command: SubmitCommand,
}

// SAFETY: The GUID and one EFIAPI function pointer match EDK II's IPMI_PROTOCOL.
unsafe impl ProtocolInterface for IpmiProtocol {
    const PROTOCOL_GUID: BinaryGuid = IPMI_PROTOCOL_GUID;
}

#[repr(C)]
pub(crate) struct ProtocolWrapper {
    interface: IpmiProtocol,
    service: Service<dyn Ipmi>,
}

// SAFETY: The first repr(C) field is the exact IPMI_PROTOCOL ABI. The private
// trailing service is not visible to C callers.
unsafe impl ProtocolInterface for ProtocolWrapper {
    const PROTOCOL_GUID: BinaryGuid = IPMI_PROTOCOL_GUID;
}

impl ProtocolWrapper {
    pub(crate) fn new(service: Service<dyn Ipmi>) -> Box<Self> {
        Box::new(Self { interface: IpmiProtocol { submit_command }, service })
    }
}

fn overlaps(first: usize, first_len: usize, second: usize, second_len: usize) -> bool {
    first_len != 0
        && second_len != 0
        && (first.checked_add(first_len).is_none_or(|end| end > second)
            && second.checked_add(second_len).is_none_or(|end| end > first))
}

/// # Safety
///
/// This must be a live interface published by this component. Nonzero buffer
/// lengths require valid readable/writable caller memory. The size pointer must
/// be valid and aligned. Arbitrary invalid pointers cannot be validated by UEFI.
unsafe extern "efiapi" fn submit_command(
    this: *mut IpmiProtocol,
    net_function: u8,
    command: u8,
    request_data: *mut u8,
    request_data_size: u32,
    response_data: *mut u8,
    response_data_size: *mut u32,
) -> efi::Status {
    // SAFETY: The protocol caller supplies the ABI's valid memory regions.
    let result = unsafe {
        submit_inner(this, net_function, command, request_data, request_data_size, response_data, response_data_size)
    };
    match result {
        Ok(()) => efi::Status::SUCCESS,
        Err(error) => {
            log::error!("IPMI_PROTOCOL submission failed: {error:?}");
            patina::error::EfiError::from(error).into()
        }
    }
}

unsafe fn submit_inner(
    this: *mut IpmiProtocol,
    net_function: u8,
    command: u8,
    request_data: *mut u8,
    request_data_size: u32,
    response_data: *mut u8,
    response_data_size: *mut u32,
) -> Result<(), Error> {
    if this.is_null()
        || !(this as usize).is_multiple_of(align_of::<ProtocolWrapper>())
        || response_data_size.is_null()
        || !(response_data_size as usize).is_multiple_of(align_of::<u32>())
        || (request_data.is_null() && request_data_size != 0)
        || net_function > 0x3e
        || net_function & 1 != 0
    {
        return Err(Error::InvalidRequest);
    }
    let request_len = request_data_size as usize;
    if request_len > MAX_MESSAGE - 2 {
        return Err(Error::RequestTooLarge);
    }
    // SAFETY: The ABI requires a valid, aligned UINT32 size pointer.
    let capacity = unsafe { response_data_size.read() } as usize;
    if (response_data.is_null() && capacity != 0)
        || overlaps(this as usize, size_of::<ProtocolWrapper>(), response_data as usize, capacity)
        || overlaps(this as usize, size_of::<ProtocolWrapper>(), response_data_size as usize, size_of::<u32>())
        || overlaps(response_data as usize, capacity, response_data_size as usize, size_of::<u32>())
    {
        return Err(Error::InvalidRequest);
    }
    if capacity == 0 {
        // A local IPMI response always needs at least a completion byte. Do not
        // execute commands merely to discover their response length.
        // SAFETY: Validated size pointer, no references alias this write.
        unsafe {
            response_data_size.write(1);
        }
        return Err(Error::BufferTooSmall { required: 1 });
    }

    // Copy before creating any output references; C callers may reuse buffers.
    let mut request = [0; MAX_MESSAGE - 2];
    if request_len != 0 {
        // SAFETY: ABI guarantees request_len readable bytes; stack buffer is disjoint.
        unsafe {
            ptr::copy_nonoverlapping(request_data, request.as_mut_ptr(), request_len);
        }
    }
    let mut response = [0; MAX_MESSAGE - 3];
    // SAFETY: This is the first field of the live ProtocolWrapper we installed.
    let wrapper = unsafe { &*this.cast::<ProtocolWrapper>() };
    let result = wrapper.service.submit(
        Request { netfn: net_function, lun: 0, command, data: &request[..request_len] },
        &mut response[..(capacity - 1).min(MAX_MESSAGE - 3)],
    );
    match result {
        Ok(result) => {
            if result.data_len > response.len() || result.data_len >= capacity {
                return Err(Error::MalformedResponse);
            }
            // SAFETY: ABI guarantees capacity writable bytes, validated disjoint
            // from wrapper and size. Payload is staged in a separate stack buffer.
            unsafe {
                response_data.write(result.completion_code);
                ptr::copy_nonoverlapping(response.as_ptr(), response_data.add(1), result.data_len);
                response_data_size.write((result.data_len + 1) as u32);
            }
            Ok(())
        }
        Err(Error::BufferTooSmall { required }) => {
            if required > response.len() {
                return Err(Error::MalformedResponse);
            }
            // SAFETY: Valid size pointer; outputs have not been borrowed.
            unsafe {
                response_data_size.write((required + 1) as u32);
            }
            Err(Error::BufferTooSmall { required: required + 1 })
        }
        Err(error) => Err(error),
    }
}
