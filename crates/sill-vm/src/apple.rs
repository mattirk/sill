//! The VM with Apple's Virtualization framework. The machine lives on a serial dispatch queue of
//! its own: every call to it is made there (`on_queue`), so any thread may use a `Vm`, and the
//! process's main thread (the window's) is never involved.
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::{AnyThread, DefinedClass};
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSData, NSError, NSObject, NSObjectProtocol, NSString, NSURL};
use objc2_virtualization::*;

use crate::{Config, State};

/// How long a start or a connection may take before it counts as failed.
const WAIT: Duration = Duration::from_secs(30);

pub(crate) fn supported() -> bool {
    // SAFETY: a class method without arguments
    unsafe { VZVirtualMachine::isSupported() }
}

/// A value only ever touched on the VM's queue, carried there from another thread.
struct OnQueue<T>(T);
// SAFETY: the framework's objects are used only on the queue the machine was made with
unsafe impl<T> Send for OnQueue<T> {}
unsafe impl<T> Sync for OnQueue<T> {}

/// A running (or stopped) VM; dropping it ends the machine.
pub struct Vm {
    machine: OnQueue<Retained<VZVirtualMachine>>,
    queue: DispatchRetained<DispatchQueue>,
    /// Held while the VM is this process's: a second owner of the same disks is refused.
    _lock: std::fs::File,
    /// The ports the guest may connect to (`listen`), each listener with its delegate: the
    /// framework holds the delegate weakly, so it lives here.
    listeners: std::sync::Mutex<Vec<OnQueue<(Retained<VZVirtioSocketListener>, Retained<Accepting>)>>>,
}

/// What a connection from the guest is handed to, on a thread of its own.
pub type Accept = Arc<dyn Fn(UnixStream) + Send + Sync>;

/// The delegate of a vsock listener: every connection the guest makes is accepted and handed,
/// as a stream of its own, to the port's [`Accept`].
pub struct AcceptingIvars {
    accept: Accept,
}

objc2::define_class!(
    // SAFETY: NSObject has no subclassing requirements; Accepting does not implement Drop
    #[unsafe(super(NSObject))]
    #[ivars = AcceptingIvars]
    pub struct Accepting;

    unsafe impl NSObjectProtocol for Accepting {}

    unsafe impl VZVirtioSocketListenerDelegate for Accepting {
        #[unsafe(method(listener:shouldAcceptNewConnection:fromSocketDevice:))]
        fn should_accept(
            &self,
            _listener: &VZVirtioSocketListener,
            connection: &VZVirtioSocketConnection,
            _device: &VZVirtioSocketDevice,
        ) -> objc2::runtime::Bool {
            // the connection closes its own descriptor when it goes: keep a copy
            // SAFETY: the connection's descriptor is open while it lives
            let fd = unsafe { libc::dup(connection.fileDescriptor()) };
            if fd < 0 {
                return objc2::runtime::Bool::NO;
            }
            // SAFETY: a descriptor of our own, just made
            let stream = UnixStream::from(unsafe { OwnedFd::from_raw_fd(fd) });
            let accept = self.ivars().accept.clone();
            let spawned = std::thread::Builder::new()
                .name("sill-vm-accepted".into())
                .spawn(move || accept(stream))
                .is_ok();
            objc2::runtime::Bool::new(spawned)
        }
    }
);

impl Accepting {
    fn new(accept: Accept) -> Retained<Self> {
        let this = Self::alloc().set_ivars(AcceptingIvars { accept });
        // SAFETY: NSObject's init on a freshly allocated object
        unsafe { objc2::msg_send![super(this), init] }
    }
}

