//! The VM through Windows' Host Compute System (docs/silli-windows.md, sections 4 and 5):
//! `computecore.dll` for the machine, `computenetwork.dll` for its network, Winsock's Hyper-V
//! sockets for talking to the guest. A machine is described in a JSON document and made, changed
//! and stopped by a handful of calls, each an *operation* that is waited for.
//!
//! HCS refuses an ordinary account: this code runs in Silli's VM service, or in the program of a
//! developer who is in "Hyper-V Administrators". The machine is tied to the handle held here, so
//! it ends with the process that made it.
//!
//! A VM directory on Windows holds `system.vhd` (the image: the raw disk with the VHD footer),
//! `home.vhdx` and `state.vhdx` (the data disks, growing as they are written), `console.log` and
//! `lock`.
use std::ffi::c_void;
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::{Config, State};

type Handle = *mut c_void;

#[link(name = "computecore", kind = "raw-dylib")]
unsafe extern "system" {
    fn HcsCreateOperation(context: *const c_void, callback: *const c_void) -> Handle;
    fn HcsCloseOperation(operation: Handle);
    fn HcsWaitForOperationResult(operation: Handle, timeout_ms: u32, result: *mut *mut u16) -> i32;
    fn HcsCreateComputeSystem(id: *const u16, configuration: *const u16, operation: Handle, security: *const c_void, system: *mut Handle) -> i32;
    fn HcsOpenComputeSystem(id: *const u16, access: u32, system: *mut Handle) -> i32;
    fn HcsStartComputeSystem(system: Handle, operation: Handle, options: *const u16) -> i32;
    fn HcsShutDownComputeSystem(system: Handle, operation: Handle, options: *const u16) -> i32;
    fn HcsTerminateComputeSystem(system: Handle, operation: Handle, options: *const u16) -> i32;
    fn HcsCloseComputeSystem(system: Handle);
    fn HcsGetComputeSystemProperties(system: Handle, operation: Handle, query: *const u16) -> i32;
    fn HcsModifyComputeSystem(system: Handle, operation: Handle, configuration: *const u16, identity: Handle) -> i32;
    fn HcsGetServiceProperties(query: *const u16, result: *mut *mut u16) -> i32;
    fn HcsGrantVmAccess(vm_id: *const u16, file: *const u16) -> i32;
}

#[link(name = "computenetwork", kind = "raw-dylib")]
unsafe extern "system" {
    fn HcnCreateNetwork(id: *const Guid, settings: *const u16, network: *mut Handle, error: *mut *mut u16) -> i32;
    fn HcnCloseNetwork(network: Handle) -> i32;
    fn HcnDeleteNetwork(id: *const Guid, error: *mut *mut u16) -> i32;
    fn HcnCreateEndpoint(network: Handle, id: *const Guid, settings: *const u16, endpoint: *mut Handle, error: *mut *mut u16) -> i32;
    fn HcnCloseEndpoint(endpoint: Handle) -> i32;
    fn HcnDeleteEndpoint(id: *const Guid, error: *mut *mut u16) -> i32;
}

#[link(name = "virtdisk", kind = "raw-dylib")]
unsafe extern "system" {
    fn CreateVirtualDisk(
        kind: *const StorageType,
        path: *const u16,
        access: u32,
        security: *const c_void,
        flags: u32,
        provider_flags: u32,
        parameters: *const CreateParameters,
        overlapped: *const c_void,
        handle: *mut Handle,
    ) -> u32;
}

#[link(name = "ws2_32", kind = "raw-dylib")]
unsafe extern "system" {
    fn WSAStartup(version: u16, data: *mut u8) -> i32;
    fn WSAGetLastError() -> i32;
    fn socket(family: i32, kind: i32, protocol: i32) -> usize;
    fn connect(socket: usize, address: *const SockaddrHv, length: i32) -> i32;
    fn send(socket: usize, bytes: *const u8, length: i32, flags: i32) -> i32;
    fn recv(socket: usize, bytes: *mut u8, length: i32, flags: i32) -> i32;
    fn shutdown(socket: usize, how: i32) -> i32;
    fn closesocket(socket: usize) -> i32;
}

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
    fn CloseHandle(handle: Handle) -> i32;
    fn GetCurrentProcess() -> Handle;
}

