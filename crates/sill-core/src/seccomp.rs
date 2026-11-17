//! The jail's second fence: a seccomp filter bwrap installs before it execs the shim
//! (`--seccomp <fd>`, security.md S18). bwrap's namespaces and the empty capability set are the
//! boundary; the filter takes away the syscalls behind nearly every kernel escape of the container
//! years — mount API, keyrings, bpf, userfaultfd, perf, io_uring, module loading, new namespaces —
//! none of which an agent, its shell or its MCP servers have a use for. Everything else is
//! allowed: this is a deny-list, so tools keep working; ptrace stays (strace, gdb).
//!
//! The program is classic BPF built by hand (no libseccomp), from a table of syscall numbers per
//! architecture: x86_64 and aarch64 (Sill.app's VM). On another architecture [`program`] is `None`
//! and the jail runs without the filter, with a warning.

use std::os::fd::OwnedFd;

use anyhow::{Context, Result};

/// `struct sock_filter`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Insn {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

// BPF opcodes
const LD_W_ABS: u16 = 0x20;
const ALU_AND_K: u16 = 0x54;
const JMP_JEQ_K: u16 = 0x15;
const JMP_JGE_K: u16 = 0x35;
const RET_K: u16 = 0x06;

// struct seccomp_data offsets
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const fn off_arg(i: u32) -> u32 {
    16 + 8 * i // low 32 bits (little-endian)
}

const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
const AUDIT_ARCH_AARCH64: u32 = 0xc000_00b7;
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

pub const RET_ALLOW: u32 = 0x7fff_0000;
pub const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const EPERM: u32 = 1;
const ENOSYS: u32 = 38;

/// Namespace flags of `clone`/`unshare`. `CLONE_NEWTIME` (0x80) is only for `unshare` and `clone3`:
/// in `clone`'s flags that bit belongs to the exit signal.
const NS_FLAGS: u32 = 0x0002_0000 /* NEWNS */ | 0x0200_0000 /* NEWCGROUP */ | 0x0400_0000 /* NEWUTS */
    | 0x0800_0000 /* NEWIPC */ | 0x1000_0000 /* NEWUSER */ | 0x2000_0000 /* NEWPID */ | 0x4000_0000 /* NEWNET */;
const CLONE_NEWTIME: u32 = 0x80;

/// What the filter knows of one architecture: its syscall numbers differ, the decisions do not.
pub struct Abi {
    /// `AUDIT_ARCH_*`: a syscall made in any other ABI kills the process.
    arch: u32,
    /// x86_64 only: numbers with the x32 bit are a second ABI under the same arch value.
    x32: bool,
    /// Refused with EPERM.
    denied: &'static [(&'static str, u32)],
    clone: u32,
    unshare: u32,
    clone3: u32,
}

pub const X86_64: Abi = Abi {
    arch: AUDIT_ARCH_X86_64,
    x32: true,
    denied: &[
        // mounts, the classic and the new API
        ("mount", 165),
        ("umount2", 166),
        ("pivot_root", 155),
        ("chroot", 161),
        ("open_tree", 428),
        ("move_mount", 429),
        ("fsopen", 430),
        ("fsconfig", 431),
        ("fsmount", 432),
        ("fspick", 433),
        ("mount_setattr", 442),
        ("name_to_handle_at", 303),
        ("open_by_handle_at", 304),
        // kernel: modules, kexec, reboot, swap, accounting, clocks, quotas, host names
        ("init_module", 175),
        ("finit_module", 313),
        ("delete_module", 176),
        ("create_module", 174),
        ("query_module", 178),
        ("get_kernel_syms", 177),
        ("kexec_load", 246),
        ("kexec_file_load", 320),
        ("reboot", 169),
        ("swapon", 167),
        ("swapoff", 168),
        ("acct", 163),
        ("settimeofday", 164),
        ("clock_settime", 227),
        ("clock_adjtime", 305),
        ("adjtimex", 159),
        ("quotactl", 179),
        ("quotactl_fd", 443),
        ("sethostname", 170),
        ("setdomainname", 171),
        ("vhangup", 153),
        ("syslog", 103),
        ("ioperm", 173),
        ("iopl", 172),
        ("sysfs", 139),
        ("_sysctl", 156),
        ("uselib", 134),
        ("ustat", 136),
        ("lookup_dcookie", 212),
        ("nfsservctl", 180),
        // the escape surface proper
        ("keyctl", 250),
        ("add_key", 248),
        ("request_key", 249),
        ("bpf", 321),
        ("userfaultfd", 323),
        ("perf_event_open", 298),
        ("fanotify_init", 300),
        ("memfd_secret", 447),
        ("io_uring_setup", 425),
        ("io_uring_enter", 426),
        ("io_uring_register", 427),
        // joining another namespace
        ("setns", 308),
    ],
    clone: 56,
    unshare: 272,
    clone3: 435,
};

