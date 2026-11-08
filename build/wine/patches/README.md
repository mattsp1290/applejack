# Wine patches

`build/wine/build.sh` applies every `*.patch` in this directory with `patch -p1`
to the pinned Wine source after the msync patch set, in file-name order, once per
extracted tree. Patches may depend on earlier ones: the script applies the whole
set in order to a clone of the tree first and touches the real tree only when
every patch applied there, so a set that fails leaves the tree unpatched.
`--preflight` runs that trial on the tree without the msync patch applied.

`build/work/wine/.patches-applied` records the digest of the set a tree carries.
Changing a patch takes `sh build/wine/fetch.sh` before the next `build.sh`, which
otherwise stops, since a patch goes on pristine source. `fetch.sh` also refreshes
the mtimes of the files it resets, so a dropped hunk does not stay compiled in
`build/work/wine/build`. A change to this directory changes what the bundle ships,
so it also bumps `version` in `build/bundle-spec.toml`. A new patch or date also
updates the `WINE_PATCHES_*` constants in `xtask/src/bundle_toml.rs`, which render
the notices.

Each patch starts with a header block:

```
Origin: <upstream URL, merge request, or "applejack"; the repository and author of any code it adapts>
Date: <date of the last change>
Reason: <why the payload needs it>
Drop-when: <the condition under which the patch is removed>
```

Patches are committed; archives and build output are not. The msync patches
are not stored here; they come from the `wine-msync` pin and gate G4b records
whether one applied.

| Patch | Touches | Source |
| --- | --- | --- |
| `0001-dxmt-macdrv-abi.patch` | `dlls/winemac.drv/{cocoa_window.m,macdrv.h,macdrv_cocoa.h,window.c}` | Adapted to Wine 11.0 from <https://github.com/cyyever/wine/commit/c5375ee66cb355bb949d60e952da3bf9323d7bc3> by cyy, the Wine 11.9 reference in <https://github.com/3Shain/dxmt/issues/170>. LGPL-2.1-or-later, as Wine. |
| `0002-nsiproxy-macos-route-table.patch` | `dlls/nsiproxy.sys/{ip.c,ndis.c}` | applejack (U22a, 2026-11-06). LGPL-2.1-or-later, as Wine. |
| `0003-ntdll-directory-object-boolean-args.patch` | `dlls/ntdll/unix/sync.c` | applejack (U22b, 2026-11-08). LGPL-2.1-or-later, as Wine. |
| `0004-iphlpapi-best-route-port.patch` | `dlls/iphlpapi/iphlpapi_main.c` | applejack (U22b, 2026-11-08). LGPL-2.1-or-later, as Wine. |

`0001-dxmt-macdrv-abi.patch` gives DXMT 0.80 what it needs from Wine 11.0: an
exported `macdrv_functions` table and a `get_win_data` that returns DXMT's layout
with a view in it. It differs from the reference in four ways. It has no
`RTLD_GLOBAL` hunk: `dlopen` already defaults to global on macOS, so DXMT's
`dlsym(RTLD_DEFAULT)` needs only the export. The view belongs to a client surface
instead of being created bare, so win32u resizes it and disposes of it with the
window; a bare view would keep its first frame. The struct is per window under the
win data lock instead of one shared static, so two windows cannot overwrite each
other's. And the table's Metal view slots count their users, because every swap
chain on a window shares one view and DXMT holds its layer unretained. See "DXMT
under Wine 11.0" in `docs/engines/rosetta-wine.md` for the limits.

