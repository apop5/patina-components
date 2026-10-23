# Patina IPMI

`patina_ipmi` is a `no_std`, boot-services-only IPMI component. It implements KCS, SMIC, BT, SSIF, and direct IPMB,
publishes a Rust `Ipmi` service, and optionally installs EDK II's `MdeModulePkg` `IPMI_PROTOCOL`.

This is not the distinct Intel `IpmiTransportProtocol` or an SMM provider. LAN, RMCP/RMCP+, serial/modem,
ACPI/SMBIOS discovery, and OS-facing ACPI/SMBIOS table generation are outside its scope.

## Platform integration

Implement `IpmiInfo` and explicitly register `IpmiComponent::<Platform>::new()` in the platform's component list.
Register `IpmiProtocolInstaller::new()` as well when C drivers need `IPMI_PROTOCOL`. Both components implement Patina's
`IntoComponent` trait. Configuration follows the platform-owned, opt-in model of Patina's `CpuInfo`; it requires
neither changes to DXE Core nor a PEI IPMI HOB.

### KCS example

```rust,no_run
extern crate alloc;
use alloc::{boxed::Box, vec};
use patina::component::IntoComponent;
use patina_ipmi::{
    Error, Endpoint, IpmiComponent, IpmiConfig, IpmiInfo, IpmiProtocolInstaller, Timeouts,
    backend::{AccessWidth, AddressSpace, RegisterWindow},
};

struct Platform;

impl IpmiInfo for Platform {
    fn config() -> Result<IpmiConfig, Error> {
        // Example only: the platform must establish ownership and I/O decode.
        // SAFETY: On this example x64 platform, KCS owns ports 0xca2 and 0xca3,
        // byte accesses are supported, and no other agent accesses this window.
        let registers = unsafe {
            RegisterWindow::new(0xca2, 1, 2, AccessWidth::Byte, AddressSpace::IoPort)?
        };
        Ok(IpmiConfig {
            endpoints: vec![Endpoint::Kcs { id: "bmc-kcs", registers: Box::new(registers) }],
            preference: vec!["bmc-kcs"],
            timeouts: Timeouts::default(),
        })
    }
}

let provider = IpmiComponent::<Platform>::new().into_component();
let c_adapter = IpmiProtocolInstaller::new().into_component();
// Add provider and c_adapter to the platform's ComponentInfo::components list.
# let _ = (provider, c_adapter);
```

`IpmiInfo::config()` transfers ownership of endpoint resources and is called once when the registered component
dispatches. Register exactly one provider. Disable other DXE IPMI providers before enabling this component.
An existing Rust `Ipmi` service or `IPMI_PROTOCOL` causes initialization to fail before hardware probing. Platforms must also prevent
unadvertised access by other Rust components, SMM drivers, or other agents.

### Multiple KCS windows and non-adjacent registers

Each candidate window is a separate endpoint. Use `RegisterWindow::from_addresses` to provide the address of each
logical register independently, including descending or nonuniform mappings. For KCS the order is
`[data_address, status_command_address]`; no `data + 1` relationship is assumed.

```rust,no_run
extern crate alloc;
use alloc::{boxed::Box, vec, vec::Vec};
use patina_ipmi::{
    Error, Endpoint, IpmiConfig, IpmiInfo, Timeouts,
    backend::{AccessWidth, AddressSpace, RegisterWindow},
};

struct MultiWindowPlatform;

impl IpmiInfo for MultiWindowPlatform {
    fn config() -> Result<IpmiConfig, Error> {
        let mut endpoints = Vec::new();
        for (id, data, status_command) in [
            ("kcs-0", 0xca2, 0xca3),
            ("kcs-1", 0xca4, 0xca5),
            ("kcs-2", 0xca6, 0xca7),
            ("kcs-3", 0xca8, 0xca1),
        ] {
            // SAFETY: On this example x64 platform, every listed register is
            // decoded, exclusively owned, and supports byte accesses.
            let registers = unsafe {
                RegisterWindow::from_addresses(
                    &[data, status_command], AccessWidth::Byte, AddressSpace::IoPort,
                )?
            };
            endpoints.push(Endpoint::Kcs { id, registers: Box::new(registers) });
        }
        Ok(IpmiConfig {
            endpoints,
            preference: vec!["kcs-0", "kcs-1", "kcs-2", "kcs-3"],
            timeouts: Timeouts::default(),
        })
    }
}
```

All four candidates are probed; the first available endpoint in `preference` is selected. This does not expose four
independent command services. Explicit register addresses also work for BT (`[control, data]`), SMIC
(`[data, status_control, flags]`), and MMIO. All registers in one `RegisterWindow` use the same address space and access
width; a custom `Registers` backend can handle mixed access rules.