/// The same list in the generic numbering aarch64 uses; x86's legacy calls (`ioperm`, `sysfs`,
/// `uselib`, …) do not exist there.
pub const AARCH64: Abi = Abi {
    arch: AUDIT_ARCH_AARCH64,
    x32: false,
    denied: &[
        // mounts, the classic and the new API
        ("mount", 40),
        ("umount2", 39),
        ("pivot_root", 41),
        ("chroot", 51),
        ("open_tree", 428),
        ("move_mount", 429),
        ("fsopen", 430),
        ("fsconfig", 431),
        ("fsmount", 432),
        ("fspick", 433),
        ("mount_setattr", 442),
        ("name_to_handle_at", 264),
        ("open_by_handle_at", 265),
        // kernel: modules, kexec, reboot, swap, accounting, clocks, quotas, host names
        ("init_module", 105),
        ("finit_module", 273),
        ("delete_module", 106),
        ("kexec_load", 104),
        ("kexec_file_load", 294),
        ("reboot", 142),
        ("swapon", 224),
        ("swapoff", 225),
        ("acct", 89),
        ("settimeofday", 170),
        ("clock_settime", 112),
        ("clock_adjtime", 266),
        ("adjtimex", 171),
        ("quotactl", 60),
        ("quotactl_fd", 443),
        ("sethostname", 161),
        ("setdomainname", 162),
        ("vhangup", 58),
        ("syslog", 116),
        ("lookup_dcookie", 18),
        ("nfsservctl", 42),
        // the escape surface proper
        ("keyctl", 219),
        ("add_key", 217),
        ("request_key", 218),
        ("bpf", 280),
        ("userfaultfd", 282),
        ("perf_event_open", 241),
        ("fanotify_init", 262),
        ("memfd_secret", 447),
        ("io_uring_setup", 425),
        ("io_uring_enter", 426),
        ("io_uring_register", 427),
        // joining another namespace
        ("setns", 268),
    ],
    clone: 220,
    unshare: 97,
    clone3: 435,
};

/// The architecture this build runs on, when the filter is written for it.
pub fn native() -> Option<&'static Abi> {
    if cfg!(target_arch = "x86_64") {
        Some(&X86_64)
    } else if cfg!(target_arch = "aarch64") {
        Some(&AARCH64)
    } else {
        None
    }
}

/// The filter, or `None` on an architecture it is not written for. `allow_userns` (the policy's
/// `jail.allow_userns`) keeps `clone`/`unshare` with namespace flags: an agent's own bwrap needs them.
pub fn program(allow_userns: bool) -> Option<Vec<Insn>> {
    native().map(|abi| program_for(abi, allow_userns))
}

