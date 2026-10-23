use super::*;
use crate::backend::{IpmbBus, Smbus};
use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

struct TestClock(AtomicU64);

impl TestClock {
    fn new() -> Self {
        Self(AtomicU64::new(0))
    }
}

impl Clock for TestClock {
    fn now_us(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

#[derive(Debug)]
enum Op {
    Read(u8, u8),
    Write(u8, u8),
}

struct Script(Arc<Mutex<VecDeque<Op>>>);

impl Registers for Script {
    fn read(&mut self, register: u8) -> Result<u8, Error> {
        match self.0.lock().pop_front().expect("unexpected register read") {
            Op::Read(expected, value) => {
                assert_eq!(register, expected);
                Ok(value)
            }
            op => panic!("expected {op:?}, received read({register})"),
        }
    }
    fn write(&mut self, register: u8, value: u8) -> Result<(), Error> {
        match self.0.lock().pop_front().expect("unexpected register write") {
            Op::Write(expected, byte) => {
                assert_eq!((register, value), (expected, byte));
                Ok(())
            }
            op => panic!("expected {op:?}, received write({register}, {value})"),
        }
    }
}

fn script(ops: Vec<Op>) -> (Box<dyn Registers>, Arc<Mutex<VecDeque<Op>>>) {
    let queue = Arc::new(Mutex::new(ops.into()));
    (Box::new(Script(queue.clone())), queue)
}

fn kcs_script(request: &[u8], response: &[u8]) -> Vec<Op> {
    let mut ops = vec![Op::Read(1, 0), Op::Write(1, 0x61), Op::Read(1, 0x80)];
    for byte in &request[..request.len() - 1] {
        ops.extend([Op::Write(0, *byte), Op::Read(1, 0x80)]);
    }
    ops.extend([Op::Write(1, 0x62), Op::Read(1, 0x80), Op::Write(0, request[request.len() - 1])]);
    for byte in response {
        ops.extend([Op::Read(1, 0x41), Op::Read(1, 0x41), Op::Read(0, *byte), Op::Write(0, 0x68)]);
    }
    ops.extend([Op::Read(1, 1), Op::Read(1, 1), Op::Read(0, 0)]);
    ops
}

#[test]
fn kcs_handshake_and_response() {
    let request = [0x18, 1, 0xaa];
    let expected = [0x1c, 1, 0, 0x55];
    let (registers, queue) = script(kcs_script(&request, &expected));
    let mut transport = kcs::Kcs(registers);
    let clock = TestClock::new();
    let mut response = [0; MAX_MESSAGE];
    let length = transport.exchange(&request, &mut response, &Deadline::new(&clock, 1000, None)).unwrap();
    assert_eq!(&response[..length], &expected);
    assert!(queue.lock().is_empty());
}

#[test]
fn kcs_recovers_a_stale_transaction_without_resetting_bmc() {
    let mut ops = vec![
        Op::Read(1, 0xc0),
        Op::Write(1, 0x60),
        Op::Read(1, 1),
        Op::Read(0, 0),
        Op::Write(0, 0),
        Op::Read(1, 0x41),
        Op::Read(1, 0x41),
        Op::Read(0, 1),
        Op::Write(0, 0x68),
        Op::Read(1, 1),
        Op::Read(1, 1),
        Op::Read(0, 0),
    ];
    let (registers, queue) = script(core::mem::take(&mut ops));
    let clock = TestClock::new();
    kcs::Kcs(registers).initialize(&Deadline::new(&clock, 1000, None)).unwrap();
    assert!(queue.lock().is_empty());
}

struct ConstantRegisters(u8);
impl Registers for ConstantRegisters {
    fn read(&mut self, _: u8) -> Result<u8, Error> {
        Ok(self.0)
    }
    fn write(&mut self, _: u8, _: u8) -> Result<(), Error> {
        panic!("unexpected write")
    }
}

#[test]
fn absent_registers_are_not_treated_as_ready() {
    let clock = TestClock::new();
    assert_eq!(
        kcs::Kcs(Box::new(ConstantRegisters(0xff))).initialize(&Deadline::new(&clock, 10, None)),
        Err(Error::NotPresent)
    );
}

#[test]
fn stuck_input_buffer_has_a_single_bounded_deadline() {
    let clock = TestClock::new();
    assert_eq!(
        kcs::Kcs(Box::new(ConstantRegisters(2))).initialize(&Deadline::new(&clock, 10, None)),
        Err(Error::Timeout)
    );
    assert!(clock.0.load(Ordering::Relaxed) <= 13);
}

#[test]
fn deadlines_support_counter_wrap_and_cancellation() {
    let clock = TestClock(AtomicU64::new(u64::MAX - 3));
    let stopped = AtomicBool::new(false);
    let deadline = Deadline::new(&clock, 10, Some(&stopped));
    for _ in 0..8 {
        assert!(deadline.remaining().is_ok());
    }
    stopped.store(true, Ordering::Release);
    assert_eq!(deadline.remaining(), Err(Error::Stopped));
    stopped.store(false, Ordering::Release);
    assert!(deadline.remaining().is_ok());
    assert_eq!(deadline.remaining(), Err(Error::Timeout));
}

fn smic_control(ops: &mut Vec<Op>, code: u8, byte: Option<u8>, status: u8, flags: u8) {
    ops.extend([Op::Read(2, flags), Op::Write(1, code)]);
    if let Some(byte) = byte {
        ops.push(Op::Write(0, byte));
    }
    ops.extend([Op::Write(2, flags | 1), Op::Read(2, 0), Op::Read(1, status)]);
}

#[test]
fn smic_control_data_and_end_handshakes() {
    let mut ops = Vec::new();
    smic_control(&mut ops, 0x40, None, 0xc0, 0);
    smic_control(&mut ops, 0x41, Some(0x18), 0xc1, 0);
    ops.push(Op::Read(2, 0x40));
    smic_control(&mut ops, 0x43, Some(1), 0xc3, 0x40);
    ops.extend([Op::Read(0, 0), Op::Read(2, 0x80)]);
    smic_control(&mut ops, 0x44, None, 0xc4, 0x80);
    ops.extend([Op::Read(2, 0x80), Op::Read(0, 0x1c)]);
    smic_control(&mut ops, 0x45, None, 0xc5, 0x80);
    ops.extend([Op::Read(2, 0x80), Op::Read(0, 1)]);
    smic_control(&mut ops, 0x45, None, 0xc6, 0x80);
    ops.push(Op::Read(0, 0));
    smic_control(&mut ops, 0x46, None, 0xc0, 0);
    let (registers, queue) = script(ops);
    let mut transport = smic::Smic(registers);
    let clock = TestClock::new();
    let mut response = [0; MAX_MESSAGE];
    let length = transport.exchange(&[0x18, 1], &mut response, &Deadline::new(&clock, 1000, None)).unwrap();
    assert_eq!(&response[..length], &[0x1c, 1, 0]);
    assert!(queue.lock().is_empty());
}

fn bt_script(sequence: u8, request: &[u8], response: &[u8]) -> Vec<Op> {
    let mut ops = vec![
        Op::Read(0, 0),
        Op::Write(0, 1),
        Op::Write(1, (request.len() + 1) as u8),
        Op::Write(1, request[0]),
        Op::Write(1, sequence),
    ];
    for byte in &request[1..] {
        ops.push(Op::Write(1, *byte));
    }
    ops.extend([
        Op::Write(0, 4),
        Op::Read(0, 8),
        Op::Write(0, 0x40),
        Op::Write(0, 8),
        Op::Write(0, 2),
        Op::Read(1, (response.len() + 1) as u8),
        Op::Read(1, response[0]),
        Op::Read(1, sequence),
    ]);
    for byte in &response[1..] {
        ops.push(Op::Read(1, *byte));
    }
    ops.push(Op::Write(0, 0x40));
    ops
}

#[test]
fn bt_length_sequence_and_host_busy_release() {
    let expected = [0x1c, 1, 0, 0x55];
    let (registers, queue) = script(bt_script(0, &[0x18, 1], &expected));
    let mut transport = bt::Bt::new(registers);
    let clock = TestClock::new();
    let mut response = [0; MAX_MESSAGE];
    let length = transport.exchange(&[0x18, 1], &mut response, &Deadline::new(&clock, 1000, None)).unwrap();
    assert_eq!(&response[..length], &expected);
    assert!(queue.lock().is_empty());
}

#[test]
fn bt_capabilities_set_the_message_limit() {
    let mut ops = vec![Op::Read(0, 0)];
    ops.extend(bt_script(0, &[0x18, 0x36], &[0x1c, 0x36, 0, 1, 64, 64, 1, 0]));
    let (registers, queue) = script(ops);
    let mut transport = bt::Bt::new(registers);
    let clock = TestClock::new();
    transport.initialize(&Deadline::new(&clock, 1000, None)).unwrap();
    let mut response = [0; MAX_MESSAGE];
    assert_eq!(
        transport.exchange(&[0; 63], &mut response, &Deadline::new(&clock, 1000, None)),
        Err(Error::RequestTooLarge)
    );
    assert!(queue.lock().is_empty());
}

#[test]
fn bt_releases_host_busy_after_malformed_response() {
    let mut ops = bt_script(0, &[0x18, 1], &[0x1c, 1, 0]);
    let index = ops.iter().position(|op| matches!(op, Op::Read(1, 4))).unwrap();
    ops.truncate(index);
    ops.extend([Op::Read(1, 0), Op::Write(0, 0x40)]);
    let (registers, queue) = script(ops);
    let clock = TestClock::new();
    let mut response = [0; MAX_MESSAGE];
    assert_eq!(
        bt::Bt::new(registers).exchange(&[0x18, 1], &mut response, &Deadline::new(&clock, 1000, None)),
        Err(Error::MalformedResponse)
    );
    assert!(queue.lock().is_empty());
}

#[derive(Debug)]
pub(crate) enum BusOp {
    Write(u8, bool, Vec<u8>),
    Read(u8, bool, Result<Vec<u8>, Error>),
}

pub(crate) struct ScriptBus(pub Arc<Mutex<VecDeque<BusOp>>>);
impl Smbus for ScriptBus {
    fn write_block(&mut self, address: u8, command: u8, pec: bool, data: &[u8], timeout: u64) -> Result<(), Error> {
        assert_eq!(address, 0x20);
        assert!(timeout > 0);
        match self.0.lock().pop_front().expect("unexpected SMBus write") {
            BusOp::Write(expected, expected_pec, bytes) => {
                assert_eq!((command, pec, data), (expected, expected_pec, bytes.as_slice()));
                Ok(())
            }
            op => panic!("expected {op:?}, received SMBus write"),
        }
    }
    fn read_block(
        &mut self,
        address: u8,
        command: u8,
        pec: bool,
        data: &mut [u8; 32],
        timeout: u64,
    ) -> Result<usize, Error> {
        assert_eq!(address, 0x20);
        assert!(timeout > 0);
        match self.0.lock().pop_front().expect("unexpected SMBus read") {
            BusOp::Read(expected, expected_pec, result) => {
                assert_eq!((command, pec), (expected, expected_pec));
                let bytes = result?;
                data[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            op => panic!("expected {op:?}, received SMBus read"),
        }
    }
}

fn ssif_with_caps(
    capabilities: u8,
    input: u8,
    output: u8,
    mut ops: Vec<BusOp>,
) -> (ssif::Ssif, Arc<Mutex<VecDeque<BusOp>>>) {
    let mut all = vec![
        BusOp::Write(2, false, vec![0x18, 0x57, 0]),
        BusOp::Read(3, false, Ok(vec![0x1c, 0x57, 0, 0, capabilities, input, output])),
    ];
    all.append(&mut ops);
    let queue = Arc::new(Mutex::new(all.into()));
    let mut transport = ssif::Ssif::new(Box::new(ScriptBus(queue.clone())), 0x20);
    let clock = TestClock::new();
    transport.initialize(&Deadline::new(&clock, 1000, None)).unwrap();
    (transport, queue)
}

#[test]
fn ssif_multipart_and_pec_are_negotiated_and_framed() {
    let request = [0x18; 70];
    let mut first = vec![0, 1, 0x1c, 0x18, 0];
    first.resize(32, 0xaa);
    let mut middle = vec![0];
    middle.resize(32, 0xbb);
    let ops = vec![
        BusOp::Write(6, true, request[..32].to_vec()),
        BusOp::Write(7, true, request[32..64].to_vec()),
        BusOp::Write(8, true, request[64..].to_vec()),
        BusOp::Read(3, true, Err(Error::NotReady)),
        BusOp::Read(3, true, Ok(first)),
        BusOp::Read(9, true, Ok(middle)),
        BusOp::Read(9, true, Ok(vec![0xff, 0xcc, 0xdd])),
    ];
    let (mut transport, queue) = ssif_with_caps(0x88, 255, 255, ops);
    let clock = TestClock::new();
    let mut response = [0; MAX_MESSAGE];
    let length = transport.exchange(&request, &mut response, &Deadline::new(&clock, 1000, None)).unwrap();
    assert_eq!(length, 63);
    assert_eq!(&response[..3], &[0x1c, 0x18, 0]);
    assert_eq!(&response[61..63], &[0xcc, 0xdd]);
    assert!(queue.lock().is_empty());
}

#[test]
fn ssif_rejects_out_of_order_multipart_block() {
    let mut first = vec![0, 1];
    first.resize(32, 0);
    let mut middle = vec![1];
    middle.resize(32, 0);
    let (mut transport, queue) = ssif_with_caps(
        0x80,
        255,
        255,
        vec![
            BusOp::Write(2, false, vec![0x18, 1]),
            BusOp::Read(3, false, Ok(first)),
            BusOp::Read(9, false, Ok(middle)),
        ],
    );
    let clock = TestClock::new();
    assert_eq!(
        transport.exchange(&[0x18, 1], &mut [0; MAX_MESSAGE], &Deadline::new(&clock, 1000, None)),
        Err(Error::MalformedResponse)
    );
    assert!(queue.lock().is_empty());
}

#[test]
fn ssif_single_part_limit_is_enforced_before_bus_access() {
    let (mut transport, queue) = ssif_with_caps(0, 32, 32, vec![]);
    let clock = TestClock::new();
    assert_eq!(
        transport.exchange(&[0; 33], &mut [0; MAX_MESSAGE], &Deadline::new(&clock, 1000, None)),
        Err(Error::RequestTooLarge)
    );
    assert!(queue.lock().is_empty());
}

#[test]
fn ssif_exact_multiple_of_block_size_ends_with_command_eight() {
    let request = [0x18; 64];
    let (mut transport, queue) = ssif_with_caps(
        0x40,
        64,
        64,
        vec![
            BusOp::Write(6, false, request[..32].to_vec()),
            BusOp::Write(8, false, request[32..].to_vec()),
            BusOp::Read(3, false, Ok(vec![0x1c, 0x18, 0])),
        ],
    );
    let clock = TestClock::new();
    assert_eq!(transport.exchange(&request, &mut [0; MAX_MESSAGE], &Deadline::new(&clock, 1000, None)), Ok(3));
    assert!(queue.lock().is_empty());
}

#[test]
fn ssif_maximum_message_does_not_wrap_offsets() {
    let request = [0x18; MAX_MESSAGE];
    let mut ops = vec![BusOp::Write(6, false, request[..32].to_vec())];
    for chunk in request[32..224].chunks(32) {
        ops.push(BusOp::Write(7, false, chunk.to_vec()));
    }
    ops.push(BusOp::Write(8, false, request[224..].to_vec()));
    let mut first = vec![0, 1, 0x1c, 0x18, 0];
    first.resize(32, 0xaa);
    ops.push(BusOp::Read(3, false, Ok(first)));
    for sequence in 0..7 {
        let mut block = vec![sequence];
        block.resize(32, 0xbb);
        ops.push(BusOp::Read(9, false, Ok(block)));
    }
    ops.push(BusOp::Read(9, false, Ok(vec![0xff; 9])));
    let (mut transport, queue) = ssif_with_caps(0x80, 255, 255, ops);
    let clock = TestClock::new();
    assert_eq!(transport.exchange(&request, &mut [0; MAX_MESSAGE], &Deadline::new(&clock, 1000, None)), Ok(255));
    assert!(queue.lock().is_empty());
}

struct IpmbEcho {
    frame: Vec<u8>,
    bad_checksum: bool,
    stale: bool,
}
impl IpmbBus for IpmbEcho {
    fn send(&mut self, frame: &[u8], timeout: u64) -> Result<(), Error> {
        assert!(timeout > 0);
        assert_eq!(frame.iter().take(3).fold(0u8, |sum, b| sum.wrapping_add(*b)), 0);
        assert_eq!(frame[3..].iter().fold(0u8, |sum, b| sum.wrapping_add(*b)), 0);
        assert_eq!((frame[0], frame[3]), (0x40, 0x82));
        self.frame = frame.to_vec();
        Ok(())
    }
    fn receive(&mut self, output: &mut [u8], timeout: u64) -> Result<usize, Error> {
        assert!(timeout > 0);
        let r = &self.frame;
        let mut response = vec![r[3], (r[1] & 0xfc) | 4, 0, r[0], r[4] | (r[1] & 3), r[5], 0, 0x55, 0];
        if self.stale {
            self.stale = false;
            response[4] = response[4].wrapping_add(4);
        }
        response[2] = 0u8.wrapping_sub(response[0].wrapping_add(response[1]));
        response[8] = 0u8.wrapping_sub(response[3..8].iter().fold(0u8, |s, b| s.wrapping_add(*b)));
        if self.bad_checksum {
            response[8] ^= 1;
        }
        output[..response.len()].copy_from_slice(&response);
        Ok(response.len())
    }
}

#[test]
fn ipmb_checksums_sequence_luns_and_stale_responses() {
    let mut transport =
        ipmb::Ipmb::new(Box::new(IpmbEcho { frame: vec![], bad_checksum: false, stale: true }), 0x20, 0x41);
    let clock = TestClock::new();
    let mut response = [0; MAX_MESSAGE];
    let length = transport.exchange(&[0x1a, 1], &mut response, &Deadline::new(&clock, 1000, None)).unwrap();
    assert_eq!(&response[..length], &[0x1e, 1, 0, 0x55]);
}

#[test]
fn ipmb_rejects_corrupt_response() {
    let mut transport =
        ipmb::Ipmb::new(Box::new(IpmbEcho { frame: vec![], bad_checksum: true, stale: false }), 0x20, 0x41);
    let clock = TestClock::new();
    assert_eq!(
        transport.exchange(&[0x18, 1], &mut [0; MAX_MESSAGE], &Deadline::new(&clock, 1000, None)),
        Err(Error::MalformedResponse)
    );
}

#[test]
fn common_response_validation_rejects_wrong_netfn_command_and_short_data() {
    for response in [&[0x18, 1, 0][..], &[0x1c, 2, 0], &[0x1c, 1]] {
        assert_eq!(validate_response(&[0x18, 1], response), Err(Error::MalformedResponse));
    }
    assert_eq!(validate_response(&[0x18, 1], &[0x1c, 1, 0xc1]), Ok(()));
}