### BT example

BT uses the same register-window backend as KCS, but logical register 0 is control and register 1 is the data buffer.
This example uses an illustrative x64 I/O base of `0xe4`; replace it with the platform's decoded, exclusively owned
BT register window. For MMIO, supply a mapped base, the appropriate spacing/access width, and `AddressSpace::Mmio`.

```rust,no_run
extern crate alloc;
use alloc::{boxed::Box, vec};
use patina::component::IntoComponent;
use patina_ipmi::{
    Error, Endpoint, IpmiComponent, IpmiConfig, IpmiInfo, IpmiProtocolInstaller, Timeouts,
    backend::{AccessWidth, AddressSpace, RegisterWindow},
};

struct BtPlatform;

impl IpmiInfo for BtPlatform {
    fn config() -> Result<IpmiConfig, Error> {
        // SAFETY: On this example x64 platform, BT exclusively owns control
        // port 0xe4 and data port 0xe5, with byte accesses and I/O decode enabled.
        let registers = unsafe {
            RegisterWindow::new(0xe4, 1, 2, AccessWidth::Byte, AddressSpace::IoPort)?
        };
        Ok(IpmiConfig {
            endpoints: vec![Endpoint::Bt { id: "bmc-bt", registers: Box::new(registers) }],
            preference: vec!["bmc-bt"],
            timeouts: Timeouts::default(),
        })
    }
}

let provider = IpmiComponent::<BtPlatform>::new().into_component();
let c_adapter = IpmiProtocolInstaller::new().into_component();
// Add provider and c_adapter to the platform's ComponentInfo::components list.
# let _ = (provider, c_adapter);
```

### SSIF example

SSIF needs a platform SMBus controller adapter implementing `Smbus`, not a register window. The acquisition trait below
is an example integration contract: implement it for the platform's real adapter to initialize/acquire the controller
once and transfer its ownership. The crate does not supply a chipset-specific SMBus controller driver.

```rust,no_run
extern crate alloc;
use alloc::{boxed::Box, vec};
use core::marker::PhantomData;
use patina::component::IntoComponent;
use patina_ipmi::{
    Error, Endpoint, IpmiComponent, IpmiConfig, IpmiInfo, IpmiProtocolInstaller, Timeouts,
    backend::Smbus,
};

trait PlatformSmbus: Smbus + Sized + 'static {
    fn acquire() -> Result<Self, Error>;
}

struct SsifPlatform<B>(PhantomData<B>);

impl<B: PlatformSmbus> IpmiInfo for SsifPlatform<B> {
    fn config() -> Result<IpmiConfig, Error> {
        let bus = B::acquire()?;
        Ok(IpmiConfig {
            endpoints: vec![Endpoint::Ssif {
                id: "bmc-ssif",
                bus: Box::new(bus),
                address: 0x20,
            }],
            preference: vec!["bmc-ssif"],
            timeouts: Timeouts::default(),
        })
    }
}

fn register_components<B: PlatformSmbus>() {
    let provider = IpmiComponent::<SsifPlatform<B>>::new().into_component();
    let c_adapter = IpmiProtocolInstaller::new().into_component();
    // Add provider and c_adapter to the platform's ComponentInfo::components list.
#   let _ = (provider, c_adapter);
}
```

Call the example registration function with the platform adapter type, or use those component expressions directly in
the platform's component list. Replace `0x20` with the BMC's **seven-bit** SMBus address, not an eight-bit address
containing the read/write bit. The adapter must honor transaction timeouts, handle SMBus block counts, and implement
PEC when requested; return acquisition and controller errors rather than silently substituting a different bus.

### Endpoint resources

| Interface | Backend and configuration |
|-----------|---------------------------|
| KCS | `Registers`: data at logical register 0, status/command at 1 |
| SMIC | `Registers`: data at 0, status/control at 1, flags at 2 |
| BT | `Registers`: control at 0, data buffer at 1 |
| SSIF | `Smbus` backend and seven-bit BMC address |
| Direct IPMB | `IpmbBus` backend and distinct seven-bit BMC and local addresses |

`RegisterWindow` supports x64 I/O ports and x64/AArch64 MMIO, register spacing, explicit per-register addresses,
and byte/word/dword accesses. Its unsafe constructors require exclusive access to valid, mapped registers with the
specified access semantics. Use a custom `Registers` implementation for platform-specific access rules.
This crate does not enable chipset decoding or map physical MMIO addresses.

SSIF and IPMB require platform controller adapters, not register-address guessing:

- `Smbus` implements SMBus block reads/writes, including count-byte handling and PEC when requested.
- `IpmbBus` sends requests as a master and receives responses addressed to the local slave address. It is not merely
  an I2C master write/read transaction or an IPMI `Send Message` bridge. Frames include the destination address byte.
