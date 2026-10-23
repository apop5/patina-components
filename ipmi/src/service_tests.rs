use super::*;
use crate::{
    backend::Smbus,
    config::{Endpoint, Timeouts},
};
use alloc::{sync::Arc, vec, vec::Vec};
use core::sync::atomic::AtomicU64;

struct TestClock(AtomicU64);
impl Clock for TestClock {
    fn now_us(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

struct Device {
    fail: Option<Error>,
    device_len: usize,
    update_mode: bool,
    command: u8,
    calls: Arc<AtomicU64>,
}

impl Smbus for Device {
    fn write_block(&mut self, _: u8, command: u8, _: bool, data: &[u8], _: u64) -> Result<(), Error> {
        assert_eq!(command, 2);
        self.calls.fetch_add(1, Ordering::Relaxed);
        if let Some(error) = self.fail {
            return Err(error);
        }
        self.command = data[1];
        Ok(())
    }
    fn read_block(&mut self, _: u8, command: u8, _: bool, output: &mut [u8; 32], _: u64) -> Result<usize, Error> {
        assert_eq!(command, 3);
        output.fill(0);
        output[0] = 0x1c;
        output[1] = self.command;
        match self.command {
            0x57 => {
                output[5] = 32;
                output[6] = 32;
                Ok(7)
            }
            1 => {
                if self.update_mode {
                    output[5] = 0x80;
                }
                Ok(self.device_len)
            }
            _ => {
                output[2] = 0xc1;
                Ok(3)
            }
        }
    }
}

fn endpoint(id: &'static str, fail: Option<Error>, calls: Arc<AtomicU64>) -> Endpoint {
    Endpoint::Ssif {
        id,
        address: 0x20,
        bus: Box::new(Device { fail, device_len: 14, update_mode: false, command: 0, calls }),
    }
}

fn config(endpoints: Vec<Endpoint>, preference: Vec<EndpointId>) -> IpmiConfig {
    IpmiConfig { endpoints, preference, timeouts: Timeouts { initialization_us: 1000, transaction_us: 1000 } }
}

fn clock() -> Box<dyn Clock> {
    Box::new(TestClock(AtomicU64::new(0)))
}

#[test]
fn discovers_all_candidates_and_uses_preference_not_candidate_order() {
    let first = Arc::new(AtomicU64::new(0));
    let second = Arc::new(AtomicU64::new(0));
    let service = IpmiService::discover(
        config(
            vec![endpoint("first", None, first.clone()), endpoint("second", None, second.clone())],
            vec!["second", "first"],
        ),
        clock(),
    )
    .unwrap();
    assert_eq!(service.selected().id, "second");
    assert_eq!(service.endpoints().len(), 2);
    assert!(service.endpoints().iter().all(|e| e.availability == Availability::Available));
    assert_eq!(first.load(Ordering::Relaxed), 2);
    assert_eq!(second.load(Ordering::Relaxed), 2);
}

#[test]
fn falls_back_only_to_an_explicitly_allowed_endpoint() {
    let calls = Arc::new(AtomicU64::new(0));
    let service = IpmiService::discover(
        config(
            vec![endpoint("absent", Some(Error::NotPresent), calls.clone()), endpoint("backup", None, calls)],
            vec!["absent", "backup"],
        ),
        clock(),
    )
    .unwrap();
    assert_eq!(service.selected().id, "backup");
    assert_eq!(service.endpoints()[0].availability, Availability::Unavailable(Error::NotPresent));
}

#[test]
fn available_unlisted_endpoint_is_never_selected() {
    let calls = Arc::new(AtomicU64::new(0));
    let result = IpmiService::discover(
        config(
            vec![endpoint("absent", Some(Error::NotPresent), calls.clone()), endpoint("unlisted", None, calls)],
            vec!["absent"],
        ),
        clock(),
    );
    assert!(matches!(result, Err(Error::NoUsableEndpoint)));
}

#[test]
fn rejects_duplicate_ids_preferences_unknown_ids_and_empty_config() {
    let calls = Arc::new(AtomicU64::new(0));
    let invalid = [
        config(vec![endpoint("same", None, calls.clone()), endpoint("same", None, calls.clone())], vec!["same"]),
        config(vec![endpoint("a", None, calls.clone())], vec!["a", "a"]),
        config(vec![endpoint("a", None, calls.clone())], vec!["missing"]),
        config(vec![], vec![]),
        config(vec![endpoint("", None, calls.clone())], vec![""]),
    ];
    for config in invalid {
        assert_eq!(config.validate(), Err(Error::InvalidConfig));
    }
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[test]
fn rejects_zero_timeouts_and_reserved_bus_addresses() {
    let calls = Arc::new(AtomicU64::new(0));
    let mut cfg = config(vec![endpoint("a", None, calls)], vec!["a"]);
    cfg.timeouts.transaction_us = 0;
    assert_eq!(cfg.validate(), Err(Error::InvalidConfig));
    cfg.timeouts.transaction_us = 100;
    if let Endpoint::Ssif { address, .. } = &mut cfg.endpoints[0] {
        *address = 0x80;
    }
    assert_eq!(cfg.validate(), Err(Error::InvalidConfig));
}

#[test]
fn discovery_rejects_short_device_id_and_firmware_update_mode() {
    for (device_len, update_mode, expected) in [(3, false, Error::MalformedResponse), (14, true, Error::NotReady)] {
        let calls = Arc::new(AtomicU64::new(0));
        let endpoint = Endpoint::Ssif {
            id: "a",
            address: 0x20,
            bus: Box::new(Device { fail: None, device_len, update_mode, command: 0, calls: calls.clone() }),
        };
        let service =
            IpmiService::discover(config(vec![endpoint, self::endpoint("b", None, calls)], vec!["a", "b"]), clock())
                .unwrap();
        assert_eq!(service.endpoints()[0].availability, Availability::Unavailable(expected));
        assert_eq!(service.selected().id, "b");
    }
}

pub(crate) fn service() -> (IpmiService, Arc<AtomicU64>) {
    let calls = Arc::new(AtomicU64::new(0));
    (IpmiService::discover(config(vec![endpoint("a", None, calls.clone())], vec!["a"]), clock()).unwrap(), calls)
}

#[test]
fn completion_code_is_not_a_transport_error() {
    let (service, calls) = service();
    let result = service.submit(Request { netfn: 6, lun: 0, command: 0xff, data: &[] }, &mut []).unwrap();
    assert_eq!(result, Response { completion_code: 0xc1, data_len: 0 });
    assert_eq!(calls.load(Ordering::Relaxed), 3);
}

#[test]
fn reports_required_payload_size_without_replaying_the_command() {
    let (service, calls) = service();
    assert_eq!(
        service.submit(Request { netfn: 6, lun: 0, command: 1, data: &[] }, &mut [0; 1]),
        Err(Error::BufferTooSmall { required: 11 })
    );
    assert_eq!(calls.load(Ordering::Relaxed), 3);
}

#[test]
fn invalid_requests_do_not_access_hardware() {
    let (service, calls) = service();
    for (netfn, lun) in [(7, 0), (64, 0), (6, 4)] {
        assert_eq!(service.submit(Request { netfn, lun, command: 1, data: &[] }, &mut []), Err(Error::InvalidRequest));
    }
    assert_eq!(
        service.submit(Request { netfn: 6, lun: 0, command: 1, data: &[0; 254] }, &mut []),
        Err(Error::RequestTooLarge)
    );
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

#[test]
fn shutdown_and_lock_contention_never_touch_the_bus() {
    let (service, calls) = service();
    let guard = service.transport.lock();
    assert_eq!(service.submit(Request { netfn: 6, lun: 0, command: 1, data: &[] }, &mut []), Err(Error::Busy));
    drop(guard);
    service.stop();
    assert_eq!(service.submit(Request { netfn: 6, lun: 0, command: 1, data: &[] }, &mut []), Err(Error::Stopped));
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

struct FailingTransport(Arc<AtomicU64>);
impl Transport for FailingTransport {
    fn exchange(&mut self, _: &[u8], _: &mut [u8; MAX_MESSAGE], _: &Deadline<'_>) -> Result<usize, Error> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Err(Error::Timeout)
    }
}

#[test]
fn transaction_failure_never_reselects_or_replays() {
    let (mut service, calls) = service();
    service.transport = Mutex::new(Box::new(FailingTransport(calls.clone())));
    assert_eq!(service.submit(Request { netfn: 6, lun: 0, command: 2, data: &[] }, &mut []), Err(Error::Timeout));
    assert_eq!(service.selected().id, "a");
    assert_eq!(calls.load(Ordering::Relaxed), 3);
    assert_eq!(service.submit(Request { netfn: 6, lun: 0, command: 2, data: &[] }, &mut []), Err(Error::NotReady));
    assert_eq!(calls.load(Ordering::Relaxed), 3);
}
