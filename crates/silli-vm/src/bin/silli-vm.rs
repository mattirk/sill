//! `silli-vm DIR`: run the VM in DIR (see the library) until told to stop, for the image build
//! and for trying an image by hand. Prints `ready` once the guest's SSH answers; OpenSSH reaches
//! it through `DIR/ssh.sock` (`ssh -o ProxyCommand='nc -U DIR/ssh.sock' silli@silli-box`). Stops
//! the guest when `SILLI_VM_HOLD` (a path) exists, on Ctrl-C or when the guest powers itself off.
//! `SILLI_VM_SHARE=/path,…` shares Mac folders under the virtiofs tags `silli-mac-0`, `-1`, ….
//! CPUs and memory: `SILLI_VM_CPUS`, `SILLI_VM_MEMORY_GB` (default: sized for this Mac). The binary
//! needs the `com.apple.security.virtualization` entitlement
//! (`packaging/mac/silli-vm.entitlements`).
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("silli-vm: macOS only");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use anyhow::Context;
    use silli_vm::{Config, State, Vm};

    let dir = PathBuf::from(std::env::args().nth(1).context("usage: silli-vm DIR")?);
    let problems = silli_vm::check(&dir);
    for problem in &problems {
        eprintln!("{} {}", problem.what, problem.fix);
    }
    anyhow::ensure!(problems.is_empty(), "this Mac cannot run the VM");
    let mut config = Config::sized_for_this_mac(dir.clone());
    let number = |name: &str| std::env::var(name).ok().and_then(|v| v.parse().ok());
    if let Some(cpus) = number("SILLI_VM_CPUS") {
        config.cpus = cpus as usize;
    }
    if let Some(gb) = number("SILLI_VM_MEMORY_GB") {
        config.memory_gb = gb;
    }
    let started = Instant::now();
    let say = |what: &str| eprintln!("[{:>5.1}s] {what}", started.elapsed().as_secs_f32());

    let vm = Arc::new(Vm::start(&config)?);
    say(&format!("started ({} CPUs, {} GB)", config.cpus, config.memory_gb));
    silli_vm::serve_ssh(vm.clone(), &dir.join("ssh.sock"))?;
    vm.wait_for_ssh(Duration::from_secs(180))?;
    say("ssh answers on vsock");
    // `SILLI_VM_SHARE=/path,…`: Mac folders for share slots 0, 1, … (`silli-mac-<n>`)
    if let Ok(spec) = std::env::var("SILLI_VM_SHARE") {
        for (slot, path) in spec.split(',').filter(|p| !p.is_empty()).enumerate() {
            vm.share_slot(slot, Some(std::path::Path::new(path)))?;
        }
        say("sharing the folders");
    }
    println!("ready");

    let interrupted = Arc::new(AtomicBool::new(false));
    {
        let interrupted = interrupted.clone();
        let mut signals = [libc::SIGINT, libc::SIGTERM];
        // a plain flag: the loop below notices it
        static FLAG: AtomicBool = AtomicBool::new(false);
        extern "C" fn on_signal(_: libc::c_int) {
            FLAG.store(true, Ordering::Relaxed);
        }
        for signal in &mut signals {
            // SAFETY: an async-signal-safe handler that only stores a flag
            unsafe { libc::signal(*signal, on_signal as *const () as libc::sighandler_t) };
        }
        std::thread::spawn(move || {
            while !FLAG.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(200));
            }
            interrupted.store(true, Ordering::Relaxed);
        });
    }
    let hold = std::env::var_os("SILLI_VM_HOLD").map(PathBuf::from);
    loop {
        if vm.state() == State::Stopped {
            say("the guest powered off");
            break;
        }
        let told = hold.as_ref().is_some_and(|path| path.exists());
        if told || interrupted.load(Ordering::Relaxed) {
            say("shutting down");
            vm.shut_down(Duration::from_secs(60))?;
            say("stopped");
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    let _ = std::fs::remove_file(dir.join("ssh.sock"));
    Ok(())
}