#[link(name = "ole32", kind = "raw-dylib")]
unsafe extern "system" {
    fn CoTaskMemFree(memory: *mut c_void);
}

#[link(name = "advapi32", kind = "raw-dylib")]
unsafe extern "system" {
    fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
    fn GetTokenInformation(token: Handle, class: u32, information: *mut c_void, length: u32, needed: *mut u32) -> i32;
    fn ConvertSidToStringSidW(sid: *const c_void, text: *mut *mut u16) -> i32;
}

/// A GUID as Windows lays it out.
#[repr(C)]
#[derive(Clone, Copy)]
struct Guid(u32, u16, u16, [u8; 8]);

/// `VIRTUAL_STORAGE_TYPE`.
#[repr(C)]
struct StorageType {
    device: u32,
    vendor: Guid,
}

/// `CREATE_VIRTUAL_DISK_PARAMETERS`, version 2.
#[repr(C)]
struct CreateParameters {
    version: u32,
    unique_id: Guid,
    maximum_size: u64,
    block_size: u32,
    sector_size: u32,
    physical_sector_size: u32,
    parent_path: *const u16,
    source_path: *const u16,
    open_flags: u32,
    parent_type: StorageType,
    source_type: StorageType,
    resiliency: Guid,
}

/// Winsock's address of a Hyper-V socket: a VM and a service in it.
#[repr(C)]
struct SockaddrHv {
    family: u16,
    reserved: u16,
    vm: Guid,
    service: Guid,
}

const AF_HYPERV: i32 = 34;
const HV_PROTOCOL_RAW: i32 = 1;
/// HCS: the feature it needs (Virtual Machine Platform) is not installed.
const HCS_NOT_INSTALLED: i32 = 0x8037_0114_u32 as i32;
/// HCS: this account may not use it.
const HCS_ACCESS_DENIED: i32 = 0x8037_011B_u32 as i32;
/// The port Windows serves every share of a VM on; the guest connects to it to mount one.
pub const SHARE_PORT: u32 = 50001;
/// A share keeps the owner and mode of what the guest makes in it (hcsshim's "Linux metadata").
const SHARE_LINUX_METADATA: u32 = 4;
/// How long a call to HCS may take.
const WAIT: Duration = Duration::from_secs(60);

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// A NUL-terminated UTF-16 string the system allocated, read and freed with `free`.
fn taken(text: *mut u16, free: impl FnOnce(*mut c_void)) -> String {
    if text.is_null() {
        return String::new();
    }
    // SAFETY: a NUL-terminated string the system handed over, freed once here
    unsafe {
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        let said = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        free(text.cast());
        said
    }
}

fn local(text: *mut u16) -> String {
    // SAFETY: HCS allocates its documents with LocalAlloc
    taken(text, |p| unsafe {
        LocalFree(p);
    })
}

fn task(text: *mut u16) -> String {
    // SAFETY: HCN allocates its error records with CoTaskMemAlloc
    taken(text, |p| unsafe { CoTaskMemFree(p) })
}

fn hresult(code: i32) -> String {
    format!("0x{:08X}", code as u32)
}

/// One HCS call that works through an operation: started by `call`, waited for; its result
/// document, or what HCS said went wrong.
fn operation(what: &str, timeout: Duration, call: impl FnOnce(Handle) -> i32) -> Result<String> {
    // SAFETY: no context and no callback: the result is waited for
    let op = unsafe { HcsCreateOperation(std::ptr::null(), std::ptr::null()) };
    if op.is_null() {
        bail!("{what}: Windows could not make the operation");
    }
    let started = call(op);
    let mut said = std::ptr::null_mut();
    // SAFETY: a valid operation and out pointer; a call that failed is not waited for
    let waited = unsafe { HcsWaitForOperationResult(op, if started < 0 { 0 } else { timeout.as_millis() as u32 }, &mut said) };
    let said = local(said);
    // SAFETY: the operation made above, used no more
    unsafe { HcsCloseOperation(op) };
    let code = if started < 0 { started } else { waited };
    if code < 0 { Err(anyhow!("{what}: {} {said}", hresult(code))) } else { Ok(said) }
}

