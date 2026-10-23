use super::*;
use crate::{Availability, EndpointStatus, Interface, Response};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

struct FakeIpmi(Arc<AtomicUsize>);
impl Ipmi for FakeIpmi {
    fn submit(&self, request: Request<'_>, response: &mut [u8]) -> Result<Response, Error> {
        self.0.fetch_add(1, Ordering::Relaxed);
        assert_eq!((request.netfn, request.lun, request.command), (6, 0, 1));
        assert_eq!(request.data, &[0xaa]);
        if response.len() < 2 {
            return Err(Error::BufferTooSmall { required: 2 });
        }
        response[..2].copy_from_slice(&[0xbb, 0xcc]);
        Ok(Response { completion_code: 0xc1, data_len: 2 })
    }
    fn endpoints(&self) -> &[EndpointStatus] {
        &[]
    }
    fn selected(&self) -> EndpointStatus {
        EndpointStatus { id: "test", interface: Interface::Kcs, availability: Availability::Available }
    }
}

fn wrapper() -> (Box<ProtocolWrapper>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    (ProtocolWrapper::new(Service::mock(Box::new(FakeIpmi(calls.clone())))), calls)
}

#[test]
fn c_abi_returns_completion_code_first_and_supports_overlapping_buffers() {
    let (mut wrapper, calls) = wrapper();
    let mut bytes = [0xaa, 0, 0, 0];
    let mut length = bytes.len() as u32;
    // SAFETY: All pointers refer to live, correctly sized test allocations.
    let status = unsafe {
        (wrapper.interface.submit_command)(
            &mut wrapper.interface,
            6,
            1,
            bytes.as_mut_ptr(),
            1,
            bytes.as_mut_ptr(),
            &mut length,
        )
    };
    assert_eq!(status, efi::Status::SUCCESS);
    assert_eq!(length, 3);
    assert_eq!(&bytes[..3], &[0xc1, 0xbb, 0xcc]);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[test]
fn c_abi_reports_completion_inclusive_required_size() {
    let (mut wrapper, calls) = wrapper();
    let mut request = [0xaa];
    let mut response = [0x55; 2];
    let mut length = 2;
    // SAFETY: Test buffers satisfy the ABI memory contract.
    let status = unsafe {
        submit_command(&mut wrapper.interface, 6, 1, request.as_mut_ptr(), 1, response.as_mut_ptr(), &mut length)
    };
    assert_eq!(status, efi::Status::BUFFER_TOO_SMALL);
    assert_eq!(length, 3);
    assert_eq!(response, [0x55; 2]);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[test]
fn c_abi_zero_capacity_does_not_execute_a_command() {
    let (mut wrapper, calls) = wrapper();
    let mut length = 0;
    // SAFETY: NULL buffers with zero sizes are permitted.
    let status =
        unsafe { submit_command(&mut wrapper.interface, 6, 1, ptr::null_mut(), 0, ptr::null_mut(), &mut length) };
    assert_eq!(status, efi::Status::BUFFER_TOO_SMALL);
    assert_eq!(length, 1);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[test]
fn c_abi_rejects_null_size_request_and_self_overwrite() {
    let (mut wrapper, calls) = wrapper();
    let this = &mut wrapper.interface as *mut _;
    let mut length = 4;
    let mut response = [0; 4];
    // SAFETY: Deliberately invalid NULL arguments are rejected before use;
    // all non-NULL pointers point to live test allocations.
    unsafe {
        assert_eq!(
            submit_command(this, 6, 1, ptr::null_mut(), 0, response.as_mut_ptr(), ptr::null_mut()),
            efi::Status::INVALID_PARAMETER
        );
        assert_eq!(
            submit_command(this, 6, 1, ptr::null_mut(), 1, response.as_mut_ptr(), &mut length),
            efi::Status::INVALID_PARAMETER
        );
        assert_eq!(
            submit_command(this, 6, 1, ptr::null_mut(), 0, this.cast(), &mut length),
            efi::Status::INVALID_PARAMETER
        );
        assert_eq!(
            submit_command(this, 6, 1, ptr::null_mut(), 0, (&raw mut length).cast(), &mut length),
            efi::Status::INVALID_PARAMETER
        );
    }
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[test]
fn protocol_layout_and_guid_match_edk2() {
    assert_eq!(size_of::<IpmiProtocol>(), size_of::<SubmitCommand>());
    assert_eq!(core::mem::offset_of!(ProtocolWrapper, interface), 0);
    assert_eq!(IpmiProtocol::PROTOCOL_GUID, IPMI_PROTOCOL_GUID);
}

#[test]
fn c_adapter_rejects_inconsistent_rust_service_lengths_without_writing_outputs() {
    struct InvalidIpmi(bool);
    impl Ipmi for InvalidIpmi {
        fn submit(&self, _: Request<'_>, _: &mut [u8]) -> Result<Response, Error> {
            if self.0 {
                Ok(Response { completion_code: 0, data_len: usize::MAX })
            } else {
                Err(Error::BufferTooSmall { required: usize::MAX })
            }
        }
        fn endpoints(&self) -> &[EndpointStatus] {
            &[]
        }
        fn selected(&self) -> EndpointStatus {
            panic!("not used by protocol submission")
        }
    }
    for success in [true, false] {
        let mut wrapper = ProtocolWrapper::new(Service::mock(Box::new(InvalidIpmi(success))));
        let mut output = [0x55; 4];
        let mut length = 4;
        // SAFETY: Valid wrapper, size pointer, and output buffer; zero-sized request.
        let status = unsafe {
            submit_command(&mut wrapper.interface, 6, 1, ptr::null_mut(), 0, output.as_mut_ptr(), &mut length)
        };
        assert_eq!(status, efi::Status::DEVICE_ERROR);
        assert_eq!(output, [0x55; 4]);
        assert_eq!(length, 4);
    }
}
