//! Prints the CPUID feature bits Rosetta advertises, as one JSON line:
//! `{"avx":bool,"avx2":bool,"osxsave":bool,"xcr0_ymm":bool}`.
//! The launcher runs it with and without `ROSETTA_ADVERTISE_AVX=1`.

use std::process::ExitCode;

#[allow(
    clippy::struct_excessive_bools,
    reason = "each field is one CPUID bit reported verbatim in the JSON line"
)]
struct Features {
    avx: bool,
    avx2: bool,
    osxsave: bool,
    xcr0_ymm: bool,
}

#[cfg(target_arch = "x86_64")]
#[allow(
    unsafe_code,
    reason = "CPUID and XGETBV are always available on x86-64; XGETBV is read only when OSXSAVE says the OS enabled it"
)]
fn features() -> Features {
    use std::arch::x86_64::{__cpuid, __cpuid_count, _xgetbv};

    // SAFETY: CPUID leaf 1 and leaf 7 exist on every x86-64 CPU.
    let leaf1 = unsafe { __cpuid(1) };
    let leaf7 = unsafe { __cpuid_count(7, 0) };
    let osxsave = leaf1.ecx & (1 << 27) != 0;
    let avx = leaf1.ecx & (1 << 28) != 0;
    let avx2 = leaf7.ebx & (1 << 5) != 0;
    // SAFETY: XGETBV faults unless OSXSAVE is set, so it is gated on that bit.
    let xcr0 = if osxsave { unsafe { _xgetbv(0) } } else { 0 };
    Features {
        avx,
        avx2,
        osxsave,
        xcr0_ymm: xcr0 & 0x6 == 0x6,
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn features() -> Features {
    Features {
        avx: false,
        avx2: false,
        osxsave: false,
        xcr0_ymm: false,
    }
}

fn main() -> ExitCode {
    if !cfg!(target_arch = "x86_64") {
        eprintln!("cpuid: built for a non-x86-64 target; results are meaningless");
        return ExitCode::FAILURE;
    }
    let f = features();
    println!(
        "{{\"avx\":{},\"avx2\":{},\"osxsave\":{},\"xcr0_ymm\":{}}}",
        f.avx, f.avx2, f.osxsave, f.xcr0_ymm
    );
    ExitCode::SUCCESS
}
