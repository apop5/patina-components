//! Serialized Rust IPMI service and configured endpoint discovery.

use alloc::{boxed::Box, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
use patina::{component::service::IntoService, error::EfiError};
use spin::Mutex;

#[cfg(test)]
#[path = "service_tests.rs"]
pub(crate) mod tests;
use crate::{
    MAX_MESSAGE,
    backend::Clock,
    config::{EndpointId, Interface, IpmiConfig},
    transport::{self, Deadline, Transport},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidConfig,
    InvalidRequest,
    RequestTooLarge,
    BufferTooSmall { required: usize },
    NotPresent,
    NotReady,
    NoUsableEndpoint,
    Timeout,
    Device,
    MalformedResponse,
    Unsupported,
    Busy,
    Stopped,
    Completion(u8),
}

impl From<Error> for EfiError {
    fn from(error: Error) -> Self {
        match error {
            Error::InvalidConfig | Error::InvalidRequest => Self::InvalidParameter,
            Error::RequestTooLarge => Self::BadBufferSize,
            Error::BufferTooSmall { .. } => Self::BufferTooSmall,
            Error::NotPresent | Error::NoUsableEndpoint => Self::NotFound,
            Error::NotReady | Error::Busy => Self::NotReady,
            Error::Timeout => Self::Timeout,
            Error::Unsupported | Error::Stopped => Self::Unsupported,
            Error::Device | Error::MalformedResponse | Error::Completion(_) => Self::DeviceError,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    Available,
    Unavailable(Error),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndpointStatus {
    pub id: EndpointId,
    pub interface: Interface,
    pub availability: Availability,
}

/// NetFn is unshifted and must be an even request NetFn. LUN is 0..=3.
pub struct Request<'a> {
    pub netfn: u8,
    pub lun: u8,
    pub command: u8,
    pub data: &'a [u8],
}

/// Completion codes are BMC results, not transport errors. The caller's buffer
/// contains only the payload, excluding the completion code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Response {
    pub completion_code: u8,
    pub data_len: usize,
}

pub trait Ipmi: Send + Sync {
    fn submit(&self, request: Request<'_>, response: &mut [u8]) -> Result<Response, Error>;
    fn endpoints(&self) -> &[EndpointStatus];
    fn selected(&self) -> EndpointStatus;
}

#[derive(IntoService)]
#[service(dyn Ipmi)]
pub struct IpmiService {
    transport: Mutex<Box<dyn Transport>>,
    clock: Box<dyn Clock>,
    endpoints: Vec<EndpointStatus>,
    selected: usize,
    transaction_us: u64,
    stopped: AtomicBool,
    faulted: AtomicBool,
}

impl IpmiService {
    /// Probes every supplied candidate, then selects only from the preference
    /// list. Initialization has one deadline per endpoint, shared by setup and
    /// probing. No request is retried on another interface.
    pub fn discover(config: IpmiConfig, clock: Box<dyn Clock>) -> Result<Self, Error> {
        Self::discover_inner(config, clock).inspect_err(|error| log::error!("IPMI discovery failed: {error:?}"))
    }

    fn discover_inner(config: IpmiConfig, clock: Box<dyn Clock>) -> Result<Self, Error> {
        config.validate()?;
        let mut endpoints = Vec::with_capacity(config.endpoints.len());
        let mut transports = Vec::with_capacity(config.endpoints.len());
        for endpoint in config.endpoints {
            let id = endpoint.id();
            let interface = endpoint.interface();
            let mut transport = transport::create(endpoint);
            let deadline = Deadline::new(&*clock, config.timeouts.initialization_us, None);
            let result = (|| {
                transport.initialize(&deadline)?;
                let request = [0x18, 1];
                let mut response = [0; MAX_MESSAGE];
                let length = transport.exchange(&request, &mut response, &deadline)?;
                transport::validate_response(&request, &response[..length])?;
                if response[2] != 0 {
                    return Err(Error::Completion(response[2]));
                }
                if length < 14 {
                    return Err(Error::MalformedResponse);
                }
                if response[5] & 0x80 != 0 {
                    return Err(Error::NotReady);
                }
                deadline.remaining()?;
                Ok(())
            })();
            let availability = match result {
                Ok(()) => Availability::Available,
                Err(error) => {
                    log::warn!("IPMI endpoint {id} ({interface:?}) unavailable: {error:?}");
                    Availability::Unavailable(error)
                }
            };
            endpoints.push(EndpointStatus { id, interface, availability });
            transports.push(Some(transport));
        }
        let selected = config
            .preference
            .iter()
            .find_map(|id| endpoints.iter().position(|e| e.id == *id && e.availability == Availability::Available))
            .ok_or(Error::NoUsableEndpoint)?;
        let transport = transports[selected].take().ok_or(Error::NoUsableEndpoint)?;
        log::info!("IPMI selected endpoint {} ({:?})", endpoints[selected].id, endpoints[selected].interface);
        Ok(Self {
            transport: Mutex::new(transport),
            clock,
            endpoints,
            selected,
            transaction_us: config.timeouts.transaction_us,
            stopped: AtomicBool::new(false),
            faulted: AtomicBool::new(false),
        })
    }

    /// Stops new submissions and cancels polling at the next deadline check.
    /// The platform must not invoke firmware services from APs during EBS.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }

    fn submit_inner(&self, request: Request<'_>, response: &mut [u8]) -> Result<Response, Error> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(Error::Stopped);
        }
        if request.netfn > 0x3e || request.netfn & 1 != 0 || request.lun > 3 {
            return Err(Error::InvalidRequest);
        }
        if request.data.len() > MAX_MESSAGE - 2 {
            return Err(Error::RequestTooLarge);
        }
        // Never spin on a lock held by an interrupted caller.
        let mut transport = self.transport.try_lock().ok_or(Error::Busy)?;
        if self.faulted.load(Ordering::Acquire) {
            return Err(Error::NotReady);
        }
        let deadline = Deadline::new(&*self.clock, self.transaction_us, Some(&self.stopped));
        let mut frame = [0; MAX_MESSAGE];
        frame[0] = request.netfn << 2 | request.lun;
        frame[1] = request.command;
        frame[2..request.data.len() + 2].copy_from_slice(request.data);
        let mut received = [0; MAX_MESSAGE];
        let result = (|| {
            let length = transport.exchange(&frame[..request.data.len() + 2], &mut received, &deadline)?;
            deadline.remaining()?;
            transport::validate_response(&frame[..2], &received[..length])?;
            Ok(length)
        })();
        let length = result.inspect_err(|error| {
            // After an uncertain transaction, an unsequenced interface such as
            // SSIF could return a late response to a later identical command.
            // Fail closed instead of silently associating it with a new request.
            if *error != Error::RequestTooLarge {
                self.faulted.store(true, Ordering::Release);
            }
        })?;
        let required = length - 3;
        if required > response.len() {
            return Err(Error::BufferTooSmall { required });
        }
        response[..required].copy_from_slice(&received[3..length]);
        Ok(Response { completion_code: received[2], data_len: required })
    }
}

impl Ipmi for IpmiService {
    fn submit(&self, request: Request<'_>, response: &mut [u8]) -> Result<Response, Error> {
        self.submit_inner(request, response).inspect_err(|error| log::error!("IPMI command failed: {error:?}"))
    }

    fn endpoints(&self) -> &[EndpointStatus] {
        &self.endpoints
    }
    fn selected(&self) -> EndpointStatus {
        self.endpoints[self.selected]
    }
}