/// Why this account cannot use HCS, in plain words; `None` when it can.
pub(crate) fn unavailable() -> Option<(String, String)> {
    let mut said = std::ptr::null_mut();
    // SAFETY: a query document and an out pointer
    let code = unsafe { HcsGetServiceProperties(wide(r#"{"PropertyTypes":["Basic"]}"#).as_ptr(), &mut said) };
    let _ = local(said);
    match code {
        code if code >= 0 => None,
        HCS_NOT_INSTALLED => Some((
            "Windows' Virtual Machine Platform is switched off.".into(),
            "Switch it on in \"Turn Windows features on or off\", then restart Windows.".into(),
        )),
        HCS_ACCESS_DENIED => Some((
            "This account may not run virtual machines.".into(),
            "Silli's VM service does that for it; a developer's account is in \"Hyper-V Administrators\" instead.".into(),
        )),
        code => Some((format!("Windows cannot run virtual machines here ({}).", hresult(code)), "Silli cannot run on this PC as it is.".into())),
    }
}

/// A GUID from 128 bits, and as text.
fn guid(value: u128) -> (Guid, String) {
    let b = value.to_be_bytes();
    let g = Guid(
        u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        u16::from_be_bytes([b[4], b[5]]),
        u16::from_be_bytes([b[6], b[7]]),
        [b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]],
    );
    let text =
        format!("{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}", g.0, g.1, g.2, b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]);
    (g, text)
}

fn parse_guid(text: &str) -> Option<Guid> {
    let hex: String = text.chars().filter(char::is_ascii_hexdigit).collect();
    if hex.len() != 32 {
        return None;
    }
    Some(guid(u128::from_str_radix(&hex, 16).ok()?).0)
}

/// The service id Linux's vsock port `port` has on Hyper-V sockets.
fn vsock_service(port: u32) -> (Guid, String) {
    guid(((port as u128) << 96) | 0xfacb_11e6_bd58_6400_6a79_86d3)
}

/// 128 bits that are always the same for the same text (FNV-1a, twice over): the ids of a VM
/// directory's machine, network and endpoint, so what an earlier run left can be found again.
fn stable(text: &str, salt: u8) -> u128 {
    let mut hash: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    for byte in std::iter::once(salt).chain(text.bytes()) {
        hash ^= byte as u128;
        hash = hash.wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013b);
    }
    hash
}

/// This account's SID as text (`S-1-5-21-…`).
fn account_sid() -> Result<String> {
    const TOKEN_QUERY: u32 = 0x8;
    const TOKEN_USER: u32 = 1;
    let mut token = std::ptr::null_mut();
    // SAFETY: this process's own token, opened to be read; the buffer is large enough for a SID
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            bail!("this account's identity could not be read");
        }
        let mut buffer = [0u64; 32];
        let mut needed = 0;
        let read = GetTokenInformation(token, TOKEN_USER, buffer.as_mut_ptr().cast(), std::mem::size_of_val(&buffer) as u32, &mut needed);
        CloseHandle(token);
        if read == 0 {
            bail!("this account's identity could not be read");
        }
        // TOKEN_USER begins with the pointer to the SID
        let sid = *(buffer.as_ptr() as *const *const c_void);
        let mut text = std::ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut text) == 0 {
            bail!("this account's identity could not be read");
        }
        Ok(local(text))
    }
}

/// A new data disk at `path`: a VHDX of `bytes` that takes space only as it is written.
pub fn create_disk(path: &Path, bytes: u64) -> Result<()> {
    const VHDX: u32 = 3;
    // VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT
    let vendor = guid(0xec98_4aec_a0f9_47e9_901f_7141_5a66_345b).0;
    let none = Guid(0, 0, 0, [0; 8]);
    let kind = StorageType { device: VHDX, vendor };
    let parameters = CreateParameters {
        version: 2,
        unique_id: none,
        maximum_size: bytes,
        block_size: 0,
        sector_size: 0,
        physical_sector_size: 0,
        parent_path: std::ptr::null(),
        source_path: std::ptr::null(),
        open_flags: 0,
        parent_type: StorageType { device: 0, vendor: none },
        source_type: StorageType { device: 0, vendor: none },
        resiliency: none,
    };
    let mut handle = std::ptr::null_mut();
    // SAFETY: valid structures and a NUL-terminated path; no access is asked of the new disk
    let code = unsafe { CreateVirtualDisk(&kind, wide_path(path).as_ptr(), 0, std::ptr::null(), 0, 0, &parameters, std::ptr::null(), &mut handle) };
    if code != 0 {
        bail!("Windows could not make the disk {} (error {code})", path.display());
    }
    // SAFETY: the handle CreateVirtualDisk gave
    unsafe { CloseHandle(handle) };
    Ok(())
}