/// The filter for one architecture's numbers.
pub fn program_for(abi: &Abi, allow_userns: bool) -> Vec<Insn> {
    let stmt = |code, k| Insn { code, jt: 0, jf: 0, k };
    // Jumps are forward by a u8 count, so the program is laid out with the shared return
    // instructions at the end and every test jumping to them; the counts are resolved last.
    #[derive(Clone, Copy, PartialEq)]
    enum To {
        Next,
        Eperm,
        Enosys,
        Kill,
        Clone,
        Unshare,
    }
    let mut body: Vec<(Insn, To, To)> = Vec::new(); // (insn, where jt goes, where jf goes)
    let jump = |body: &mut Vec<_>, code, k, jt, jf| body.push((stmt(code, k), jt, jf));
    // another ABI's syscall numbers mean nothing here: a foreign binary is killed
    body.push((stmt(LD_W_ABS, OFF_ARCH), To::Next, To::Next));
    jump(&mut body, JMP_JEQ_K, abi.arch, To::Next, To::Kill);
    body.push((stmt(LD_W_ABS, OFF_NR), To::Next, To::Next));
    if abi.x32 {
        jump(&mut body, JMP_JGE_K, X32_SYSCALL_BIT, To::Enosys, To::Next);
    }
    for (_, nr) in abi.denied {
        jump(&mut body, JMP_JEQ_K, *nr, To::Eperm, To::Next);
    }
    // clone3 takes its flags in a struct the filter cannot read: ENOSYS, and libc falls back
    // to clone, which is checked
    jump(&mut body, JMP_JEQ_K, abi.clone3, To::Enosys, To::Next);
    if !allow_userns {
        jump(&mut body, JMP_JEQ_K, abi.clone, To::Clone, To::Next);
        jump(&mut body, JMP_JEQ_K, abi.unshare, To::Unshare, To::Next);
    }
    body.push((stmt(RET_K, RET_ALLOW), To::Next, To::Next));
    let eperm_at = body.len();
    body.push((stmt(RET_K, RET_ERRNO | EPERM), To::Next, To::Next));
    let enosys_at = body.len();
    body.push((stmt(RET_K, RET_ERRNO | ENOSYS), To::Next, To::Next));
    let kill_at = body.len();
    body.push((stmt(RET_K, RET_KILL_PROCESS), To::Next, To::Next));
    let mut clone_at = 0;
    let mut unshare_at = 0;
    if !allow_userns {
        // the flags are the first argument of both, on either architecture
        clone_at = body.len();
        body.push((stmt(LD_W_ABS, off_arg(0)), To::Next, To::Next));
        body.push((stmt(ALU_AND_K, NS_FLAGS), To::Next, To::Next));
        jump(&mut body, JMP_JEQ_K, 0, To::Next, To::Next); // resolved below: 0 → allow, else → eperm
        body.push((stmt(RET_K, RET_ALLOW), To::Next, To::Next));
        body.push((stmt(RET_K, RET_ERRNO | EPERM), To::Next, To::Next));
        unshare_at = body.len();
        body.push((stmt(LD_W_ABS, off_arg(0)), To::Next, To::Next));
        body.push((stmt(ALU_AND_K, NS_FLAGS | CLONE_NEWTIME), To::Next, To::Next));
        jump(&mut body, JMP_JEQ_K, 0, To::Next, To::Next);
        body.push((stmt(RET_K, RET_ALLOW), To::Next, To::Next));
        body.push((stmt(RET_K, RET_ERRNO | EPERM), To::Next, To::Next));
    }
    let target = |to: To, at: usize| -> u8 {
        let idx = match to {
            To::Next => at + 1,
            To::Eperm => eperm_at,
            To::Enosys => enosys_at,
            To::Kill => kill_at,
            To::Clone => clone_at,
            To::Unshare => unshare_at,
        };
        u8::try_from(idx - at - 1).expect("seccomp program: jump too long")
    };
    let mut out = Vec::with_capacity(body.len());
    for (i, (mut insn, jt, jf)) in body.iter().copied().enumerate() {
        if insn.code == JMP_JEQ_K || insn.code == JMP_JGE_K {
            // the two "flags & NS == 0" tests: jt → the allow right after, jf → the eperm after it
            let (jt, jf) = if jt == To::Next && jf == To::Next { (0, 1) } else { (target(jt, i), target(jf, i)) };
            insn.jt = jt;
            insn.jf = jf;
        }
        out.push(insn);
    }
    out
}

/// The program as bwrap reads it: `struct sock_filter` array in native byte order.
pub fn bytes(prog: &[Insn]) -> Vec<u8> {
    let mut v = Vec::with_capacity(prog.len() * 8);
    for i in prog {
        v.extend_from_slice(&i.code.to_ne_bytes());
        v.push(i.jt);
        v.push(i.jf);
        v.extend_from_slice(&i.k.to_ne_bytes());
    }
    v
}

/// A memfd holding the program, inheritable (no CLOEXEC) so a spawned bwrap can read it as
/// `--seccomp <fd>`; `None` when there is no program for this architecture. bwrap reads to EOF,
/// so a second spawn from the same fd needs [`rewind`] first.
pub fn memfd(allow_userns: bool) -> Result<Option<OwnedFd>> {
    let Some(prog) = program(allow_userns) else { return Ok(None) };
    let fd = rustix::fs::memfd_create("sill-seccomp", rustix::fs::MemfdFlags::CLOEXEC).context("memfd_create")?;
    rustix::io::write(&fd, &bytes(&prog)).context("write seccomp program")?;
    rewind(&fd)?;
    rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::empty()).context("fcntl")?;
    Ok(Some(fd))
}

