# IPMI

The `patina_ipmi` crate provides platform-configured KCS, SMIC, BT, SSIF, and direct IPMB access.
It publishes a Rust service and an optional EDK II-compatible `IPMI_PROTOCOL`.

Platforms implement `IpmiInfo`, supplying candidate endpoints and an ordered selection preference, and register
`IpmiComponent`. Register `IpmiProtocolInstaller` when C drivers also need access. The component initializes its own
DXE transport and does not require an IPMI HOB from PEI.

See the crate-level documentation for configuration examples, hardware-backend contracts, discovery and fallback
behavior, API details, lifecycle requirements, and validation commands. SSIF/IPMB require platform bus-controller
adapters. Scripted host tests are not a substitute for QEMU and physical-platform testing.