/// A raw disk image as a fixed VHD: the same bytes, to whole megabytes, and the format's
/// 512-byte footer. Written beside `to` and moved into place, so a half-made file is never taken
/// for a disk.
pub fn raw_to_vhd(raw: &Path, to: &Path) -> Result<()> {
    use std::io::Seek;
    let temp = to.with_extension("vhd.new");
    let mut from = std::fs::File::open(raw).with_context(|| format!("opening {}", raw.display()))?;
    let mut out = std::fs::File::create(&temp).with_context(|| format!("making {}", temp.display()))?;
    let copied = std::io::copy(&mut from, &mut out)?;
    let size = copied.div_ceil(1 << 20) << 20;
    out.set_len(size)?;
    out.seek(std::io::SeekFrom::Start(size))?;
    out.write_all(&vhd_footer(size))?;
    out.sync_all()?;
    drop(out);
    std::fs::rename(&temp, to)?;
    Ok(())
}

/// The footer of a fixed VHD of `size` bytes.
fn vhd_footer(size: u64) -> [u8; 512] {
    let mut footer = [0u8; 512];
    footer[0..8].copy_from_slice(b"conectix");
    footer[8..12].copy_from_slice(&2u32.to_be_bytes());
    footer[12..16].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    // no dynamic header: a fixed disk
    footer[16..24].copy_from_slice(&u64::MAX.to_be_bytes());
    footer[28..32].copy_from_slice(b"sili");
    footer[32..36].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    footer[36..40].copy_from_slice(b"Wi2k");
    footer[40..48].copy_from_slice(&size.to_be_bytes());
    footer[48..56].copy_from_slice(&size.to_be_bytes());
    // the geometry the format's specification gives a large disk
    let sectors = (size / 512).min(65535 * 16 * 255);
    let (cylinders, per_track) = if sectors >= 65535 * 16 * 63 { (sectors / 255 / 16, 255u8) } else { (sectors / 63 / 16, 63u8) };
    footer[56..58].copy_from_slice(&(cylinders.min(65535) as u16).to_be_bytes());
    footer[58] = 16;
    footer[59] = per_track;
    footer[60..64].copy_from_slice(&2u32.to_be_bytes());
    footer[68..84].copy_from_slice(&stable(&size.to_string(), 9).to_be_bytes());
    let sum: u32 = footer.iter().map(|b| *b as u32).sum();
    footer[64..68].copy_from_slice(&(!sum).to_be_bytes());
    footer
}

/// The VM's network and its one endpoint, deleted again when dropped.
struct Network {
    network: (Guid, Handle),
    endpoint: (Guid, Handle),
}

// SAFETY: HCN's handles may be used from any thread
unsafe impl Send for Network {}
unsafe impl Sync for Network {}

impl Drop for Network {
    fn drop(&mut self) {
        // SAFETY: the handles and ids made in `Network::make`
        unsafe {
            let mut said = std::ptr::null_mut();
            HcnCloseEndpoint(self.endpoint.1);
            HcnDeleteEndpoint(&self.endpoint.0, &mut said);
            let _ = task(said);
            let mut said = std::ptr::null_mut();
            HcnCloseNetwork(self.network.1);
            HcnDeleteNetwork(&self.network.0, &mut said);
            let _ = task(said);
        }
    }
}

