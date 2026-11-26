//! Pure safe-Rust cBPF seccomp filter generator for Linux Bubblewrap.
//!
//! Bubblewrap reads raw `struct sock_filter` bytes from the file descriptor
//! passed to `--seccomp <fd>` and installs the filter right before `execve` of
//! the sandboxed command (after namespace and mount setup has finished).
//!
//! Generating the bytecode directly in safe Rust avoids external crates and
//! `unsafe` FFI while allowing the instruction stream and offsets to be
//! unit-tested deterministically on every platform.

use crate::policy::NetworkAccess;

const BPF_LD: u16 = 0x00;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JMP: u16 = 0x05;
const BPF_JEQ: u16 = 0x10;
const BPF_K: u16 = 0x00;
const BPF_RET: u16 = 0x06;

const LD_ABS_W: u16 = BPF_LD | BPF_W | BPF_ABS;
const JMP_JEQ_K: u16 = BPF_JMP | BPF_JEQ | BPF_K;
const RET_K: u16 = BPF_RET | BPF_K;

const SECCOMP_DATA_NR_OFFSET: u32 = 0;
const SECCOMP_DATA_ARCH_OFFSET: u32 = 4;
// Both x86_64 and aarch64 are little-endian 64-bit ABIs.
const SECCOMP_DATA_ARG0_LO_OFFSET: u32 = 16;
const SECCOMP_DATA_ARG0_HI_OFFSET: u32 = 20;

const SECCOMP_RET_ALLOW: u32 = 0x7FFF_0000;
const SECCOMP_RET_ERRNO_EPERM: u32 = 0x0005_0001;

const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
const AUDIT_ARCH_AARCH64: u32 = 0xC000_00B7;

const AF_UNIX: u32 = 1;
const AF_INET: u32 = 2;
const AF_INET6: u32 = 10;
const AF_NETLINK: u32 = 16;
const AF_VSOCK: u32 = 40;

/// Linux target architecture supported by the cBPF seccomp generator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SeccompArch {
    #[cfg_attr(not(any(target_arch = "x86_64", test)), allow(dead_code))]
    X86_64,
    #[cfg_attr(not(any(target_arch = "aarch64", test)), allow(dead_code))]
    Aarch64,
}

impl SeccompArch {
    /// Detect the current target architecture, if supported.
    #[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
    pub(crate) const fn current() -> Option<Self> {
        #[cfg(target_arch = "x86_64")]
        {
            Some(Self::X86_64)
        }
        #[cfg(target_arch = "aarch64")]
        {
            Some(Self::Aarch64)
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            None
        }
    }

    const fn audit_arch(self) -> u32 {
        match self {
            Self::X86_64 => AUDIT_ARCH_X86_64,
            Self::Aarch64 => AUDIT_ARCH_AARCH64,
        }
    }

    const fn nr_socket(self) -> u32 {
        match self {
            Self::X86_64 => 41,
            Self::Aarch64 => 198,
        }
    }

