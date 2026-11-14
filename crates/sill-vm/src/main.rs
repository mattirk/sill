//! SPIKE (2026-11-12): boot a Debian cloud image with Apple's Virtualization framework from Rust
//! and reach its SSH. `sill-vm DIR`: DIR holds `system.raw` (the system disk), optionally
//! `home.raw` and `state.raw` (the data disks, seen in the guest as `/dev/disk/by-id/virtio-sill-home`
//! and `virtio-sill-state`, `packaging/mac/guest`) and `seed.iso` (cloud-init, NoCloud);
//! the VM's EFI variables and machine identity are kept there too, so the guest keeps its DHCP
//! lease between boots. Prints the guest's address once its SSH answers; with `SILL_VM_HOLD`
//! set to a path, keeps the VM until that file exists, then asks the guest to stop. CPUs and
//! memory: `SILL_VM_CPUS` (4), `SILL_VM_MEMORY_GB` (4). The binary needs the
//! `com.apple.security.virtualization` entitlement (`packaging/mac/sill-vm.entitlements`).
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("sill-vm: macOS only");
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    spike::run()
}

#[cfg(target_os = "macos")]
mod spike {
    use std::cell::RefCell;
    use std::io::Read;
    use std::net::{SocketAddr, TcpStream};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, anyhow, bail};
    use block2::RcBlock;
    use dispatch2::DispatchQueue;
    use objc2::rc::Retained;
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_foundation::{NSArray, NSError, NSRunLoop, NSString, NSURL};
    use objc2_virtualization::*;

    const MAC: &str = "5a:11:00:00:00:01";

    thread_local! {
        static VM: RefCell<Option<Retained<VZVirtualMachine>>> = const { RefCell::new(None) };
    }

    fn url(path: &Path) -> Retained<NSURL> {
        NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
    }

    fn said(e: Retained<NSError>) -> anyhow::Error {
        anyhow!("{}", e.localizedDescription())
    }

    pub fn run() -> Result<()> {
        let dir = PathBuf::from(std::env::args().nth(1).context("usage: sill-vm DIR")?);
        MainThreadMarker::new().context("not on the main thread")?;
        let started = Instant::now();
        unsafe {
            if !VZVirtualMachine::isSupported() {
                bail!("this Mac cannot run virtual machines");
            }
            let platform = VZGenericPlatformConfiguration::new();
            let id_path = dir.join("machine-id");
            let id = std::fs::read(&id_path).ok().and_then(|bytes| {
                VZGenericMachineIdentifier::initWithDataRepresentation(
                    VZGenericMachineIdentifier::alloc(),
                    &objc2_foundation::NSData::with_bytes(&bytes),
                )
            });
            let id = match id {
                Some(id) => id,
                None => {
                    let id = VZGenericMachineIdentifier::new();
                    std::fs::write(&id_path, id.dataRepresentation().to_vec())?;
                    id
                }
            };
            platform.setMachineIdentifier(&id);
            let boot = VZEFIBootLoader::new();
            let vars_path = dir.join("efivars");
            let vars = if vars_path.exists() {
                VZEFIVariableStore::initWithURL(VZEFIVariableStore::alloc(), &url(&vars_path))
            } else {
                VZEFIVariableStore::initCreatingVariableStoreAtURL_options_error(
                    VZEFIVariableStore::alloc(),
                    &url(&vars_path),
                    VZEFIVariableStoreInitializationOptions::empty(),
                )
                .map_err(said)?
            };
            boot.setVariableStore(Some(&vars));

            let disk = |name: &str,
                        read_only: bool,
                        id: Option<&str>|
             -> Result<Retained<VZStorageDeviceConfiguration>> {
                let attachment = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
                    VZDiskImageStorageDeviceAttachment::alloc(),
                    &url(&dir.join(name)),
                    read_only,
                )
                .map_err(said)
                .with_context(|| name.to_string())?;
                let device = VZVirtioBlockDeviceConfiguration::initWithAttachment(
                    VZVirtioBlockDeviceConfiguration::alloc(),
                    &attachment,
                );
                if let Some(id) = id {
                    device.setBlockDeviceIdentifier(&NSString::from_str(id));
                }
                Ok(Retained::into_super(device))
            };
            let mut disks = vec![disk("system.raw", false, None)?];
            for (file, id) in [("home.raw", "sill-home"), ("state.raw", "sill-state")] {
                if dir.join(file).exists() {
                    disks.push(disk(file, false, Some(id))?);
                }
            }
            if dir.join("seed.iso").exists() {
                disks.push(disk("seed.iso", true, None)?);
            }

            let net = VZVirtioNetworkDeviceConfiguration::new();
            net.setAttachment(Some(&VZNATNetworkDeviceAttachment::new()));
            let mac = VZMACAddress::initWithString(VZMACAddress::alloc(), &NSString::from_str(MAC))
                .context("MAC address")?;
            net.setMACAddress(&mac);

            let serial = VZVirtioConsoleDeviceSerialPortConfiguration::new();
            let log = VZFileSerialPortAttachment::initWithURL_append_error(
                VZFileSerialPortAttachment::alloc(),
                &url(&dir.join("console.log")),
                false,
            )
            .map_err(said)?;
            serial.setAttachment(Some(&log));

            let config = VZVirtualMachineConfiguration::new();
            config.setPlatform(&platform);
            config.setBootLoader(Some(&boot));
            let number = |name: &str, default: u64| {
                std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
            };
            config.setCPUCount(number("SILL_VM_CPUS", 4) as usize);
            config.setMemorySize(number("SILL_VM_MEMORY_GB", 4) * 1024 * 1024 * 1024);
            config.setStorageDevices(&NSArray::from_retained_slice(&disks));
            config.setNetworkDevices(&NSArray::from_retained_slice(&[Retained::into_super(net)]));
            config.setSerialPorts(&NSArray::from_retained_slice(&[Retained::into_super(serial)]));
            config.setEntropyDevices(&NSArray::from_retained_slice(&[Retained::into_super(
                VZVirtioEntropyDeviceConfiguration::new(),
            )]));
            config.validateWithError().map_err(said).context("the VM's configuration")?;

            let vm = VZVirtualMachine::initWithConfiguration(VZVirtualMachine::alloc(), &config);
            let done = RcBlock::new(move |error: *mut NSError| {
                if error.is_null() {
                    eprintln!("[{:>5.1}s] started", started.elapsed().as_secs_f32());
                } else {
                    eprintln!("start failed: {}", (*error).localizedDescription());
                    std::process::exit(1);
                }
            });
            vm.startWithCompletionHandler(&done);
            VM.with(|slot| *slot.borrow_mut() = Some(vm));
        }

        // off the main thread: wait for the guest's address and its SSH, then ask it to stop
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(180);
            let mut seen = None;
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(500));
                // read afresh each round: an older lease of the same name may be there first
                let now = leased();
                if now.is_some() && now != seen {
                    seen = now;
                    if let Some(ip) = &seen {
                        eprintln!("[{:>5.1}s] address {ip}", started.elapsed().as_secs_f32());
                    }
                }
                let Some(ip) = &seen else { continue };
                let Ok(addr) = format!("{ip}:22").parse::<SocketAddr>() else { continue };
                if let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(1)) {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut banner = [0u8; 64];
                    let n = stream.read(&mut banner).unwrap_or(0);
                    eprintln!(
                        "[{:>5.1}s] ssh answers: {}",
                        started.elapsed().as_secs_f32(),
                        String::from_utf8_lossy(&banner[..n]).trim()
                    );
                    println!("{ip}");
                    break;
                }
            }
            // held until told to go on (a file appears), so the caller can use the guest
            let go = std::env::var_os("SILL_VM_HOLD").map(PathBuf::from);
            while go.as_ref().is_some_and(|path| !path.exists()) {
                std::thread::sleep(Duration::from_millis(300));
            }
            DispatchQueue::main().exec_async(move || {
                VM.with(|slot| {
                    if let Some(vm) = slot.borrow().as_ref() {
                        let asked = unsafe { vm.requestStopWithError() };
                        eprintln!("[{:>5.1}s] stop requested: {:?}", started.elapsed().as_secs_f32(), asked.is_ok());
                    }
                });
            });
            // the guest powers off on the request; watch the state from here
            for _ in 0..120 {
                std::thread::sleep(Duration::from_millis(500));
                let (tx, rx) = std::sync::mpsc::channel();
                DispatchQueue::main().exec_async(move || {
                    let state = VM.with(|slot| slot.borrow().as_ref().map(|vm| unsafe { vm.state() }));
                    let _ = tx.send(state == Some(VZVirtualMachineState::Stopped));
                });
                if rx.recv().unwrap_or(false) {
                    eprintln!("[{:>5.1}s] stopped", started.elapsed().as_secs_f32());
                    std::process::exit(0);
                }
            }
            eprintln!("the guest did not stop; leaving (the VM ends with this process)");
            std::process::exit(2);
        });
        NSRunLoop::mainRunLoop().run();
        Ok(())
    }

    /// The address macOS's DHCP server leased to the guest, found by its host name (the guest
    /// identifies itself by a DHCP client id, not its MAC); of several, the latest lease.
    fn leased() -> Option<String> {
        let leases = std::fs::read_to_string("/var/db/dhcpd_leases").ok()?;
        leases
            .split('}')
            .filter(|entry| entry.lines().any(|line| line.trim() == "name=sill-box"))
            .filter_map(|entry| {
                let field = |key: &str| {
                    entry.lines().find_map(|line| line.trim().strip_prefix(key).map(str::to_string))
                };
                let until = u64::from_str_radix(field("lease=0x")?.as_str(), 16).ok()?;
                Some((until, field("ip_address=")?))
            })
            .max()
            .map(|(_, ip)| ip)
    }
}