impl Network {
    /// A network of Windows' ICS kind with its DHCP server and DNS proxy, not kept past a restart
    /// of Windows, and one endpoint on it; the adapter's entry for the VM's document comes back.
    /// What a run that died left under the same ids is deleted first.
    fn make(key: &str) -> Result<(Network, Value)> {
        let (network_id, network_text) = guid(stable(key, 1));
        let (endpoint_id, endpoint_text) = guid(stable(key, 2));
        // SAFETY: ids of our own; an error only says there was nothing to delete
        unsafe {
            let mut said = std::ptr::null_mut();
            HcnDeleteEndpoint(&endpoint_id, &mut said);
            let _ = task(said);
            let mut said = std::ptr::null_mut();
            HcnDeleteNetwork(&network_id, &mut said);
            let _ = task(said);
        }
        // flags: 1 DNS proxy, 2 DHCP server, 8 not persistent
        let settings = json!({
            "SchemaVersion": {"Major": 2, "Minor": 0},
            "Name": format!("Silli {}", &network_text[..8]),
            "Type": "ICS",
            "Flags": 11,
            "Ipams": [{"Type": "Static", "Subnets": [{"IpAddressPrefix": "172.29.240.0/24", "Routes": [{"NextHop": "172.29.240.1", "DestinationPrefix": "0.0.0.0/0"}]}]}],
        });
        let (mut network, mut said) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: a valid id, document and out pointers
        let code = unsafe { HcnCreateNetwork(&network_id, wide(&settings.to_string()).as_ptr(), &mut network, &mut said) };
        let said = task(said);
        if code < 0 {
            bail!("Windows could not make the VM's network: {} {said}", hresult(code));
        }
        let hash = stable(key, 3);
        let mac = format!("00-15-5D-{:02X}-{:02X}-{:02X}", hash as u8, (hash >> 8) as u8, (hash >> 16) as u8);
        let settings = json!({"SchemaVersion": {"Major": 2, "Minor": 0}, "HostComputeNetwork": network_text, "MacAddress": mac, "Policies": []});
        let (mut endpoint, mut said) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: the network made above, a valid id, document and out pointers
        let code = unsafe { HcnCreateEndpoint(network, &endpoint_id, wide(&settings.to_string()).as_ptr(), &mut endpoint, &mut said) };
        let said = task(said);
        let made = Network { network: (network_id, network), endpoint: (endpoint_id, endpoint) };
        if code < 0 {
            bail!("Windows could not make the VM's network adapter: {} {said}", hresult(code));
        }
        // keyed by the endpoint's id: under another key HCS accepts the adapter and the guest sees none
        let adapter = json!({endpoint_text.clone(): {"EndpointId": endpoint_text, "MacAddress": mac}});
        Ok((made, adapter))
    }
}

/// A running (or stopped) VM; dropping it ends the machine.
pub struct Vm {
    system: Handle,
    id: String,
    runtime: Guid,
    dir: PathBuf,
    _network: Network,
    /// Held while the VM is this process's: a second owner of the same disks is refused.
    _lock: std::fs::File,
}

// SAFETY: HCS's handles may be used from any thread
unsafe impl Send for Vm {}
unsafe impl Sync for Vm {}