pub fn rewind(fd: &OwnedFd) -> Result<()> {
    rustix::fs::seek(fd, rustix::fs::SeekFrom::Start(0)).context("seek")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Just enough classic BPF to run the program over a `seccomp_data`.
    fn run(prog: &[Insn], nr: u32, arch: u32, arg0: u64) -> u32 {
        let data = |off: u32| -> u32 {
            match off {
                OFF_NR => nr,
                OFF_ARCH => arch,
                16 => arg0 as u32,
                20 => (arg0 >> 32) as u32,
                _ => 0,
            }
        };
        let mut a = 0u32;
        let mut pc = 0usize;
        loop {
            let i = prog[pc];
            pc += 1;
            match i.code {
                LD_W_ABS => a = data(i.k),
                ALU_AND_K => a &= i.k,
                JMP_JEQ_K => pc += if a == i.k { i.jt } else { i.jf } as usize,
                JMP_JGE_K => pc += if a >= i.k { i.jt } else { i.jf } as usize,
                RET_K => return i.k,
                c => panic!("opcode {c:#x}"),
            }
            assert!(pc < prog.len(), "ran off the end");
        }
    }

    #[test]
    fn decides_as_documented() {
        let eperm = RET_ERRNO | EPERM;
        let enosys = RET_ERRNO | ENOSYS;
        // (the table, read, ptrace, setns, another ABI's arch value)
        for (abi, read, ptrace, setns, foreign) in [(&X86_64, 0, 101, 308, 0x4000_0003 /* i386 */), (&AARCH64, 63, 117, 268, 0x4000_0028 /* arm */)] {
            let p = program_for(abi, false);
            assert!(p.len() < 256);
            let x = abi.arch;
            assert_eq!(run(&p, read, x, 0), RET_ALLOW);
            assert_eq!(run(&p, ptrace, x, 0), RET_ALLOW);
            for (name, nr) in abi.denied {
                assert_eq!(run(&p, *nr, x, 0), eperm, "{name}");
            }
            assert_eq!(run(&p, abi.clone3, x, 0), enosys);
            assert_eq!(run(&p, read, foreign, 0), RET_KILL_PROCESS);
            // a thread, a fork: fine; a new mount or user namespace: not
            assert_eq!(run(&p, abi.clone, x, 0x3d0f00 | 17), RET_ALLOW);
            assert_eq!(run(&p, abi.clone, x, 0x1000_0000 | 17), eperm);
            assert_eq!(run(&p, abi.clone, x, 0x0002_0000), eperm);
            assert_eq!(run(&p, abi.unshare, x, 0x400 /* CLONE_FILES */), RET_ALLOW);
            assert_eq!(run(&p, abi.unshare, x, CLONE_NEWTIME as u64), eperm);
            assert_eq!(run(&p, abi.unshare, x, 0x1000_0000), eperm);
            // the policy that allows user namespaces lets both through, setns stays refused
            let u = program_for(abi, true);
            assert_eq!(run(&u, abi.clone, x, 0x1000_0000 | 17), RET_ALLOW);
            assert_eq!(run(&u, abi.unshare, x, 0x1000_0000), RET_ALLOW);
            assert_eq!(run(&u, setns, x, 0), eperm);
            assert_eq!(bytes(&p).len(), p.len() * 8);
        }
        assert_eq!(run(&program_for(&X86_64, false), X32_SYSCALL_BIT, AUDIT_ARCH_X86_64, 0), enosys, "x32 ABI");
        // each architecture's ABI is foreign to the other's filter
        assert_eq!(run(&program_for(&AARCH64, false), 0, AUDIT_ARCH_X86_64, 0), RET_KILL_PROCESS);
    }

    /// The tables against the numbers the kernel this is built for uses, by name.
    #[test]
    fn the_native_table_names_the_kernels_numbers() {
        let Some(abi) = native() else { return };
        let Some(header) = ["/usr/include/asm/unistd_64.h", "/usr/include/x86_64-linux-gnu/asm/unistd_64.h", "/usr/include/aarch64-linux-gnu/asm/unistd_64.h"]
            .iter()
            .filter(|p| p.contains(std::env::consts::ARCH) || !p.contains("-linux-gnu"))
            .find_map(|p| std::fs::read_to_string(p).ok())
        else {
            return; // no kernel headers on this machine
        };
        let nr = |name: &str| -> Option<u32> {
            header.lines().find_map(|l| {
                let mut w = l.split_whitespace();
                (w.next() == Some("#define") && w.next() == Some(&format!("__NR_{name}"))).then(|| w.next()?.parse().ok()).flatten()
            })
        };
        let named = [("clone", abi.clone), ("unshare", abi.unshare), ("clone3", abi.clone3)];
        for (name, have) in abi.denied.iter().chain(&named) {
            match nr(name) {
                Some(want) => assert_eq!(*have, want, "{name}"),
                None => assert!(abi.x32, "{name} is not a syscall here"),
            }
        }
    }
}