impl Vm {
    /// Make the machine from `config` and start it; returns once the framework has started it
    /// (the guest boots on: `connect` waits for what it needs).
    pub fn start(config: &Config) -> Result<Self> {
        let lock = lock(&config.dir)?;
        let queue = DispatchQueue::new("sill-vm", DispatchQueueAttr::SERIAL);
        let configuration = OnQueue(configuration(config)?);
        let (tx, rx) = mpsc::channel();
        let mut made: Option<Retained<VZVirtualMachine>> = None;
        let out = OnQueue(&mut made as *mut Option<Retained<VZVirtualMachine>>);
        let bound = OnQueue(queue.clone());
        queue.exec_sync(move || {
            let (configuration, bound, out) = (&configuration, &bound, &out);
            // SAFETY: the machine is made on, and bound to, this queue
            let machine = unsafe {
                VZVirtualMachine::initWithConfiguration_queue(
                    VZVirtualMachine::alloc(),
                    &configuration.0,
                    &bound.0,
                )
            };
            let done = RcBlock::new(move |error: *mut NSError| {
                // SAFETY: the framework passes a valid error or none
                let _ = tx.send(unsafe { error.as_ref() }.map(said));
            });
            // SAFETY: on the machine's queue; `out` outlives this synchronous call
            unsafe {
                machine.startWithCompletionHandler(&done);
                *out.0 = Some(machine);
            }
        });
        let machine = made.expect("the machine made on the queue");
        match rx.recv_timeout(WAIT) {
            Ok(None) => {}
            Ok(Some(error)) => bail!("the VM did not start: {error}"),
            Err(_) => bail!("the VM did not start within {} s", WAIT.as_secs()),
        }
        Ok(Self {
            machine: OnQueue(machine),
            queue,
            _lock: lock,
            listeners: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub fn state(&self) -> State {
        let mut state = State::Error;
        let machine = &self.machine;
        let out = OnQueue(&mut state as *mut State);
        self.queue.exec_sync(move || {
            let out = &out;
            // SAFETY: on the machine's queue; `out` outlives this synchronous call
            let now = unsafe { machine.0.state() };
            unsafe {
                *out.0 = match now {
                    VZVirtualMachineState::Stopped => State::Stopped,
                    VZVirtualMachineState::Starting => State::Starting,
                    VZVirtualMachineState::Running => State::Running,
                    VZVirtualMachineState::Stopping => State::Stopping,
                    VZVirtualMachineState::Error => State::Error,
                    _ => State::Other,
                };
            }
        });
        state
    }

    /// Ask the guest to shut down (the ACPI power button): it stops itself, cleanly.
    pub fn request_stop(&self) -> Result<()> {
        let machine = &self.machine;
        let mut result = Ok(());
        let out = OnQueue(&mut result as *mut Result<()>);
        self.queue.exec_sync(move || {
            let out = &out;
            // SAFETY: on the machine's queue; `out` outlives this synchronous call
            let asked = unsafe { machine.0.requestStopWithError() };
            unsafe { *out.0 = asked.map_err(|e| anyhow!("{}", said(&e))) };
        });
        result
    }

    /// Stop the machine at once, as pulling its plug would.
    pub fn force_stop(&self) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        let machine = &self.machine;
        self.queue.exec_sync(move || {
            let done = RcBlock::new(move |error: *mut NSError| {
                // SAFETY: the framework passes a valid error or none
                let _ = tx.send(unsafe { error.as_ref() }.map(said));
            });
            // SAFETY: on the machine's queue
            unsafe { machine.0.stopWithCompletionHandler(&done) };
        });
        match rx.recv_timeout(WAIT) {
            Ok(None) => Ok(()),
            Ok(Some(error)) => bail!("the VM did not stop: {error}"),
            Err(_) => bail!("the VM did not stop within {} s", WAIT.as_secs()),
        }
    }

    /// Ask the guest to shut down and wait up to `patience` for it; then stop it at once.
    pub fn shut_down(&self, patience: Duration) -> Result<()> {
        if self.state() == State::Stopped {
            return Ok(());
        }
        let deadline = std::time::Instant::now() + patience;
        if self.request_stop().is_ok() {
            while std::time::Instant::now() < deadline {
                if self.state() == State::Stopped {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        self.force_stop()
    }

    /// A stream to `port` in the guest (vsock); fails while nothing there listens yet.
    pub fn connect(&self, port: u32) -> Result<UnixStream> {
        let (tx, rx) = mpsc::channel::<Result<OwnedFd, String>>();
        let machine = &self.machine;
        self.queue.exec_sync(move || {
            // SAFETY: on the machine's queue
            let devices = unsafe { machine.0.socketDevices() };
            let Some(device) = devices.firstObject() else {
                let _ = tx.send(Err("the VM has no socket device".into()));
                return;
            };
            // SAFETY: the configuration's only socket device is a virtio one
            let device: Retained<VZVirtioSocketDevice> = unsafe { Retained::cast_unchecked(device) };
            let done = RcBlock::new(
                move |connection: *mut VZVirtioSocketConnection, error: *mut NSError| {
                    // SAFETY: the framework passes a valid connection or a valid error
                    let result = match unsafe { (connection.as_ref(), error.as_ref()) } {
                        (Some(connection), _) => {
                            // the connection closes its own descriptor when it goes: keep a copy
                            let fd = unsafe { libc::dup(connection.fileDescriptor()) };
                            if fd < 0 {
                                Err(std::io::Error::last_os_error().to_string())
                            } else {
                                Ok(unsafe { OwnedFd::from_raw_fd(fd) })
                            }
                        }
                        (None, Some(error)) => Err(said(error)),
                        (None, None) => Err("no connection".into()),
                    };
                    let _ = tx.send(result);
                },
            );
            // SAFETY: on the machine's queue
            unsafe { device.connectToPort_completionHandler(port, &done) };
        });
        match rx.recv_timeout(WAIT) {
            Ok(Ok(fd)) => Ok(UnixStream::from(fd)),
            Ok(Err(error)) => bail!("port {port} in the VM: {error}"),
            Err(_) => bail!("port {port} in the VM: no answer within {} s", WAIT.as_secs()),
        }
    }

    /// Give the guest `folder` of this Mac under share slot `slot` (`sill-mac-<slot>`), or take
    /// the slot's folder away (`None`). The other slots, and what is mounted from them, are left
    /// as they are.
    pub fn share_slot(&self, slot: usize, folder: Option<&Path>) -> Result<()> {
        if slot >= crate::SHARE_SLOTS {
            bail!("share slot {slot} out of {}", crate::SHARE_SLOTS);
        }
        // SAFETY: the framework's share objects, made before they are handed to the machine
        let share: Option<Retained<VZDirectoryShare>> = folder.map(|path| unsafe {
            let directory = VZSharedDirectory::initWithURL_readOnly(VZSharedDirectory::alloc(), &url(path), false);
            Retained::into_super(VZSingleDirectoryShare::initWithDirectory(VZSingleDirectoryShare::alloc(), &directory))
        });
        let share = OnQueue(share);
        let machine = &self.machine;
        let mut result = Ok(());
        let out = OnQueue(&mut result as *mut Result<()>);
        self.queue.exec_sync(move || {
            let (share, out) = (&share, &out);
            // SAFETY: on the machine's queue; `out` outlives this synchronous call
            unsafe {
                let devices = machine.0.directorySharingDevices();
                *out.0 = if slot < devices.count() {
                    let device: Retained<VZVirtioFileSystemDevice> =
                        Retained::cast_unchecked(devices.objectAtIndex(slot));
                    device.setShare(share.0.as_deref());
                    Ok(())
                } else {
                    Err(anyhow!("the VM has no share slot {slot}"))
                };
            }
        });
        result
    }

    /// Let the guest connect to `port` on the host (vsock address 2): each connection is handed
    /// to `accept`. Anything in the guest can connect; what it may ask is the caller's to check.
    pub fn listen(&self, port: u32, accept: Accept) -> Result<()> {
        let machine = &self.machine;
        let mut made: Option<(Retained<VZVirtioSocketListener>, Retained<Accepting>)> = None;
        let out = OnQueue(&mut made as *mut Option<(Retained<VZVirtioSocketListener>, Retained<Accepting>)>);
        let accept = OnQueue(accept);
        self.queue.exec_sync(move || {
            let (out, accept) = (&out, &accept);
            // SAFETY: on the machine's queue; `out` outlives this synchronous call
            unsafe {
                let devices = machine.0.socketDevices();
                let Some(device) = devices.firstObject() else { return };
                let device: Retained<VZVirtioSocketDevice> = Retained::cast_unchecked(device);
                let delegate = Accepting::new(accept.0.clone());
                let listener = VZVirtioSocketListener::new();
                listener.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*delegate)));
                device.setSocketListener_forPort(&listener, port);
                *out.0 = Some((listener, delegate));
            }
        });
        let made = made.ok_or_else(|| anyhow!("the VM has no socket device"))?;
        self.listeners.lock().unwrap().push(OnQueue(made));
        Ok(())
    }

    /// Wait until the guest's SSH answers on vsock, up to `patience`.
    pub fn wait_for_ssh(&self, patience: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + patience;
        loop {
            if let Ok(mut stream) = self.connect(crate::SSH_PORT) {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut banner = [0u8; 4];
                if stream.read_exact(&mut banner).is_ok() && &banner == b"SSH-" {
                    return Ok(());
                }
            }
            if self.state() != State::Running && self.state() != State::Starting {
                bail!("the VM stopped while starting ({:?})", self.state());
            }
            if std::time::Instant::now() >= deadline {
                bail!("the VM's SSH did not answer within {} s", patience.as_secs());
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        if self.state() != State::Stopped {
            let _ = self.force_stop();
        }
    }
}

/// Let OpenSSH reach the guest: each connection to the local socket `path` is carried to the
/// guest's SSH over vsock (`ssh -o ProxyCommand='nc -U PATH'`). Runs until the VM goes.
pub fn serve_ssh(vm: Arc<Vm>, path: &Path) -> Result<std::thread::JoinHandle<()>> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).with_context(|| path.display().to_string())?;
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    let path = path.to_path_buf();
    Ok(std::thread::Builder::new()
        .name("sill-vm-ssh".into())
        .spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                if Arc::strong_count(&vm) == 1 && vm.state() == State::Stopped {
                    break;
                }
                let guest = match vm.connect(crate::SSH_PORT) {
                    Ok(guest) => guest,
                    Err(_) => continue, // the client sees its connection close
                };
                relay(client, guest);
            }
            let _ = std::fs::remove_file(path);
        })?)
}

