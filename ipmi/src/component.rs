//! Patina component registration, timer adaptation, and protocol publication.

use crate::{
    Error, Ipmi,
    backend::Clock,
    config::IpmiInfo,
    protocol::{IPMI_PROTOCOL_GUID, ProtocolWrapper},
    service::IpmiService,
};
use alloc::boxed::Box;
use core::marker::PhantomData;
use patina::{
    component::{
        Storage, component,
        service::{Service, perf_timer::ArchTimerFunctionality},
    },
    error::EfiError,
    standard::efi,
    uefi::{
        boot_services::{BootServices, StandardBootServices, tpl::Tpl},
        event::EventType,
    },
};

/// Platforms explicitly register this component with their IpmiInfo type.
pub struct IpmiComponent<P: IpmiInfo>(PhantomData<P>);

impl<P: IpmiInfo> Default for IpmiComponent<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: IpmiInfo> IpmiComponent<P> {
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

struct ArchClock {
    timer: Service<dyn ArchTimerFunctionality>,
    origin: u64,
    frequency: u64,
}

impl ArchClock {
    fn new(timer: Service<dyn ArchTimerFunctionality>) -> Result<Self, Error> {
        let frequency = timer.perf_frequency();
        // The timer service exposes arbitrary counter ranges. Refuse unsupported
        // counters rather than turn a downward-counting clock into false deadlines.
        if frequency == 0 || timer.cpu_count_start() != 0 || timer.cpu_count_end() != u64::MAX {
            return Err(Error::Unsupported);
        }
        let origin = timer.cpu_count();
        Ok(Self { timer, origin, frequency })
    }
}

impl Clock for ArchClock {
    fn now_us(&self) -> u64 {
        let ticks = self.timer.cpu_count().wrapping_sub(self.origin);
        ((u128::from(ticks) * 1_000_000) / u128::from(self.frequency)) as u64
    }
}

fn ensure_no_provider(bs: &impl BootServices) -> Result<(), EfiError> {
    // SAFETY: Locate without constructing a mutable Rust reference to a foreign
    // provider. The interface is never dereferenced.
    match unsafe { bs.locate_protocol_unchecked(&IPMI_PROTOCOL_GUID.0, core::ptr::null_mut()) } {
        Ok(_) => Err(EfiError::AlreadyStarted),
        Err(efi::Status::NOT_FOUND) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[component]
impl<P: IpmiInfo> IpmiComponent<P> {
    fn entry_point(
        self,
        timer: Service<dyn ArchTimerFunctionality>,
        bs: StandardBootServices,
        storage: &mut Storage,
    ) -> Result<(), EfiError> {
        let result = (|| {
            if storage.get_service::<dyn Ipmi>().is_some() {
                return Err(EfiError::AlreadyStarted);
            }
            ensure_no_provider(&bs)?;
            let config = P::config()?;
            let clock = ArchClock::new(timer)?;
            let service = Box::new(IpmiService::discover(config, Box::new(clock))?);
            // Callback and Rust service share a boot-lifetime allocation.
            let service: &'static IpmiService = Box::leak(service);
            register_exit(&bs, service)?;
            storage.add_service(service);
            Ok(())
        })();
        result.inspect_err(|error| log::error!("IPMI component initialization failed: {error:?}"))
    }
}

/// Optional C compatibility adapter; Rust components consume `Service<dyn Ipmi>`.
#[derive(Default)]
pub struct IpmiProtocolInstaller;

#[component]
impl IpmiProtocolInstaller {
    pub const fn new() -> Self {
        Self
    }

    fn entry_point(self, service: Service<dyn Ipmi>, bs: StandardBootServices) -> Result<(), EfiError> {
        let result = (|| {
            ensure_no_provider(&bs)?;
            bs.install_protocol_interface(None, ProtocolWrapper::new(service))?;
            Ok(())
        })();
        result.inspect_err(|error| log::error!("IPMI protocol installation failed: {error:?}"))
    }
}

extern "efiapi" fn stop_on_exit(_event: efi::Event, service: &'static IpmiService) {
    service.stop();
}

fn register_exit(bs: &impl BootServices, service: &'static IpmiService) -> Result<(), EfiError> {
    if let Err(error) = bs.create_event_ex(
        EventType::NOTIFY_SIGNAL,
        Tpl::NOTIFY,
        Some(stop_on_exit),
        service,
        &efi::EVENT_GROUP_EXIT_BOOT_SERVICES,
    ) {
        service.stop();
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use patina::component::service::perf_timer::MockArchTimerFunctionality;
    use patina::uefi::boot_services::MockBootServices;

    #[test]
    fn duplicate_provider_is_rejected_without_dereferencing_it() {
        let mut bs = MockBootServices::new();
        bs.expect_locate_protocol_unchecked().once().returning(|guid, registration| {
            assert_eq!(*guid, IPMI_PROTOCOL_GUID.0);
            assert!(registration.is_null());
            Ok(core::ptr::dangling_mut::<core::ffi::c_void>())
        });
        assert_eq!(ensure_no_provider(&bs), Err(EfiError::AlreadyStarted));
    }

    #[test]
    fn only_not_found_is_accepted_as_absence() {
        for (status, expected) in
            [(efi::Status::NOT_FOUND, Ok(())), (efi::Status::DEVICE_ERROR, Err(EfiError::DeviceError))]
        {
            let mut bs = MockBootServices::new();
            bs.expect_locate_protocol_unchecked().once().returning(move |_, _| Err(status));
            assert_eq!(ensure_no_provider(&bs), expected);
        }
    }

    #[test]
    fn clock_validates_frequency_and_preserves_microseconds() {
        let mut timer = MockArchTimerFunctionality::new();
        timer.expect_perf_frequency().return_const(1_000_000u64);
        timer.expect_cpu_count_start().return_const(0u64);
        timer.expect_cpu_count_end().return_const(u64::MAX);
        timer.expect_cpu_count().times(1).return_const(42u64);
        let clock = ArchClock::new(Service::mock(Box::new(timer))).unwrap();
        assert_eq!(clock.frequency, 1_000_000);
        assert_eq!(clock.origin, 42);
    }

    #[test]
    fn zero_frequency_is_an_explicit_initialization_error() {
        let mut timer = MockArchTimerFunctionality::new();
        timer.expect_perf_frequency().return_const(0u64);
        assert!(matches!(ArchClock::new(Service::mock(Box::new(timer))), Err(Error::Unsupported)));
    }

    #[test]
    fn duplicate_rust_service_is_rejected_before_platform_resources_are_acquired() {
        struct Platform;
        impl IpmiInfo for Platform {
            fn config() -> Result<crate::IpmiConfig, Error> {
                panic!("duplicate provider must not acquire platform resources");
            }
        }
        let mut storage = Storage::new();
        storage.add_service(crate::service::tests::service().0);
        let result = IpmiComponent::<Platform>::new().entry_point(
            Service::mock(Box::new(MockArchTimerFunctionality::new())),
            StandardBootServices::new_uninit(),
            &mut storage,
        );
        assert_eq!(result, Err(EfiError::AlreadyStarted));
    }

    #[test]
    fn exit_boot_services_callback_stops_command_submission() {
        let service = Box::leak(Box::new(crate::service::tests::service().0));
        let mut bs = MockBootServices::new();
        bs.expect_create_event_ex::<&'static IpmiService>().once().returning(|kind, tpl, callback, context, guid| {
            assert_eq!(kind, EventType::NOTIFY_SIGNAL);
            assert_eq!(tpl, Tpl::NOTIFY);
            assert_eq!(*guid, efi::EVENT_GROUP_EXIT_BOOT_SERVICES);
            let event = core::ptr::dangling_mut::<core::ffi::c_void>();
            // SAFETY: register_exit passed a live, boot-lifetime IpmiService
            // as the context for this callback.
            unsafe {
                callback.unwrap()(event, context);
            }
            Ok(event)
        });
        register_exit(&bs, service).unwrap();
        assert_eq!(
            service.submit(crate::Request { netfn: 6, lun: 0, command: 1, data: &[] }, &mut []),
            Err(Error::Stopped)
        );
    }

    #[test]
    fn failed_event_registration_stops_the_unpublished_service() {
        let service = Box::leak(Box::new(crate::service::tests::service().0));
        let mut bs = MockBootServices::new();
        bs.expect_create_event_ex::<&'static IpmiService>()
            .once()
            .returning(|_, _, _, _, _| Err(efi::Status::OUT_OF_RESOURCES));
        assert_eq!(register_exit(&bs, service), Err(EfiError::OutOfResources));
        assert_eq!(
            service.submit(crate::Request { netfn: 6, lun: 0, command: 1, data: &[] }, &mut []),
            Err(Error::Stopped)
        );
    }
}