impl Vm {
    /// Make the machine from the disks in `config.dir` and start it; returns once HCS has started
    /// it (the guest boots on: `connect` waits for what it needs). `ports` are the guest's vsock
    /// ports this account may connect to from Windows.
    pub fn start(config: &Config, ports: &[u32]) -> Result<Self> {
        let dir = std::path::absolute(&config.dir)?;
        let lock = lock(&dir)?;
        let key = dir.to_string_lossy().to_lowercase();
        let id = format!("silli-{:016x}", stable(&key, 0) as u64);
        let system_disk = dir.join("system.vhd");
        if !system_disk.exists() {
            bail!("no system disk in {}", dir.display());
        }
        let mut attachments = serde_json::Map::new();
        let mut disks = vec![system_disk];
        disks.extend(["home.vhdx", "state.vhdx"].map(|name| dir.join(name)).into_iter().filter(|disk| disk.exists()));
        for (lun, disk) in disks.iter().enumerate() {
            attachments.insert(lun.to_string(), json!({"Type": "VirtualDisk", "Path": disk}));
        }
        let (network, adapter) = Network::make(&key)?;
        // who may connect to the guest's listeners from Windows: this account, and the system
        let allowed = format!("D:P(A;;FA;;;{})(A;;FA;;;SY)", account_sid()?);
        let mut services = serde_json::Map::new();
        for port in ports {
            services
                .insert(vsock_service(*port).1, json!({"BindSecurityDescriptor": allowed, "ConnectSecurityDescriptor": allowed, "AllowWildcardBinds": false}));
        }
        let pipe = format!(r"\\.\pipe\{id}-console");
        let document = json!({
            "SchemaVersion": {"Major": 2, "Minor": 2},
            "Owner": "Silli",
            "ShouldTerminateOnLastHandleClosed": true,
            "VirtualMachine": {
                "StopOnReset": true,
                "Chipset": {"UseUtc": true, "Uefi": {"BootThis": {"DevicePath": "disks", "DiskNumber": 0, "DeviceType": "ScsiDrive"}}},
                "ComputeTopology": {
                    "Memory": {"SizeInMB": config.memory_gb * 1024, "AllowOvercommit": true},
                    "Processor": {"Count": config.cpus},
                },
                "Devices": {
                    "Scsi": {"disks": {"Attachments": attachments}},
                    "NetworkAdapters": adapter,
                    "ComPorts": {"0": {"NamedPipe": pipe}},
                    "HvSocket": {"HvSocketConfig": {"ServiceTable": services}},
                    "Plan9": {},
                },
            },
        });
        // a machine an earlier run left under this id (its process killed mid-call) is ended first
        end_leftover(&id);
        let mut system: Handle = std::ptr::null_mut();
        operation("making the VM", WAIT, |op| {
            // SAFETY: valid strings, operation and out pointer; the security descriptor is reserved
            unsafe { HcsCreateComputeSystem(wide(&id).as_ptr(), wide(&document.to_string()).as_ptr(), op, std::ptr::null(), &mut system) }
        })?;
        let mut vm = Vm { system, id: id.clone(), runtime: Guid(0, 0, 0, [0; 8]), dir: dir.clone(), _network: network, _lock: lock };
        // the machine runs as an account of its own, which must be let at its disks
        for disk in &disks {
            // SAFETY: valid strings
            let code = unsafe { HcsGrantVmAccess(wide(&id).as_ptr(), wide_path(disk).as_ptr()) };
            if code < 0 {
                bail!("the VM could not be given its disk {}: {}", disk.display(), hresult(code));
            }
        }
        operation("starting the VM", WAIT, |op| {
            // SAFETY: the system made above
            unsafe { HcsStartComputeSystem(system, op, std::ptr::null()) }
        })?;
        let properties: Value = serde_json::from_str(&vm.properties()?).context("reading the VM's properties")?;
        vm.runtime = properties["RuntimeId"].as_str().and_then(parse_guid).context("the VM has no runtime id")?;
        console(&pipe, &dir.join("console.log"));
        Ok(vm)
    }

    fn properties(&self) -> Result<String> {
        operation("asking about the VM", Duration::from_secs(10), |op| {
            // SAFETY: the system held by this Vm
            unsafe { HcsGetComputeSystemProperties(self.system, op, wide("{}").as_ptr()) }
        })
    }

    pub fn state(&self) -> State {
        let Ok(properties) = self.properties() else { return State::Stopped };
        match serde_json::from_str::<Value>(&properties).ok().as_ref().and_then(|p| p["State"].as_str()) {
            Some("Running") => State::Running,
            Some("Created") => State::Starting,
            Some("Stopped") | None => State::Stopped,
            Some("Paused" | "SavedAsTemplate" | "Saved") => State::Other,
            Some(_) => State::Other,
        }
    }

    /// Ask the guest to shut down: it stops itself, cleanly.
    pub fn request_stop(&self) -> Result<()> {
        // Hyper-V's shutdown service, where the machine has one. A machine made as this one is has
        // none (tried 2026-11-20: "not found"), so the guest is asked through its own root helper
        // by whoever holds the VM, and this is the fallback that costs nothing to try.
        let options = r#"{"Mechanism":"IntegrationService","Type":"Shutdown"}"#;
        operation("asking the VM to shut down", WAIT, |op| {
            // SAFETY: the system held by this Vm
            unsafe { HcsShutDownComputeSystem(self.system, op, wide(options).as_ptr()) }
        })
        .map(drop)
    }