- Each bus operation must complete within its supplied microsecond timeout and release any controller locks on return.
  Return `Error::NotReady` for an SSIF response that is not yet available, not a generic controller failure.
- Bus adapters must use polling or otherwise work at the caller's TPL; the service does not raise TPL while polling.
  Firmware callers must run on the BSP, at a TPL supported by their backend (normally application or callback).

## Discovery and selection

There is no universal IPMI command that discovers all host register addresses and buses. The platform supplies
trusted candidate endpoints. The component validates their configuration, initializes each interface, and probes it
with `Get Device ID`. It records availability and failure reasons for every candidate, including unselected ones.
Firmware-update mode is considered not ready.

Selection uses only `preference`, an explicit ordered list of endpoint IDs. An available endpoint that is not listed
cannot be selected. Duplicate IDs, unknown preferences, reserved bus addresses, and zero timeouts are configuration
errors. If no allowed endpoint is available, initialization fails and logs the endpoint-specific diagnostics.
No arbitrary I/O, MMIO, or bus-address scanning is performed.

BT buffer sizes and SSIF message sizes, multipart support, and PEC support are queried during initialization.
If a BMC explicitly reports that the capabilities command is unsupported, the transport logs this and uses the
32-byte baseline; other errors are not hidden. Direct IPMB is limited to 32-byte wire messages.

## Rust command API

Consumers request `Service<dyn Ipmi>` through Patina dependency injection:

```rust,no_run
use patina::component::service::Service;
use patina_ipmi::{Error, Ipmi, Request, Response};

fn device_id(ipmi: Service<dyn Ipmi>) -> Result<Response, Error> {
    let mut payload = [0u8; 32];
    ipmi.submit(Request { netfn: 0x06, lun: 0, command: 0x01, data: &[] }, &mut payload)
}
```

NetFn is unshifted and must be even; LUN is 0 through 3. Responses separate the BMC completion code from the payload.
A nonzero completion code is not a transport failure. `BufferTooSmall` reports the required payload size, excluding
the completion code; the command has already executed. Do not blindly resubmit non-idempotent commands.

`endpoints()` returns the initial discovery report; `selected()` identifies the selected endpoint. These are discovery
snapshots, not continuously refreshed health reports.

## Reliability and lifecycle

- Commands are serialized across Rust and C callers. A concurrent or reentrant call returns `Busy`, not a spin wait.
- One deadline covers the complete transaction, not a fresh timeout per register or byte. Initialization has a separate
  deadline per candidate, shared by interface setup and the probe. An advancing monotonic clock and bounded controller
  backends are required. The component uses Patina's architectural timer; unsupported counter layouts fail explicitly.
- Fallback occurs only during discovery. Commands are never automatically replayed on another interface.
- An uncertain transaction failure faults the service: later submissions return `NotReady`. This prevents an
  unsequenced late response from being mistaken for the result of a later command. Recovery requires platform
  reinitialization (normally reboot); no automatic BMC reset is issued.
- KCS initialization can abort a stale host-interface transaction without resetting the BMC.
- An ExitBootServices callback stops submissions. Platforms must quiesce AP firmware activity before EBS; a bus operation
  already in progress must finish within its supplied timeout. This is not a runtime service.

## C compatibility

The optional adapter uses GUID `dbc6381f-5554-4d14-8ffd-76d787b8acbf`, the EFIAPI calling convention, and the
one-function-pointer layout from EDK II's `MdeModulePkg/Include/Protocol/IpmiProtocol.h`.
It uses LUN zero, includes the completion code as response byte zero, and reports required sizes in `UINT32` units
of bytes. Request/response buffer overlap is supported by staging; overwriting the protocol or size pointer is rejected.
As with other UEFI protocols, callers must supply valid memory; arbitrary invalid pointers cannot be made safe.

A zero-capacity response buffer reports a minimum requirement of one byte without executing the command. With a
nonzero undersized buffer, the command can execute and return the exact required size. This is not a side-effect-free
response-size query.

## Validation

```text
cargo test -p patina_ipmi
cargo clippy -p patina_ipmi --all-targets --all-features -- -D warnings
cargo check -p patina_ipmi --target x86_64-unknown-uefi --features ci_features
cargo check -p patina_ipmi --target aarch64-unknown-uefi --features ci_features
cargo fmt --all -- --check
```

Host tests use scripted registers and bus backends to validate wire ordering, framing, discovery, selection, timeouts,
malformed data, checksums, and protocol-buffer behavior. They do not certify hardware interoperability. QEMU and
physical-platform validation, including platform SSIF/IPMB controller adapters, are required before production use.