    const fn blocked_syscalls(self) -> &'static [u32] {
        match self {
            Self::X86_64 => &[
                425, // io_uring_setup
                426, // io_uring_enter
                427, // io_uring_register
                101, // ptrace
                310, // process_vm_readv
                311, // process_vm_writev
                321, // bpf
                246, // kexec_load
                320, // kexec_file_load
            ],
            Self::Aarch64 => &[
                425, // io_uring_setup
                426, // io_uring_enter
                427, // io_uring_register
                117, // ptrace
                270, // process_vm_readv
                271, // process_vm_writev
                280, // bpf
                104, // kexec_load
                294, // kexec_file_load
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

impl SockFilter {
    fn encode(self) -> [u8; 8] {
        let code = self.code.to_ne_bytes();
        let k = self.k.to_ne_bytes();
        [code[0], code[1], self.jt, self.jf, k[0], k[1], k[2], k[3]]
    }
}

#[derive(Clone, Copy)]
enum JumpTarget {
    Next,
    Allow,
    Deny,
}

struct PendingJump {
    k: u32,
    on_true: JumpTarget,
    on_false: JumpTarget,
}

enum Op {
    LoadWord(u32),
    JumpEq(PendingJump),
}

/// Build the cBPF instruction stream for `arch` and `network`.
pub(crate) fn build_filter(arch: SeccompArch, network: NetworkAccess) -> Vec<SockFilter> {
    let mut ops = Vec::with_capacity(22);

    // 1. Verify expected Linux audit arch; reject foreign/compat ABIs.
    ops.push(Op::LoadWord(SECCOMP_DATA_ARCH_OFFSET));
    ops.push(Op::JumpEq(PendingJump {
        k: arch.audit_arch(),
        on_true: JumpTarget::Next,
        on_false: JumpTarget::Deny,
    }));

    // 2. Load syscall number and deny dangerous kernel/process inspection calls.
    ops.push(Op::LoadWord(SECCOMP_DATA_NR_OFFSET));
    for &nr in arch.blocked_syscalls() {
        ops.push(Op::JumpEq(PendingJump {
            k: nr,
            on_true: JumpTarget::Deny,
            on_false: JumpTarget::Next,
        }));
    }

    // 3. Filter `socket(domain, ...)` while leaving `socketpair` allowed.
    ops.push(Op::JumpEq(PendingJump {
        k: arch.nr_socket(),
        on_true: JumpTarget::Next,
        on_false: JumpTarget::Allow,
    }));

    // Ensure upper 32 bits of `args[0]` are zero so a crafted 64-bit register
    // value cannot bypass the `int domain` check after kernel truncation.
    ops.push(Op::LoadWord(SECCOMP_DATA_ARG0_HI_OFFSET));
    ops.push(Op::JumpEq(PendingJump {
        k: 0,
        on_true: JumpTarget::Next,
        on_false: JumpTarget::Deny,
    }));

    ops.push(Op::LoadWord(SECCOMP_DATA_ARG0_LO_OFFSET));
    // Always deny AF_UNIX and AF_VSOCK sockets to prevent connecting to host
    // daemons (such as ssh-agent or Docker) even when NetworkAccess::Open.
    for domain in [AF_UNIX, AF_VSOCK] {
        ops.push(Op::JumpEq(PendingJump {
            k: domain,
            on_true: JumpTarget::Deny,
            on_false: JumpTarget::Next,
        }));
    }
    if network == NetworkAccess::None {
        for domain in [AF_INET, AF_INET6, AF_NETLINK] {
            ops.push(Op::JumpEq(PendingJump {
                k: domain,
                on_true: JumpTarget::Deny,
                on_false: JumpTarget::Next,
            }));
        }
    }

    let allow_index = ops.len();
    let deny_index = allow_index + 1;

    let mut program = Vec::with_capacity(ops.len() + 2);
    for (index, op) in ops.into_iter().enumerate() {
        let instruction = match op {
            Op::LoadWord(offset) => SockFilter {
                code: LD_ABS_W,
                jt: 0,
                jf: 0,
                k: offset,
            },
            Op::JumpEq(jump) => {
                let offset_to = |target: JumpTarget| -> u8 {
                    let destination = match target {
                        JumpTarget::Next => index + 1,
                        JumpTarget::Allow => allow_index,
                        JumpTarget::Deny => deny_index,
                    };
                    let delta = destination - (index + 1);
                    u8::try_from(delta).expect("seccomp jump target fits in u8")
                };
                SockFilter {
                    code: JMP_JEQ_K,
                    jt: offset_to(jump.on_true),
                    jf: offset_to(jump.on_false),
                    k: jump.k,
                }
            }
        };
        program.push(instruction);
    }

    program.push(SockFilter {
        code: RET_K,
        jt: 0,
        jf: 0,
        k: SECCOMP_RET_ALLOW,
    });
    program.push(SockFilter {
        code: RET_K,
        jt: 0,
        jf: 0,
        k: SECCOMP_RET_ERRNO_EPERM,
    });
    program
}

/// Encode the cBPF filter as raw `struct sock_filter` bytes.
pub(crate) fn build_filter_bytes(arch: SeccompArch, network: NetworkAccess) -> Vec<u8> {
    let filter = build_filter(arch, network);
    let mut bytes = Vec::with_capacity(filter.len() * 8);
    for instruction in filter {
        bytes.extend_from_slice(&instruction.encode());
    }
    bytes
}

/// Write the cBPF filter into a pipe and return the read end for `--seccomp`.
#[cfg(target_os = "linux")]
pub(crate) fn seccomp_pipe(
    arch: SeccompArch,
    network: NetworkAccess,
) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::io::Write;

    let bytes = build_filter_bytes(arch, network);
    let (read_end, mut write_end) = std::io::pipe()?;
    write_end.write_all(&bytes)?;
    drop(write_end);
    Ok(std::os::fd::OwnedFd::from(read_end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    struct SeccompInput {
        nr: u32,
        arch: u32,
        arg0: u64,
    }

    fn execute_cbpf(program: &[SockFilter], input: SeccompInput) -> u32 {
        let mut data = [0u8; 64];
        data[0..4].copy_from_slice(&input.nr.to_ne_bytes());
        data[4..8].copy_from_slice(&input.arch.to_ne_bytes());
        let arg0_lo = (input.arg0 & 0xFFFF_FFFF) as u32;
        let arg0_hi = (input.arg0 >> 32) as u32;
        data[16..20].copy_from_slice(&arg0_lo.to_ne_bytes());
        data[20..24].copy_from_slice(&arg0_hi.to_ne_bytes());

        let mut pc = 0usize;
        let mut acc = 0u32;
        loop {
            let insn = program[pc];
            match insn.code {
                LD_ABS_W => {
                    let offset = insn.k as usize;
                    acc = u32::from_ne_bytes(data[offset..offset + 4].try_into().unwrap());
                    pc += 1;
                }
                JMP_JEQ_K => {
                    let jump = if acc == insn.k { insn.jt } else { insn.jf } as usize;
                    pc += 1 + jump;
                }
                RET_K => return insn.k,
                other => panic!("unexpected cBPF opcode: {other:#x}"),
            }
        }
    }

    #[test]
    fn current_arch_matches_expected_target() {
        #[cfg(target_arch = "x86_64")]
        assert_eq!(SeccompArch::current(), Some(SeccompArch::X86_64));
        #[cfg(target_arch = "aarch64")]
        assert_eq!(SeccompArch::current(), Some(SeccompArch::Aarch64));
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        assert_eq!(SeccompArch::current(), None);
    }

    #[test]
    fn encoded_bytes_are_eight_byte_aligned_sock_filters() {
        for arch in [SeccompArch::X86_64, SeccompArch::Aarch64] {
            for network in [NetworkAccess::None, NetworkAccess::Open] {
                let program = build_filter(arch, network);
                let bytes = build_filter_bytes(arch, network);
                assert_eq!(bytes.len(), program.len() * 8);
                assert_eq!(bytes.len() % 8, 0);
                let expected_len = match network {
                    NetworkAccess::Open => 20,
                    NetworkAccess::None => 23,
                };
                assert_eq!(program.len(), expected_len);
            }
        }
    }

    #[test]
    fn filter_blocks_wrong_arch_and_dangerous_syscalls() {
        for arch in [SeccompArch::X86_64, SeccompArch::Aarch64] {
            let program = build_filter(arch, NetworkAccess::Open);
            let wrong_arch = if arch == SeccompArch::X86_64 {
                AUDIT_ARCH_AARCH64
            } else {
                AUDIT_ARCH_X86_64
            };
            assert_eq!(
                execute_cbpf(
                    &program,
                    SeccompInput {
                        nr: 0,
                        arch: wrong_arch,
                        arg0: 0,
                    },
                ),
                SECCOMP_RET_ERRNO_EPERM
            );
            // 32-bit x86 compat arch must also be rejected.
            assert_eq!(
                execute_cbpf(
                    &program,
                    SeccompInput {
                        nr: 1,
                        arch: 0x4000_0003,
                        arg0: 0,
                    },
                ),
                SECCOMP_RET_ERRNO_EPERM
            );

            for &nr in arch.blocked_syscalls() {
                assert_eq!(
                    execute_cbpf(
                        &program,
                        SeccompInput {
                            nr,
                            arch: arch.audit_arch(),
                            arg0: 0,
                        },
                    ),
                    SECCOMP_RET_ERRNO_EPERM,
                    "blocked syscall {nr} on {arch:?}"
                );
            }
        }
    }

    #[test]
    fn filter_enforces_socket_rules_and_allows_socketpair_and_ordinary_syscalls() {
        let cases = [
            (SeccompArch::X86_64, 53u32, 0u32),    // socketpair, read
            (SeccompArch::Aarch64, 199u32, 63u32), // socketpair, read
        ];

        for (arch, nr_socketpair, nr_read) in cases {
            let open = build_filter(arch, NetworkAccess::Open);
            let none = build_filter(arch, NetworkAccess::None);
            let audit = arch.audit_arch();
            let nr_socket = arch.nr_socket();

            // AF_UNIX and AF_VSOCK are blocked in both modes.
            for domain in [AF_UNIX, AF_VSOCK] {
                assert_eq!(
                    execute_cbpf(
                        &open,
                        SeccompInput {
                            nr: nr_socket,
                            arch: audit,
                            arg0: u64::from(domain),
                        },
                    ),
                    SECCOMP_RET_ERRNO_EPERM
                );
                assert_eq!(
                    execute_cbpf(
                        &none,
                        SeccompInput {
                            nr: nr_socket,
                            arch: audit,
                            arg0: u64::from(domain),
                        },
                    ),
                    SECCOMP_RET_ERRNO_EPERM
                );
            }

            // Spoofed upper 32 bits in `args[0]` must not bypass `AF_UNIX` or `AF_INET`.
            assert_eq!(
                execute_cbpf(
                    &open,
                    SeccompInput {
                        nr: nr_socket,
                        arch: audit,
                        arg0: (1u64 << 32) | u64::from(AF_UNIX),
                    },
                ),
                SECCOMP_RET_ERRNO_EPERM
            );
            assert_eq!(
                execute_cbpf(
                    &open,
                    SeccompInput {
                        nr: nr_socket,
                        arch: audit,
                        arg0: (1u64 << 32) | u64::from(AF_INET),
                    },
                ),
                SECCOMP_RET_ERRNO_EPERM
            );

            // AF_INET, AF_INET6, AF_NETLINK are blocked under None and allowed under Open.
            for domain in [AF_INET, AF_INET6, AF_NETLINK] {
                assert_eq!(
                    execute_cbpf(
                        &none,
                        SeccompInput {
                            nr: nr_socket,
                            arch: audit,
                            arg0: u64::from(domain),
                        },
                    ),
                    SECCOMP_RET_ERRNO_EPERM
                );
                assert_eq!(
                    execute_cbpf(
                        &open,
                        SeccompInput {
                            nr: nr_socket,
                            arch: audit,
                            arg0: u64::from(domain),
                        },
                    ),
                    SECCOMP_RET_ALLOW
                );
            }

            // socketpair(AF_UNIX) and ordinary syscalls stay allowed in both modes.
            for nr in [nr_socketpair, nr_read] {
                assert_eq!(
                    execute_cbpf(
                        &none,
                        SeccompInput {
                            nr,
                            arch: audit,
                            arg0: u64::from(AF_UNIX),
                        },
                    ),
                    SECCOMP_RET_ALLOW
                );
                assert_eq!(
                    execute_cbpf(
                        &open,
                        SeccompInput {
                            nr,
                            arch: audit,
                            arg0: u64::from(AF_UNIX),
                        },
                    ),
                    SECCOMP_RET_ALLOW
                );
            }
        }
    }
}