`0002-nsiproxy-macos-route-table.patch` makes the IPv4 route table and the
adapter list that `nsiproxy.sys` builds on macOS match what the same file
builds on Linux, which is what Windows programs expect: interface-scoped
routes (`RTF_IFSCOPE`, a VPN tunnel's own default route) and the ARP cache
(`RTF_WASCLONED`, `RTF_LLINFO`) are not routes, host routes are /32, only
gateway routes carry a next hop, routes into 127/8 are marked loopback (the
Linux code also marks its direct /32 routes, this patch does not), point-to-point
tunnels are `MIB_IF_TYPE_PPP`, and a netmask is read by its address's family
(the kernel leaves the netmask's own family unset). Without it, with Tailscale
up, `GetIpForwardTable` held three default routes, `GetAdaptersAddresses` gave
the tunnels and the loopback adapter gateways equal to their own addresses and
`en0` a hundred of them, and one tunnel address had prefix length 0. The
evidence is in the patch header; `crates/applejack-engine/probes/netprobe/`
prints what a Windows program sees and is how to re-measure it. The Steam
client's CM connect loop seen alongside it is 0003's, not this one's.

`0003-ntdll-directory-object-boolean-args.patch` makes the unix side of
`NtQueryDirectoryObject` read its BOOLEAN arguments as one byte. clang
assumes a BOOLEAN argument arrives zero-extended, so the code tested 32 bits
of `restart`, while a PE caller such as `GetLogicalDrives` sets only the low
byte; a stale byte above it restarted the enumeration on every call, so
`GetLogicalDrives` never returned. The Steam client's machine-ID thread calls
it through WMI's `Win32_DiskPartition`, and the client's CM connect waits for
that thread: without the patch the client logs `EConnect called - scheduling
connection for 50ms from now` ten times a second and never connects. Other
syscalls share the cause; see "Sub-int arguments to unix syscalls" below.

`0004-iphlpapi-best-route-port.patch` gives `GetBestRoute2`'s routing probe a
port. It asks the kernel for the route by connecting a UDP socket to the
destination, which usually carries port 0, and macOS refuses `connect()` to
port 0, so the kernel's answer was never used: a `GetBestInterface`,
`GetBestInterfaceEx` or `GetBestRoute2` call that gave no source address and
port 0 fell back to scanning the route table (`GetBestInterface` always
passes port 0).

Wine is LGPL: this directory is the source of the changes the bundle's `wine/`
carries. `THIRD_PARTY_NOTICES.md` in the bundle links it at the commit that last
changed it, which `build/assemble.sh` reads from Git, so a bundle meant to leave
this repository is assembled from a commit on `main`.

## Sub-int arguments to unix syscalls

0003 fixes one case of a class. Wine's x86_64 syscall dispatcher copies each
PE argument slot into the SysV argument registers as a whole 64-bit value.
Under the Windows ABI the bits above a BOOLEAN (or any argument narrower than
32 bits) are undefined, and a PE caller often writes only the low byte; clang,
compiling the unix side for the SysV ABI, assumes the caller zero-extended it
and may test 32 bits. Whether a call then misbehaves depends on what the
caller left in the upper bits, which is stack and register contents, so it
comes and goes.

In the stock Wine 11.0 `x86_64-unix/ntdll.so` (measured with `objdump`) these
syscalls use a register-passed BOOLEAN as 32 bits and are not patched. The
first five test it locally:

| Syscall | Argument | Read |
| --- | --- | --- |
| `NtDelayExecution` | `alertable` | `testl %edi,%edi` |
| `NtContinue` | `alertable` | `testl %esi,%esi` |
| `NtDuplicateToken` | `effective_only` | `testl %ecx,%ecx` |
| `NtQueryDefaultLocale`, `NtSetDefaultLocale` | `user` | `testl %edi,%edi` |

`NtAdjustPrivilegesToken` is exposed through the server request instead. It
copies `disable` whole (`movl %esi,%r13d` at 0x299d1) and stores it as 32
bits into `req->disable_all` (`movl %r13d,-0xc0(%rbp)` at 0x29a53), an `int`
in `server/protocol.def` that wineserver tests whole (`server/token.c` line
1284 in 11.0: `if (req->disable_all)`). Its own `if (!disable)` is a byte
test (`testb %r13b,%r13b` at 0x29a68), so a FALSE with dirty upper bits
still sends the privilege list (the test sees zero and falls through to add it
at 0x29a6d), but wineserver sees `disable_all` nonzero, ignores the list and
disables every privilege of the token (and returns no previous state).

None has been seen to fail. kernelbase's `SleepEx`, the common route into
`NtDelayExecution`, passes a whole 32-bit `BOOL`; a caller that set only the
low byte with dirty bits above would turn a non-alertable sleep into an
alertable wait. They stay unpatched until a failure is observed.

Not exposed in the same build: BOOLEANs passed on the stack (the seventh
argument onward, such as `NtQueryDirectoryFile`'s `single_entry` and
`restart_scan`) are loaded with `movzbl`, and several register BOOLEANs are
read only through their low byte (`NtWaitForSingleObject`,
`NtWaitForMultipleObjects`, `NtSignalAndWaitForSingleObject`, `NtSetTimer`,
`NtCreateEvent`, `NtCreateMutant`, `NtCommitTransaction`,
`NtRollbackTransaction`, `NtSetTimerResolution`, the keyed events).
`NtWaitForMultipleObjects` spills `alertable` as 32 bits (`movl
%ecx,-0x2c(%rbp)`) but afterwards reads it only as a byte: `movzbl
%r13b,%edx` at 0x47c2d into `inproc_wait`, `cmpb $0x1,%r13b` at 0x47c47,
and `cmpb $0x0,-0x30(%rbp)` at 0x47dcf in `inproc_wait`.

To check a new Wine build, disassemble each unix syscall that takes a BOOLEAN
and follow every use of the argument's register (SysV order `rdi`,
`rsi`, `rdx`, `rcx`, `r8`, `r9`; seventh and later on the stack):

```
objdump -d --no-show-raw-insn --disassemble-symbols=_NtDelayExecution \
  build/work/wine/stage/lib/wine/x86_64-unix/ntdll.so
```

A `testl`, `cmpl` or 32-bit `cmov` on the register (`%edi`, `%r8d`) means
the upper bits count: exposed. A `movzbl` from the 8-bit register (`%dil`,
`%r8b`), a `testb` or a `cmpb` reads the byte alone. A 32-bit `movl` copy or
spill decides nothing yet: follow every use of the copy, not only its first
test, including into the callee it is passed to. A 32-bit copy into a server
request field (a `movl` into the request on the stack; in the C source,
`req->field = arg`) is exposed when wineserver reads the field wider than a
byte, even if the local code later tests only the byte. To check, find the
field's type in `server/protocol.def` (or the generated
`include/wine/server_protocol.h`) and its uses in `server/*.c`; an `int`
field tested as `if (req->field)` is exposed. `NtAdjustPrivilegesToken`
above is that case: its first local test is a `testb`, but the earlier
`movl %r13d,-0xc0(%rbp)` fills `disable_all`, an `int` that
`server/token.c` tests whole.
A fix per function reloads the argument through `volatile BOOLEAN`, as 0003
does; the general fix is for the dispatcher to zero-extend sub-int arguments,
and is upstream's to make.