    /// Stop the machine at once, as pulling its plug would.
    pub fn force_stop(&self) -> Result<()> {
        operation("stopping the VM", WAIT, |op| {
            // SAFETY: the system held by this Vm
            unsafe { HcsTerminateComputeSystem(self.system, op, std::ptr::null()) }
        })
        .map(drop)
    }

    /// Ask the guest to shut down and wait up to `patience` for it; then stop it at once.
    pub fn shut_down(&self, patience: Duration) -> Result<()> {
        if self.state() == State::Stopped {
            return Ok(());
        }
        let asked = self.request_stop();
        let until = Instant::now() + patience;
        while asked.is_ok() && Instant::now() < until {
            if self.state() == State::Stopped {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        self.force_stop()
    }

    /// A connection to the guest's vsock `port`, which must be one of those the VM was started
    /// with.
    pub fn connect(&self, port: u32) -> Result<HvStream> {
        static STARTED: std::sync::Once = std::sync::Once::new();
        STARTED.call_once(|| {
            let mut data = [0u8; 512];
            // SAFETY: Winsock 2.2 with room for its data
            unsafe { WSAStartup(0x0202, data.as_mut_ptr()) };
        });
        let address = SockaddrHv { family: AF_HYPERV as u16, reserved: 0, vm: self.runtime, service: vsock_service(port).0 };
        // SAFETY: plain Winsock calls with a valid address
        unsafe {
            let s = socket(AF_HYPERV, 1, HV_PROTOCOL_RAW);
            if s == usize::MAX {
                bail!("no Hyper-V socket could be made (Winsock error {})", WSAGetLastError());
            }
            if connect(s, &address, std::mem::size_of::<SockaddrHv>() as i32) != 0 {
                let code = WSAGetLastError();
                closesocket(s);
                bail!("the VM does not answer on port {port} (Winsock error {code})");
            }
            Ok(HvStream(Arc::new(Socket(s))))
        }
    }

    /// Share `folder` with the running VM under `name`: the guest mounts it from [`SHARE_PORT`].
    pub fn share(&self, name: &str, folder: &Path) -> Result<()> {
        self.change_share("Add", json!({"Name": name, "AccessName": name, "Path": folder, "Port": SHARE_PORT, "Flags": SHARE_LINUX_METADATA}))
    }

    /// Stop sharing what was shared as `name`. Unmount it in the guest first.
    pub fn unshare(&self, name: &str) -> Result<()> {
        self.change_share("Remove", json!({"Name": name, "AccessName": name, "Port": SHARE_PORT}))
    }

    fn change_share(&self, request: &str, settings: Value) -> Result<()> {
        let document = json!({"ResourcePath": "VirtualMachine/Devices/Plan9/Shares", "RequestType": request, "Settings": settings});
        operation("changing the VM's shared folders", WAIT, |op| {
            // SAFETY: the running system, a valid document, the caller's own identity
            unsafe { HcsModifyComputeSystem(self.system, op, wide(&document.to_string()).as_ptr(), std::ptr::null_mut()) }
        })
        .map(drop)
    }

    /// The VM's directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// HCS's name for the machine.
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.force_stop();
        // SAFETY: the system held by this Vm, used no more
        unsafe { HcsCloseComputeSystem(self.system) };
    }
}

/// End a machine left under `id` by a run that died.
fn end_leftover(id: &str) {
    const ALL_ACCESS: u32 = 0x1000_0000;
    let mut system = std::ptr::null_mut();
    // SAFETY: a valid id and out pointer
    if unsafe { HcsOpenComputeSystem(wide(id).as_ptr(), ALL_ACCESS, &mut system) } < 0 {
        return;
    }
    let _ = operation("ending the VM an earlier run left", WAIT, |op| {
        // SAFETY: the system opened above
        unsafe { HcsTerminateComputeSystem(system, op, std::ptr::null()) }
    });
    // SAFETY: the system opened above, used no more
    unsafe { HcsCloseComputeSystem(system) };
    std::thread::sleep(Duration::from_millis(500));
}

/// One owner of a VM directory at a time.
fn lock(dir: &Path) -> Result<std::fs::File> {
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("lock"))?;
    file.try_lock().map_err(|_| anyhow!("the VM in {} is already running", dir.display()))?;
    Ok(file)
}