/// Carry bytes both ways until either side is done.
fn relay(a: UnixStream, b: UnixStream) {
    let pair = |mut from: UnixStream, mut to: UnixStream| {
        move || {
            let mut buf = [0u8; 64 * 1024];
            loop {
                match from.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if to.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = to.shutdown(std::net::Shutdown::Write);
        }
    };
    let (Ok(a2), Ok(b2)) = (a.try_clone(), b.try_clone()) else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("sill-vm-relay".into())
        .spawn(pair(a, b2));
    let _ = std::thread::Builder::new()
        .name("sill-vm-relay".into())
        .spawn(pair(b, a2));
}

/// The directory is this process's VM: a second process starting the same disks is refused here,
/// before the framework would refuse their attachment less clearly.
fn lock(dir: &Path) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("lock"))?;
    // SAFETY: a valid descriptor
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("the VM in {} is already running", dir.display());
    }
    Ok(file)
}

fn said(error: &NSError) -> String {
    error.localizedDescription().to_string()
}

fn url(path: &Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
}

/// The machine: EFI boot from `system.raw`, the data disks and seed when present, NAT network,
/// a vsock device, the console into `console.log`, and an identity and network address kept
/// beside the disks.
fn configuration(config: &Config) -> Result<Retained<VZVirtualMachineConfiguration>> {
    let dir = &config.dir;
    let system = dir.join("system.raw");
    if !system.exists() {
        bail!("no system image at {}", system.display());
    }
    // SAFETY: the framework's configuration objects, made and filled before the machine exists
    unsafe {
        let platform = VZGenericPlatformConfiguration::new();
        let id = identity(&dir.join("machine-id"))?;
        platform.setMachineIdentifier(&id);

        let boot = VZEFIBootLoader::new();
        let vars = dir.join("efivars");
        let store = if vars.exists() {
            VZEFIVariableStore::initWithURL(VZEFIVariableStore::alloc(), &url(&vars))
        } else {
            VZEFIVariableStore::initCreatingVariableStoreAtURL_options_error(
                VZEFIVariableStore::alloc(),
                &url(&vars),
                VZEFIVariableStoreInitializationOptions::empty(),
            )
            .map_err(|e| anyhow!("{}", said(&e)))?
        };
        boot.setVariableStore(Some(&store));

        // "cached", not the framework's default "automatic": with "automatic", Linux guests on
        // Apple silicon get corrupted data from their virtio disks (2026-11-12: programs in the
        // guest crashed with illegal instructions while the files on disk were intact; Lima and
        // UTM met the same and chose "cached"). `SILL_VM_DISK_CACHE` overrides it for tests.
        let caching = match std::env::var("SILL_VM_DISK_CACHE").as_deref() {
            Ok("automatic") => VZDiskImageCachingMode::Automatic,
            Ok("uncached") => VZDiskImageCachingMode::Uncached,
            _ => VZDiskImageCachingMode::Cached,
        };
        let disk = |path: &PathBuf, read_only: bool, id: Option<&str>| {
            let attachment = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_cachingMode_synchronizationMode_error(
                VZDiskImageStorageDeviceAttachment::alloc(),
                &url(path),
                read_only,
                caching,
                VZDiskImageSynchronizationMode::Full,
            )
            .map_err(|e| anyhow!("{}: {}", path.display(), said(&e)))?;
            let device = VZVirtioBlockDeviceConfiguration::initWithAttachment(
                VZVirtioBlockDeviceConfiguration::alloc(),
                &attachment,
            );
            if let Some(id) = id {
                device.setBlockDeviceIdentifier(&NSString::from_str(id));
            }
            let device: Retained<VZStorageDeviceConfiguration> = Retained::into_super(device);
            anyhow::Ok(device)
        };
        let mut disks = vec![disk(&system, false, None)?];
        for (file, id) in [("home.raw", "sill-home"), ("state.raw", "sill-state")] {
            let path = dir.join(file);
            if path.exists() {
                disks.push(disk(&path, false, Some(id))?);
            }
        }
        let seed = dir.join("seed.iso");
        if seed.exists() {
            disks.push(disk(&seed, true, None)?);
        }

        let net = VZVirtioNetworkDeviceConfiguration::new();
        net.setAttachment(Some(&VZNATNetworkDeviceAttachment::new()));
        // the same address every start: the guest's network setup recognises its interface by it
        let mac = mac_address(&dir.join("mac"))?;
        net.setMACAddress(&mac);

        let console = VZVirtioConsoleDeviceSerialPortConfiguration::new();
        let log = VZFileSerialPortAttachment::initWithURL_append_error(
            VZFileSerialPortAttachment::alloc(),
            &url(&dir.join("console.log")),
            false,
        )
        .map_err(|e| anyhow!("{}", said(&e)))?;
        console.setAttachment(Some(&log));

        let machine = VZVirtualMachineConfiguration::new();
        machine.setPlatform(&platform);
        machine.setBootLoader(Some(&boot));
        machine.setCPUCount(config.cpus);
        machine.setMemorySize(config.memory_gb << 30);
        machine.setStorageDevices(&NSArray::from_retained_slice(&disks));
        machine.setNetworkDevices(&NSArray::from_retained_slice(&[Retained::into_super(net)]));
        machine.setSerialPorts(&NSArray::from_retained_slice(&[Retained::into_super(console)]));
        // the Mac's folders: a device per slot, each empty until given a folder
        let slots: Vec<Retained<VZDirectorySharingDeviceConfiguration>> = (0..crate::SHARE_SLOTS)
            .map(|slot| {
                let device = VZVirtioFileSystemDeviceConfiguration::initWithTag(
                    VZVirtioFileSystemDeviceConfiguration::alloc(),
                    &NSString::from_str(&crate::share_tag(slot)),
                );
                device.setShare(Some(&VZMultipleDirectoryShare::new()));
                Retained::into_super(device)
            })
            .collect();
        machine.setDirectorySharingDevices(&NSArray::from_retained_slice(&slots));
        machine.setSocketDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            VZVirtioSocketDeviceConfiguration::new(),
        )]));
        machine.setEntropyDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            VZVirtioEntropyDeviceConfiguration::new(),
        )]));
        machine
            .validateWithError()
            .map_err(|e| anyhow!("the VM's configuration: {}", said(&e)))?;
        Ok(machine)
    }
}

/// The network interface's address, made once (random, locally administered) and kept.
fn mac_address(path: &Path) -> Result<Retained<VZMACAddress>> {
    // SAFETY: the framework's address, from its own string form or new
    unsafe {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Some(address) =
                VZMACAddress::initWithString(VZMACAddress::alloc(), &NSString::from_str(text.trim()))
        {
            return Ok(address);
        }
        let address = VZMACAddress::randomLocallyAdministeredAddress();
        std::fs::write(path, address.string().to_string())?;
        Ok(address)
    }
}

/// The machine's identity, made once and kept, so the guest stays the same machine.
fn identity(path: &Path) -> Result<Retained<VZGenericMachineIdentifier>> {
    // SAFETY: the framework's identifier, from its own data representation or new
    unsafe {
        if let Ok(bytes) = std::fs::read(path)
            && let Some(id) = VZGenericMachineIdentifier::initWithDataRepresentation(
                VZGenericMachineIdentifier::alloc(),
                &NSData::with_bytes(&bytes),
            )
        {
            return Ok(id);
        }
        let id = VZGenericMachineIdentifier::new();
        std::fs::write(path, id.dataRepresentation().to_vec())?;
        Ok(id)
    }
}
