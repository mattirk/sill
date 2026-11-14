//! SPIKE (2026-11-12): boot a Debian cloud image with Apple's Virtualization framework from Rust
//! and reach its SSH. `sill-vm DIR`: DIR holds `disk.raw` and `seed.iso` (cloud-init, NoCloud).
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

            let disk = |name: &str, read_only: bool| -> Result<Retained<VZStorageDeviceConfiguration>> {
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
                Ok(Retained::into_super(device))
            };
            let disks = [disk("disk.raw", false)?, disk("seed.iso", true)?];

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
            config.setCPUCount(4);
            config.setMemorySize(4 * 1024 * 1024 * 1024);
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
                if seen.is_none() {
                    seen = leased();
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
    /// identifies itself by a DHCP client id, not its MAC).
    fn leased() -> Option<String> {
        let leases = std::fs::read_to_string("/var/db/dhcpd_leases").ok()?;
        let mut found = None;
        for entry in leases.split('}') {
            if entry.lines().any(|line| line.trim() == "name=sill-box") {
                found = entry
                    .lines()
                    .find_map(|line| line.trim().strip_prefix("ip_address=").map(str::to_string));
            }
        }
        found
    }
}