/// The guest's console, copied into `log` for as long as the machine's pipe lasts.
fn console(pipe: &str, log: &Path) {
    let (pipe, log) = (pipe.to_string(), log.to_path_buf());
    let _ = std::thread::Builder::new().name("silli-vm-console".into()).spawn(move || {
        // the machine's worker makes the pipe as it starts: knock for a few seconds
        let opened = (0..50).find_map(|_| {
            let file = std::fs::OpenOptions::new().read(true).open(&pipe);
            if file.is_err() {
                std::thread::sleep(Duration::from_millis(100));
            }
            file.ok()
        });
        let (Some(mut pipe), Ok(mut log)) = (opened, std::fs::File::create(&log)) else { return };
        let mut bytes = [0u8; 4096];
        while let Ok(n) = pipe.read(&mut bytes) {
            if n == 0 || log.write_all(&bytes[..n]).is_err() {
                break;
            }
        }
    });
}

struct Socket(usize);

impl Drop for Socket {
    fn drop(&mut self) {
        // SAFETY: the socket this owns
        unsafe { closesocket(self.0) };
    }
}

/// A connection to a listener in the guest. A clone is the same connection, so one thread can
/// read while another writes.
#[derive(Clone)]
pub struct HvStream(Arc<Socket>);

impl HvStream {
    /// End the connection in both directions: a read or write waiting on it returns.
    pub fn shutdown(&self) {
        // SAFETY: the socket this holds; 2 is both directions
        unsafe { shutdown(self.0.0, 2) };
    }

    /// No more is written: the guest reads the end, and may still answer.
    pub fn shutdown_write(&self) {
        // SAFETY: the socket this holds; 1 is the sending direction
        unsafe { shutdown(self.0.0, 1) };
    }
}

impl Read for HvStream {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        // SAFETY: a valid buffer and the socket this holds
        let n = unsafe { recv(self.0.0, bytes.as_mut_ptr(), bytes.len().min(i32::MAX as usize) as i32, 0) };
        if n < 0 { Err(std::io::Error::from_raw_os_error(unsafe { WSAGetLastError() })) } else { Ok(n as usize) }
    }
}

impl Write for HvStream {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        // SAFETY: a valid buffer and the socket this holds
        let n = unsafe { send(self.0.0, bytes.as_ptr(), bytes.len().min(i32::MAX as usize) as i32, 0) };
        if n < 0 { Err(std::io::Error::from_raw_os_error(unsafe { WSAGetLastError() })) } else { Ok(n as usize) }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vsock_port_has_its_hyper_v_service_id() {
        assert_eq!(vsock_service(7600).1, "00001db0-facb-11e6-bd58-64006a7986d3");
        assert_eq!(vsock_service(50001).1, "0000c351-facb-11e6-bd58-64006a7986d3");
    }

    #[test]
    fn a_vm_directory_always_has_the_same_ids() {
        assert_eq!(stable("c:\\a", 1), stable("c:\\a", 1));
        assert_ne!(stable("c:\\a", 1), stable("c:\\a", 2));
        assert_ne!(stable("c:\\a", 1), stable("c:\\b", 1));
        let (_, text) = guid(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        assert_eq!(text, "01234567-89ab-cdef-0123-456789abcdef");
        assert!(parse_guid(&text).is_some() && parse_guid("not a guid").is_none());
    }

    #[test]
    fn a_fixed_vhd_ends_in_a_footer_that_sums_right() {
        let footer = vhd_footer(3 << 30);
        assert_eq!(&footer[0..8], b"conectix");
        assert_eq!(u64::from_be_bytes(footer[48..56].try_into().unwrap()), 3 << 30);
        let mut unsummed = footer;
        unsummed[64..68].fill(0);
        let sum: u32 = unsummed.iter().map(|b| *b as u32).sum();
        assert_eq!(u32::from_be_bytes(footer[64..68].try_into().unwrap()), !sum);
    }
}
