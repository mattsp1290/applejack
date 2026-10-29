# macOS build gaps: what an open-source "Winlator/GameHub-style" Steam runner for Apple Silicon would need (as of 2026-10-27)

Scope note: this file covers engineering gaps, licensing and platform constraints, and prior art on macOS. General state-of-the-art is covered by another researcher. Everything dated before 2025 is marked as such. "Confirmed" = read on a primary page; "secondary" = a news/aggregator summary of a primary page I could not open; "inferred" = my reasoning from confirmed facts.

Headline shifts since mid-2026 that change the whole analysis (all confirmed from Apple or WineHQ primary sources):

1. Apple shipped, in macOS 26.4-26.5, the kernel/ABI features Wine-on-ARM64 needed for years: a per-process **4K page size** (`posix_spawnattr_set_4k_page_size_np()`), an **x18 custom ABI** mode (`os_set_custom_x18_abi_enabled`), **per-thread TSO** (`thread_set_x86_64_compat()`), a linker flag to reserve the x86-64 layout (`-Wl,-x86_64_layout_emulation`, Xcode 26.4), and a **restricted entitlement** (`com.apple.developer.cross-architecture-support`, paid teams; `-unmanaged` variant intended for free accounts but currently not provisionable) that unlocks mapping below 4 GB. Source: Brendan Shanks (CodeWeavers), wine-devel, 2026-08-07 — [Wine on ARM64 macOS](https://list.winehq.org/hyperkitty/list/wine-devel@list.winehq.org/thread/CKG5CEN2BE5VRXZ7O7NX4YUSBH3247WH/); Apple DTS in [thread 841302](https://developer.apple.com/forums/thread/841302) and [thread 845976](https://developer.apple.com/forums/thread/845976).
2. CodeWeavers shipped (2026-07-31) a CrossOver Preview that runs **native ARM64 Wine + a CodeWeavers-ported FEX** on macOS 26.5+, with ARM64 DXMT, no D3DMetal, no bottle migration; the preview targets CrossOver 27/28 — [CodeWeavers blog URL](https://www.codeweavers.com/blog/mjohnson/2026/7/31/crossover-preview-the-right-to-bear-arm64-on-mac) (site blocked my fetcher; details via [iThinkDifferent](https://www.ithinkdiff.com/?p=343215), [MacMagazine](https://macmagazine.com.br/post/2026/07/31/crossover-27-ganha-versao-de-teste-com-suporte-a-chips-apple-silicon/), and an [Owler mirror](https://www.owler.com/reports/codeweavers/codeweavers-blog-meredith-johnson--crossover-previ/1785614130392)).
3. Apple's Rosetta end-of-life wording is now final in the docs: available "through macOS 27" as a general tool; afterwards only "a subset of Rosetta functionality aimed at supporting older unmaintained gaming titles, that rely on Intel-based frameworks" — [Apple: About the Rosetta translation environment](https://developer.apple.com/documentation/apple-silicon/about-the-rosetta-translation-environment). macOS 27 release notes: "All Intel-based software will no longer be compatible with macOS 28.0, excluding legacy games." — [macOS 27 release notes](https://developer.apple.com/documentation/macos-release-notes/macos-27-release-notes).
4. A non-Rosetta VM route already exists end-to-end on macOS: **steamac** runs Valve's ARM64 SteamOS in a libkrun VM with Proton 11 (ARM64EC) + FEX, Venus → virglrenderer → MoltenVK/KosmicKrisp → Metal, optional DXMT, with 4K-page guest kernel and Apple TSO — [fxgl/steamac](https://github.com/fxgl/steamac/wiki).

---

## Q1. CPU translation component: Rosetta 2 vs a Darwin FEX/Box64 port — what is blocking, and what Apple changed

### Takeaway
A Darwin host port of FEX is no longer hypothetical: CodeWeavers has one in a shipping preview, and Apple's 26.4/26.5 APIs (4K-page processes, x18 custom ABI, per-thread TSO, sub-4GB mapping via entitlement) remove the hard kernel blockers. But upstream FEX has said "no plans for macOS," Box64 has no Darwin port, and the Apple entitlement currently requires a paid developer team — so an open-source project must either (a) obtain CodeWeavers' Wine/FEX changes as they are upstreamed, or (b) do its own port, and in both cases must ship under a paid Developer ID with a provisioning profile.

### Cited Findings

**Rosetta 2 facts (confirmed, Apple primary)**
- "Rosetta ... will be available through macOS 27 — as a general-purpose tool for Intel apps ... Beyond this timeframe, we will keep a subset of Rosetta functionality aimed at supporting older unmaintained gaming titles, that rely on Intel-based frameworks." — [Apple docs](https://developer.apple.com/documentation/apple-silicon/about-the-rosetta-translation-environment)
- "The system prevents you from mixing arm64 code and x86_64 code in the same process. Rosetta translation applies to an entire process." Rosetta "doesn't translate ... Kernel extensions; Virtual Machine apps that virtualize x86_64 computer platforms." It "translates all x86_64 instructions, including ... AVX and AVX2 ... but it doesn't support the execution of AVX512." It "can translate most Intel-based apps, including apps that contain just-in-time (JIT) compilers." — [Apple docs](https://developer.apple.com/documentation/apple-silicon/about-the-rosetta-translation-environment)
- macOS 27 release notes, Gaming: "A new command line tool lets you enable support for legacy Intel-based games during beta releases ... `sudo game-test-tool enable` ... enabling legacy game support disables Rosetta, non-game processes might crash or behave unexpectedly, and this feature is intended only for playing legacy Intel-based games and is not available outside of macOS beta releases." Rosetta section: "Applications previously set to 'Open using Rosetta' by a user will have the application launch natively"; "If Rosetta was previously installed, it is not automatically restored after upgrading to macOS 27.0"; "All Intel-based software will no longer be compatible with macOS 28.0, excluding legacy games." — [macOS 27 Golden Gate Release Notes](https://developer.apple.com/documentation/macos-release-notes/macos-27-release-notes)
- An Apple Graphics and Games engineer (Jun 2026) answering whether Wine/CrossOver's Rosetta needs will be kept: "x86_64 macOS games should continue to work as they do on macOS 26. You can preview this behavior in macOS 27 by using game-test-tool." No definition of which components stay; Wine/CrossOver not addressed. — [Apple forum 830692](https://developer.apple.com/forums/thread/830692)
- Translated (Rosetta) x86_64 processes see `vm_page_size` = 4096 while native arm64 processes see 16384; the host still uses 16K pages underneath — [psutil #1885](https://github.com/giampaolo/psutil/issues/1885); Chromium made page size a runtime property "which is what allows x86_64 Chrome to run under Rosetta" — [crrev 808610](https://crrev.com/808610)

**Why Wine's x86-64 build has been "Rosetta-whole-process" on Apple Silicon (confirmed)**
- Wine 11.0 on Macs "is an x86-64 program, but it runs on Apple Silicon Macs using Rosetta 2"; upstream has no native macOS build — [The Register, 2026-01-15](https://www.theregister.com/2026/01/15/wine_11_arrives_faster_and/) (secondary) ; NixOS discussion: WoW64 build "still runs as an x86_64 build under Rosetta 2" — [NixOS Discourse](https://discourse.nixos.org/t/x86-64-darwin-removal-and-wine/77532)
- Wine bug 58567 (2025, Wine 10.12): native aarch64 macOS Wine "doesn't work"; CodeWeavers' Brendan Shanks: "ARM64 Wine builds on macOS but currently does not work." — [wine-bugs 58567](https://list.winehq.org/hyperkitty/list/wine-bugs@list.winehq.org/thread/AIEGB45LZWRUP7UAVUWQPPRXFVZU5I7L/)
- Historical (2020, Martin Storsjö's first macOS ARM64 bring-up): blockers listed were larger page size, nothing mapped in the lower 4 GB, and no simultaneously writable+executable mappings — [Phoronix, 2020-08-14](https://www.phoronix.com/news/Wine-ARM64-macOS-Initial-Patch)

**The specific blockers and Apple's 2026 answers (confirmed)**
- Blockers as stated by a Wine-for-WOA porter (Aug 2026): "Windows PE files place .text and .data in the same page, which macOS's JIT/W^X model cannot handle. pthread_jit_write_protect_np() cannot be used for foreign ARM64 code. Apple Silicon reserves x18, breaking the Windows ARM64 ABI. Wine also must reserve 0x7FFE0000 for the Windows TEB, but macOS cannot guarantee this address." Apple DTS (Quinn): "macOS 26.4 improved this situation. Check out `os_set_custom_x18_abi_enabled` ... in `<os/arch/arm64.h>`"; on JIT/W^X and TEB Quinn only asked for details. — [Apple forum 841302](https://developer.apple.com/forums/thread/841302)
- Sub-4GB mapping: on arm64, `task_info(TASK_VM_INFO)` min_address is 0x100ea0000; `mmap(0x7ffe0000, MAP_FIXED)` → ENOMEM; `mach_vm_allocate(VM_FLAGS_FIXED)` → KERN_INVALID_ADDRESS; smaller `__PAGEZERO` values are SIGKILLed before main. Apple DTS's tested recipe (Xcode 27.0, macOS 26.6.1): add the "Cross-architecture Compatibility Framework" capability (entitlement `com.apple.developer.cross-architecture-support`, needs a provisioning profile) and link with `-pagezero_size 0x4000`, or keep the 4 GB PAGEZERO and `vm_deallocate` it at startup; with entitlement + small pagezero the allocation at 0x7ffe0000 succeeds. Each helper needs its own App ID and must be an app bundle; a bare helper executable is killed. Entitlement is "only available to paid teams"; the `-unmanaged` variant works at the system level but cannot be provisioned for a free Personal Team (FB25187024). Public docs pending (r. 188189100). — [Apple forum 845976](https://developer.apple.com/forums/thread/845976); follow-up [Apple forum 852591](https://developer.apple.com/forums/thread/852591) (FB25285950 marked dup of FB25187024)
- wine-devel "Wine on ARM64 macOS" (Brendan Shanks, 2026-08-07): macOS 26.5 adds `posix_spawnattr_set_4k_page_size_np()` for 4K-page processes, an x18 custom ABI mode threads can enter/leave, and `thread_set_x86_64_compat()` to enable TSO per thread (new threads don't inherit it); Xcode 26.4 adds `-Wl,-x86_64_layout_emulation` to enlarge PAGEZERO to ~0x170000000 so USER_SHARED_DATA at 0x7ffe0000 and the sub-4GB space for i386/x86_64 emulation can be mapped; linking with `-Wl,-platform_version,macos,11.0,11.0` keeps x18 from being cleared as an interim; restricted entitlements are required to use "x18 custom ABI, 4K pages, PAGEZERO changes, and TSO"; "The standard entitlement requires a paid Apple developer account. The `-unmanaged` variant is for free accounts"; SIP-disable is a (not recommended) bypass; "CrossOver Preview runs ARM64 Wine with FEX for i386/x86_64 (ARM64EC) emulation"; Shanks "will send Wine merge requests 'soon'." — [wine-devel thread](https://list.winehq.org/hyperkitty/list/wine-devel@list.winehq.org/thread/CKG5CEN2BE5VRXZ7O7NX4YUSBH3247WH/)

**How FEX plugs into Wine (confirmed) — this is the shape a Darwin port takes**
- Wine 10.0: "The ARM64EC architecture is fully supported ... The 64-bit x86 emulation interface is implemented ... all of the Wine code as native, with only the application's x86-64 code requiring emulation. No emulation library is provided with Wine ... an external library that exports the emulation interface can be used, by specifying its name in the `HKLM\Software\Microsoft\Wow64\amd64` registry key. The FEX emulator implements this interface when built as ARM64EC." Also: "ARM64 support requires the system page size to be 4K ... Running on kernels with 16K or 64K pages is not supported at this point." — [Wine 10.0 ANNOUNCE](https://raw.githubusercontent.com/wine-mirror/wine/wine-10.0/ANNOUNCE.md)
- Wine 11.0: "On ARM64, there is support for simulating a 4K page size on top of larger host pages (typically 16K or 64K)"; "On macOS, the `%gs` register is swapped in the syscall dispatcher. This avoids conflicts between the Windows TEB and the macOS thread descriptor"; "The new WoW64 mode ... is considered fully supported"; NTSYNC used "when available" (Linux). — [Wine 11.0 ANNOUNCE](https://raw.githubusercontent.com/wine-mirror/wine/wine-11.0/ANNOUNCE.md)
- FEX's Windows modules "purely handle CPU emulation. Syscall translation is done on either the wine side (WOW64) or ABI (ARM64EC)"; FEX is built twice (`libwow64fex.dll` for 32-bit, `libarm64ecfex.dll` for 64-bit); FEX "loads early and can depend only on ntdll"; built with llvm-mingw; "Upstream wine has various things missing for full FEX/ARM64EC support" (author's fork recommended). — [FEX wiki ARM64EC](https://wiki.fex-emu.com/index.php/Development:ARM64EC); Hangover ships `libarm64ecfex.dll` as default x86_64 emulator — [AndreRH/hangover](https://github.com/AndreRH/hangover)
- Upstream FEX stance: issue #5046 "Mac Support" (2025-11-13), maintainer Sonicadvance1: "There are no plans for MacOS support in FEX at this time." Closed as not planned, locked 2025-12-10. — [FEX #5046](https://github.com/FEX-Emu/FEX/issues/5046). FEX issue #1221 (2022) "Throw an error when compiling and running on a non-4k page host" — [FEX issues search](https://github.com/FEX-Emu/FEX/issues?q=macOS+OR+Darwin)
- Fedora: "FEX only supports AArch64 host systems running a 4k page-size kernel"; Asahi runs it inside a 4K microVM (muvm/krun) — [Fedora Change: FEX](https://www.fedoraproject.org/wiki/Changes/FEX); [Fedora Asahi x86 docs](https://docs.fedoraproject.org/en-US/fedora-asahi-remix/x86-support/)
- FEX issue #2841 (2023, still open): "Denuvo needs inline self-modifying code support" — [FEX issues search](https://github.com/FEX-Emu/FEX/issues?q=macOS+OR+Darwin)

**CodeWeavers' Darwin FEX (confirmed via secondary; primary blocked)**
- CodeWeavers announced 2026-07-31 that "today's CrossOver Preview build includes native ARM64 support for macOS"; "July 2026: CrossOver team completes work to make a custom version of FEX compatible with macOS, and ships the first Mac ARM64 builds"; "The ARM64 pieces require macOS 26.5 or higher. If you are running an older OS, you will be running Intel Wine and Rosetta, not ARM64 Wine and FEX"; "The ARM64 builds also include ARM64 DXMT! If you are testing games, we recommend using the DXMT graphics option"; not expected to be useful beyond testing — [Owler mirror of CodeWeavers post](https://www.owler.com/reports/codeweavers/codeweavers-blog-meredith-johnson--crossover-previ/1785614130392) (secondary)
- Limitations reported: no D3DMetal, existing bottles cannot migrate to ARM, some launchers incompatible, CrossOver 27 drops Intel Macs — [MacMagazine](https://macmagazine.com.br/post/2026/07/31/crossover-27-ganha-versao-de-teste-com-suporte-a-chips-apple-silicon/) (secondary). Target named as CrossOver 28 by [iThinkDifferent](https://www.ithinkdiff.com/?p=343215) and CrossOver 27 by MacMagazine/Owler — conflicting.
- A forum user (galad87, Sep 2026): "The latest CrossOver beta can run 32-bit Windows programs using FEX" — [Apple forum 845976](https://developer.apple.com/forums/thread/845976)
- CodeWeavers PortJump (2026-06-19) says its "Intel emulation technology for ARM64 is also available on Linux (currently in beta)" and that apps not upgrading "will not be compatible with macOS 28" — [CodeWeavers PortJump post](https://www.codeweavers.com/blog/orudge/2026/6/19/portjump-update-upcoming-changes-to-macos-support-for-intel-based-applications) (search-snippet only; page blocked)

**Box64 (confirmed)**
- Box64 is "Linux Userspace x86_64 Emulator"; no Darwin support found anywhere — [ptitSeb/box64](https://github.com/ptitSeb/box64)
- 2026-08-07 preview (v0.4.6): Box64 "makes programs believe they are running on a 4K system by emulating 4K behavior in its memory functions" ("fake 4K"); for Windows programs it "adjusts memory when Wine or Proton loads a program or DLL"; Steam, Wine/Proton, and some Denuvo games work on 16K Asahi; "Not everything works on 16K." macOS/Darwin not mentioned. — [box86.org](https://box86.org/2026/08/preview-of-next-box64-improved-non-4k-pagesize-handling/)

**Hosting FEX in a Linux VM instead (confirmed)**
- Podman FEX machine (2026-04-20, tnk4on, personal project): custom Podman Machine OS replaces Rosetta/QEMU with FEX; requires `libkrun` provider; 13/17 known x86_64 container issues resolved; simple run 2.1 s native vs 2.8 s FEX cold / 2.4 s warm on M1 Max — [podman discussion 28549](https://github.com/containers/podman/discussions/28549)

### Inferences
- For a Mac-native stack the "FEX port" needed is **not** the Linux-syscall-emulating `FEXInterpreter`; it is the ARM64EC/WOW64 emulator DLL (`libarm64ecfex.dll`/`libwow64fex.dll`) that depends only on ntdll, plus the host-side bits FEX needs from Darwin (JIT memory allocation, signal/exception delivery, TSO toggling, 4K page semantics). Because Wine does the syscalls, "Linux syscall emulation on Darwin" and "Box64 wrapped native libs" are non-issues on this path; the issues are Mach exception → SEH translation inside Wine's macOS `ntdll`, W^X handling for JIT code cache (MAP_JIT + `pthread_jit_write_protect_np` are for native JITs; emitted FEX code is native ARM64 so this works, but **guest** x86 pages with mixed text/data are what the 4K-page mode and `-x86_64_layout_emulation` address), and x18/TEB.
- 16K pages: for a Rosetta path the OS fakes 4K already; for a native path Apple now provides real 4K-page processes (26.5) and Wine 11 has "simulate 4K on 16K" fallback. CrossOver's 26.5 floor is explained by exactly these APIs. So "FEX's 16K support for Asahi" (which does not exist — Asahi uses a 4K VM) is irrelevant; the macOS answer is the Apple API.
- Licensing: FEX is MIT, so CodeWeavers has no obligation to publish its Darwin FEX changes; Wine is LGPL, so the Wine-side ARM64-macOS changes must ship with source and Shanks has said MRs are coming. An open-source project should plan on upstream Wine gaining the loader/entitlement/TSO plumbing in late 2026–2027, and on needing its own (or a community) Darwin FEX if CodeWeavers does not publish theirs.
- The entitlement requirement is the single biggest *process* constraint: a notarized, entitled loader needs a paid Apple Developer Program membership, a provisioning profile embedded in each helper `.app`, and a unique App ID per helper. Unsigned/ad-hoc community builds would have to tell users to disable SIP (unacceptable for a one-click product).

### Gaps
- Could not read the CodeWeavers 2026-07-31 blog post directly (codeweavers.com returns 403/JS-challenge to fetchers; Wayback blocked); details rest on three secondary summaries. Whether CodeWeavers' FEX fork is public is unknown.
- No primary data on CrossOver-ARM64 performance vs Rosetta (iThinkDifferent explicitly says it is an "open question").
- Apple's public documentation for `com.apple.developer.cross-architecture-support` was still unpublished as of the forum threads (Sep–Oct 2026).
- No source on how Darwin FEX handles Mach exceptions vs Linux signals; the FEX wiki page does not discuss non-Linux hosts.

---

## Q2. Wine on macOS specifics: winemac.drv, upstream ARM64 Mach-O Wine, CrossOver differences, address space, WoW64 for 32-bit, msync/ntsync

### Takeaway
Upstream Wine on macOS is still an x86-64 Mach-O build run under Rosetta with "new WoW64" handling 32-bit games; native ARM64 Mach-O Wine compiles but did not run as of Wine 10.12 and only works in CodeWeavers' preview using macOS 26.5 APIs that upstream patches have not yet landed. Sync is msync (Mach semaphores/ulock) in CrossOver; ntsync is Linux-only.

### Cited Findings
- Wine 10.0 macOS: "When building with Xcode >= 15.3 on macOS, the preloader is no longer needed"; "Syscall emulation for applications doing direct NT syscalls is supported on macOS Sonoma and later." — [Wine 10.0 ANNOUNCE](https://raw.githubusercontent.com/wine-mirror/wine/wine-10.0/ANNOUNCE.md)
- Wine 11.0: new WoW64 "fully supported"; 16-bit apps supported in new WoW64; `WINEARCH=win32` prefixes deprecated; "Thread priority changes are implemented on Linux and macOS"; macOS `%gs` swap in syscall dispatcher to avoid TEB vs macOS thread-descriptor conflict; "The Vulkan API version 1.4.335 is supported." — [Wine 11.0 ANNOUNCE](https://raw.githubusercontent.com/wine-mirror/wine/wine-11.0/ANNOUNCE.md)
- Wine 11.x dev releases continued ARM64EC work (11.13 FFmpeg on ARM64EC; 11.16 Wine Mono ARM64; 11.20 ARM64EC relay debugging, Oct 2026) — [Phoronix Wine 11.20](https://phoronix.com/news/Wine-11.20-Released), [Phoronix Wine 11.16](https://phoronix.com/news/Wine-11.16-Released) (secondary)
- Native aarch64 Wine on macOS: bug 58567 (Wine 10.12) "doesn't work"; Shanks: "ARM64 Wine builds on macOS but currently does not work" — [wine-bugs 58567](https://list.winehq.org/hyperkitty/list/wine-bugs@list.winehq.org/thread/AIEGB45LZWRUP7UAVUWQPPRXFVZU5I7L/)
- A Wine patch series addressed Apple Silicon x18/TEB handling in syscall dispatchers — [wine-gitlab list](https://list.winehq.org/hyperkitty/list/wine-gitlab@list.winehq.org/thread/KFIKY4FPELQQMU45N65AJ4IETWPSZIN7/) (title only)
- Upstream vs CrossOver: a Wine WoW64 build failed to launch an app on Apple Silicon while CrossOver "which carries extra patches" ran it — [wine-bugs 55307](https://list.winehq.org/hyperkitty/list/wine-bugs@list.winehq.org/thread/B52HO3NKKGK3265RB3ZMIFTWVWMXW5ET/)
- 32-bit history (older): CrossOver 19 (Dec 2019) introduced "wine32on64" needing a modified LLVM; CrossOver 24 (2024) "can once again run 32-bit software on a 64-bit system" thanks to Wine 9 WoW64 — [CodeWeavers 2019 blog](https://www.codeweavers.com/blog/jwhite/2019/12/10/celebrating-the-difficult-the-release-of-crossover-19), [9to5Mac 2024](https://9to5mac.com/2024/02/22/crossover-24-macos-games/) (secondary)
- Gcenx community install guide: macOS builds are not uploaded to WineHQ, they come from Gcenx's releases/Homebrew tap (`wine-crossover` cask); "Upstream DXVK is said not to work on macOS without modifications"; Gatekeeper warns about each Windows binary; suggests `--no-quarantine`/`spctl` (i.e., community builds are not notarized) — [Gcenx/wine-on-mac](https://github.com/Gcenx/wine-on-mac)
- msync: Wine patch set using Mach semaphores plus `ulock` to emulate NT sync; "no file-descriptor limit" unlike esync (which uses pipes); benchmarks show msync faster than esync — [marzent/wine-msync](https://github.com/marzent/wine-msync). CrossOver exposes "Enables MSync in a bottle on Mac" — [CodeWeavers enablemsync](https://www.codeweavers.com/compatibility/crossover/enablemsync)
- ntsync is a Linux kernel driver (`/dev/ntsync`, Linux 6.14+); CrossOver 26 (Wine 11.0) added ntsync on Linux — [linux.org/OMG Ubuntu](https://www.linux.org/threads/omg-ubuntu-crossover-26-released-with-wine-11-0-and-ntsync-support.62405/) (secondary)
- CrossOver 26 ships Wine 11.0 and "new versions of D3DMetal, DXMT, vkd3d, and Wine Mono"; D3DMetal 3.0 — [Linux Adictos](https://en.linuxadictos.com/Crossover-26-raises-the-bar-for-Windows-games-on-Mac-and-Linux.html) (secondary)
- CrossOver Mac 26 "Auto" graphics: "uses the graphics backend specified in its proprietary database on a per-game basis, and uses wined3d if no backend is specified" — [CodeWeavers Advanced Settings (Mac 26)](https://codeweavers.helpjuice.com/en_US/advanced-settings-in-crossover-mac-26)

### Inferences
- An open-source launcher today (Oct 2026) has two Wine choices: (1) x86-64 Wine (Gcenx WineHQ/CrossOver-source builds) under Rosetta — works on every Apple Silicon Mac through macOS 27 but is dead on macOS 28 unless Apple's "legacy games" carve-out somehow covers Wine (Apple has not said it does); (2) native ARM64 Wine + Darwin FEX — only CrossOver's proprietary-packaged preview exists, requires macOS 26.5+, and requires the paid-team entitlement.
- The "macOS ntsync" equivalent is msync; there is no kernel driver path on macOS, so msync (userland Mach semaphores + ulock) is the ceiling. Since msync is out-of-tree, the project must carry it as a patch like CrossOver does.
- winemac.drv is the only display driver on macOS (no X11/Wayland); nothing found suggesting changes relevant to ARM64 beyond the TEB/x18 handling.

### Gaps
- Could not open the WineHQ MacOS-Building wiki (Anubis bot wall) to quote current `-pagezero_size`/`-image_base` link flags; the Wine 10.0 note "preloader no longer needed" is the authoritative replacement.
- No source for whether upstream Wine's msync-equivalent will ever be merged; marzent's repo is the only reference.
- No current (2026) documentation of winemac.drv limitations (e.g., HDR, VRR) found.

---

## Q3. Graphics: D3DMetal/GPTK licensing, DXMT, DXVK on MoltenVK, vkd3d-proton, KosmicKrisp, Metal 4 — minimal viable path and ceiling

### Takeaway
The only open-source path with product-grade DX11 performance on macOS is DXMT (LGPL-2.1, D3D10/11, now with an ARM64EC build); DX12 is either D3DMetal (closed, Apple license allows distribution only for non-commercial purposes and is contested) or vkd3d-proton over MoltenVK/KosmicKrisp (works in steamac with FL 12_0 on KosmicKrisp / 11_0 on MoltenVK, SM 6.0 only). Metal 4 (Apple-silicon-only) adds placement sparse resources, residency sets, argument tables, barriers, ML tensors and MetalFX frame interpolation — features that narrow the DX12 gap but no open-source DX12-on-Metal layer uses them yet.

### Cited Findings

**D3DMetal / GPTK license**
- Apple Developer Forums (Aug 2026) "Update license of D3DMetal for redistribution": UTM says "CrossOver and Whisky both redistribute D3DMetal in their apps ... the included license does not make it clear" and filed FB24262864; "Apara, a commercial Mac app that runs Windows games on Apple silicon," says D3D12 titles "run well only through D3DMetal" and customers must download GPTK themselves; a third poster quotes the license: "distribute the Apple Software solely for non-commercial purposes" (truncated). No Apple response. — [Apple forum 841547](https://developer.apple.com/forums/thread/841547)
- License fragment reproduced on Lemmy (2023, older): "you are granted a limited, non-exclusive, non-transferable, personal copyright license to (i) install, internally use, and test the Apple Software for the sole purpose of developing, testing ..." ending with distribution "solely for non-commercial purposes" — [Lemmy comment](https://old.lemmy.dbzer0.com/comment/1632820) (secondary; exact wording unverified)
- "You cannot ship a game with D3DMetal. They're not open source and the licensing terms seem to forbid using D3DMetal in a shipping title." (2023) — [Colin Cornaby](https://www.colincornaby.me/2023/06/mac-games-at-wwdc-2023/); GamingOnLinux (2023): D3DMetal is "Unsuitable for shipping games with it" — [Mastodon](https://mastodon.social/@gamingonlinux/110503811705177492)
- Gcenx's GPTK community builds (latest "3.0-3", Mar [2026]; Apple Silicon, macOS 14+, 16 GB RAM recommended) carry the note "please ensure you comply with Apple's License.pdf for D3DMetal"; releases are based on `apple/homebrew-apple@2bc4428` — [Gcenx GPTK releases](https://github.com/Gcenx/game-porting-toolkit/releases)
- Apple's "What's new" games page (2026) describes GPTK 4 with agent skills, Metal CLI tools, "The evaluation environment now supports Metal 4"; it does not describe D3DMetal redistribution — [Apple games what's new](https://developer.apple.com/games/whats-new/)
- CrossOver ARM64 preview: no D3DMetal — [MacMagazine](https://macmagazine.com.br/post/2026/07/31/crossover-27-ganha-versao-de-teste-com-suporte-a-chips-apple-silicon/) (secondary)

**DXMT**
- "A Metal-based translation layer for Direct3D 11 and 10 which allows running 3D applications on macOS using Wine"; README license LGPL-2.1 (repo shows three license files); repo contains `build-arm64ec.txt`, `build-win32.txt`, `build-win64.txt`; releases list empty in captured page; dev builds via GitHub Actions; wiki last edited 2025-06-15 (pages: Installation Guide, Device System Runtime Specifications, HDR, Vendor Extensions) — [3Shain/dxmt](https://github.com/3Shain/dxmt), [wiki](https://github.com/3Shain/dxmt/wiki)
- CrossOver ARM64 preview includes "ARM64 build of DXMT" and recommends it for games — [Owler mirror](https://www.owler.com/reports/codeweavers/codeweavers-blog-meredith-johnson--crossover-previ/1785614130392) (secondary)
- steamac: DXMT optional, "experimental. No stream output"; Death's Door title screen ~41 FPS DXVK path vs ~117 DXMT; d3d11bench 100 draws 44 (DXVK) / 100 FPS (DXMT), 2000 draws 133 vs 115 — [steamac wiki](https://github.com/fxgl/steamac/wiki)
- CodeWeavers PoE tip (CrossOver 25.1.1): bundled DXMT "doesn't support shader caching yet," prefer D3DMetal for that game — [CodeWeavers PoE tip](https://www.codeweavers.com/compatibility/crossover/tips/path-of-exile/mac-performance-improvement)

**Vulkan on Metal: MoltenVK and KosmicKrisp**
- MoltenVK 1.3.0 (2025-05-02) added Vulkan 1.3; v1.4.0 (2025-08-20) added Vulkan 1.4 — [Phoronix](https://www.phoronix.com/news/MoltenVK-1.3-Released), [MoltenVK releases](https://github.com/KhronosGroup/MoltenVK/releases) (secondary/primary)
- MoltenVK `KHR_buffer_device_address` was added via a "quick patch" for iOS 16/macOS 13+ (PR #1619) — [MoltenVK PR 1619](https://github.com/KhronosGroup/MoltenVK/pull/1619)
- KosmicKrisp: Mesa Vulkan-on-Metal driver developed by LunarG (for Google's Android emulator), upstreamed in Mesa 26.0; Vulkan 1.3 conformant; "KosmicKrisp achieves Vulkan 1.4 conformance on macOS" (LunarG headline); 2026-02-15 merge brought MoltenVK feature parity (adds VK_EXT_texel_buffer_alignment, VK_EXT_extended_dynamic_state2, VK_EXT_image_2d_view_of_3d, depthBiasClamp, largePoints, pushDescriptor); Apple Silicon only, alpha — [Phoronix](https://www.phoronix.com/news/kosmickrisp-parity), [LunarG SDK 1.4.335](https://www.lunarg.com/?p=4002)
- steamac: KosmicKrisp "requires macOS 26+ and is the default where available"; Stellar Blade Demo ~29 FPS KosmicKrisp vs ~18 MoltenVK (M1 Max); KosmicKrisp limits: "no sparse 3D textures, so no Tiled Resources Tier 3. Transform feedback with strip geometry shaders is incomplete"; `logicOp` unavailable; "DirectX 12 is limited to Shader Model 6.0 on Apple GPUs, with no SM 6.2+"; Dragon's Dogma 2 reports FL 12_1 because ROV and conservative rasterization are unavailable on Metal; Metal pipeline compile 50–100 ms uncached, ~1 ms cached — [steamac wiki](https://github.com/fxgl/steamac/wiki)

**DXVK and vkd3d-proton requirements**
- DXVK README: supports D3D8/9/10/11; build needs Wine 10.0+; uses `VK_EXT_graphics_pipeline_library` when available; no macOS mention; zlib license — [doitsujin/dxvk](https://github.com/doitsujin/dxvk)
- Gcenx DXVK-macOS tracks the 1.10.x branch (D3D10/11, zlib) — [Gcenx/DXVK-macOS](https://github.com/Gcenx/DXVK-macOS); Gcenx guide: "Upstream DXVK ... not work on macOS without modifications"; CrossOver 20+ includes DXVK with CodeWeavers' patched MoltenVK — [Gcenx/wine-on-mac](https://github.com/Gcenx/wine-on-mac)
- vkd3d-proton README: Vulkan 1.3 required; descriptor indexing with ≥1,000,000 UpdateAfterBind descriptors; `samplerMirrorClampToEdge`, `shaderDrawParameters`; `VK_EXT_robustness2`, `VK_KHR_push_descriptor`; recommended `VK_EXT_image_view_min_lod`, `VK_EXT_mutable_descriptor_type`, `VK_EXT_descriptor_buffer`; DXR on by default where supported; "no testing on Intel"; LGPL-2.1; no MoltenVK/macOS mention — [vkd3d-proton](https://github.com/HansKristian-Work/vkd3d-proton)
- Historical (2018–2022): WineHQ vkd3d gained MoltenVK support in 2018 (demos only); CodeWeavers said (c. 2021) VKD3D on macOS would not be in good shape before CrossOver 23 due to tessellation and virtual addressing — [Phoronix 2018](https://www.phoronix.com/news/VKD3D-Merges-MoltenVK), [Phoronix 2022](https://www.phoronix.com/news/CodeWeavers-2022-DXVK)
- steamac runs D3D12 via vkd3d-proton: "feature level 12_0 on KosmicKrisp and 11_0 on MoltenVK" — [steamac wiki](https://github.com/fxgl/steamac/wiki)

**Metal 4 (WWDC25 "Discover Metal 4")**
- Apple silicon only (M1+/A14+). New `MTL4CommandQueue`/`MTL4CommandBuffer`/`MTL4CommandAllocator`, parallel encoding, unified compute encoder, `MTL4ArgumentTable` (bindless-friendly), residency sets, **placement sparse resources** (pages from a placement heap), stage-to-stage barrier API, `MTL4Compiler` with QoS and unspecialized→specialized pipelines, pipeline harvesting, tensors in API and MSL, ML command encoder, Shader ML, MetalFX frame interpolation and denoising; Xcode 26 Metal 4 template — [WWDC25 session 205](https://developer.apple.com/videos/play/wwdc2025/205/)
- 2026 additions: MetalFX temporal upscaler using Neural Engine/Neural Accelerators on M5 Pro/Max; Metal command-line debugging tools; MetricKit frame-rate data — [Apple games what's new](https://developer.apple.com/games/whats-new/)
- KosmicKrisp is described as "Mesa's Vulkan driver on Metal 4" by steamac — [steamac wiki](https://github.com/fxgl/steamac/wiki)

### Inferences
- Minimal viable graphics path for an open-source launcher: DXMT for DX10/11 (native Metal, LGPL, ARM64EC-capable), wined3d for DX9 and older (or DXVK-macOS 1.10 via MoltenVK), and vkd3d-proton over KosmicKrisp (macOS 26+) or MoltenVK for DX12 with explicit expectations (FL 12_0, SM 6.0, no raytracing, no ROV/conservative raster, no Tiled Resources T3). Realistic ceilings: DX11 — competitive with CrossOver (same DXMT); DX12 — "many DX12 games boot, modern SM 6.2+/DXR titles don't."
- D3DMetal cannot be bundled by a project that is or might become commercial; a free OSS project's non-commercial redistribution is arguably permitted by the quoted clause but Apple has not confirmed and UTM/Apara have open Feedback requests. Safest design: let the user drop in D3DMetal from their own GPTK download (as Apara does), and treat it as optional.
- Metal 4 placement sparse + argument tables + barriers are the primitives a future DX12-on-Metal layer (or KosmicKrisp) needs for tiled resources/bindless; nothing open-source currently targets them beyond KosmicKrisp, so "DX12 on Metal 4" remains an ambitious-tier item.

### Gaps
- Could not retrieve Apple's GPTK `License.pdf` text; only fragments quoted by third parties.
- No DXMT changelog/version numbers found (releases page empty in capture; wiki stale since 2025-06).
- No direct evidence of DXVK 2.x running on MoltenVK/KosmicKrisp outside the steamac VM (which uses Proton's DXVK 3.x inside Linux). Whether Gcenx's DXVK-macOS has moved beyond 1.10 is unknown.
- No source on MoltenVK vs KosmicKrisp license terms (Mesa is MIT in general; not verified for KosmicKrisp specifically).

---

## Q4. Steam layer: real Windows Steam in Wine vs SteamKit-style reimplementation vs steamcmd/DepotDownloader vs native Mac client

### Takeaway
Three workable depot-download paths exist on macOS today (steamcmd with `@sSteamCmdForcePlatformType windows`, DepotDownloader `-os windows` on .NET 8, or a SteamKit2/JavaSteam/steam-vent client), but running DRM-wrapped titles still requires a Steam client (real Windows Steam in Wine, which Valve explicitly does not support on macOS and which breaks on updates, or GameNative's "Bionic Steam" style reimplementation). The Steam Subscriber Agreement (rev. 2026-09-10) prohibits reverse engineering and emulating Valve's network protocols without written consent, so the SteamKit route is a tolerated grey zone, not a licensed one.

### Cited Findings
- steamcmd: `@sSteamCmdForcePlatformType` supports `windows`, `macos`, `linux` and "lets SteamCMD download files for a platform other than the one it is currently running on"; macOS tarball offered by Valve; `@sSteamCmdForcePlatformBitness` selects 32/64 — [Valve SteamCMD wiki](https://developer.valvesoftware.com/wiki/SteamCMD); a Mac guide installs via Homebrew and fetches Windows games with the flag — [swissmacuser](https://swissmacuser.ch/download-install-steam-game-files-on-mac-os/)
- DepotDownloader: "Steam depot downloader utilizing the SteamKit2 library. Supports .NET 8.0"; Homebrew install on macOS; `-os` (windows/macos/linux, default host), `-osarch`, `-all-platforms`; `-qr` login, `-remember-password`; GPL-2.0 — [SteamRE/DepotDownloader](https://github.com/SteamRE/DepotDownloader)
- Rust: `steam-vent` "allows communication with the steam servers using the same protocol as the regular steam client ... intentionally does not include any high level apis" — [steam-vent](https://docs.rs/crate/steam-vent/latest); `steamroom` provides "client connection, encryption, authentication, and depot manifest parsing" and CDN chunk download — [steamroom 0.3.0](https://docs.rs/crate/steamroom/0.3.0); Aurelia is a headless Rust launcher on steam-vent that can "log in, manage your library, install and update games" — [Aurelia README](https://raw.githubusercontent.com/Drackrath/Aurelia/v0.1.37/README.md)
- JavaSteam (Kotlin SteamKit port) last pushed 2026-04-29 — [Longi94/JavaSteam](https://github.com/Longi94/JavaSteam); GameNative 1.1.0 changelog "fix Steam in China: javasteam query params handling" — [GameNative v1.1.0](https://newreleases.io/project/github/utkarshdalal/GameNative/release/v1.1.0)
- Pluvia: "Lightweight unofficial Steam client for Android," DRM-free games only, Winlator built in, Steam Cloud saves, GPL-3.0; "Development has stalled," maintainer points to GameNative — [oxters168/Pluvia](https://github.com/oxters168/Pluvia)
- GameNative (utkarshdalal): fork of Pluvia; v1.3.0; GPL-3.0 with THIRD_PARTY_NOTICES for "proprietary components"; fixes land "in proton-wine, in FEX, in the GPU driver wrappers or in the per-game config"; Steam DLC/workshop/branches; Epic, GOG, Amazon; "meant for playing games that you legally own" — [GameNative README](https://github.com/utkarshdalal/GameNative). 1.1.0 notes: "Bionic Steam Client – now works reliably for DRM, online connection, DLC – but only on Proton 10"; 1.2.1 "lightweight Steam implementation based on Bionic, capable of handling ... DRM, DLC and certain online connections" — [v1.1.0 notes](https://newreleases.io/project/github/utkarshdalal/GameNative/release/v1.1.0), [MuyComputer 2026-09-19](https://muycomputer.com/2026/09/19/gamenative-mejora-steam-en-android-y-ya-mueve-gta-v-y-rdr2) (secondary)
- Steam Subscriber Agreement (Revision Date 2026-09-10): §2.A license is for "personal, non-commercial use"; §2.G forbids, without Valve's prior written consent, to "reverse engineer, derive source code from, modify, disassemble, decompile, create derivative works" and to "emulate or redirect the communication protocols used by Valve in any network feature"; §4.B "You agree that you will not tamper with the execution of Steam" and no "unauthorized third-party software"; §4.D termination without notice — [Steam Subscriber Agreement](https://store.steampowered.com/subscriber_agreement/)
- Valve support (quoted by a user, Nov 2024): Steam "is unable to support running its Windows client or Windows games on macOS through emulation software" — [Steam discussion](https://steamcommunity.com/discussions/forum/1/4625853520754546053) (secondhand)
- CodeWeavers: for steamwebhelper crashes "install Steam in a 'new Win 7 64-bit bottle'"; users report every Steam update breaks the bottle; black-window fix: disable GPU accelerated rendering — [CodeWeavers Steam tips](https://www.codeweavers.com/compatibility/crossover/tips/steam/mitigation-to-some-steam-issues-on-mac) (older, 2023–2024)
- FEX issue #5336 (Feb 2026, open): "steamwebhelper inconsistently crashes with multiblock enabled" — [FEX issues](https://github.com/FEX-Emu/FEX/issues?q=macOS+OR+Darwin)
- steamac (VM route): real Steam Deck/Steam Frame client; 583 MB client update applied fine; QR sign-in; Shader Pre-Caching off by default; "anti-cheat systems that block VMs will not work"; Steam Frame sign-in can't pair in VM — [steamac wiki](https://github.com/fxgl/steamac/wiki)

### Inferences
- Native macOS Steam cannot be told to install a Windows depot through its UI; the supported "force platform" knob exists only in steamcmd/DepotDownloader. Using those under the user's own credentials is the lowest-risk depot path. It yields files but not a running `steamclient` for DRM-wrapped executables — the same wall Pluvia hit, which is why GameNative ships a Steam-client reimplementation ("Bionic Steam") and a THIRD_PARTY_NOTICES file mentioning proprietary components.
- For macOS, the equivalent would be either: run real Windows Steam (x86-64, CEF) inside the prefix — works under Rosetta/CrossOver today but is officially unsupported, fragile, and FEX has open steamwebhelper bugs; or mimic GameNative's approach (SteamKit2 .NET 8 runs on macOS; Rust `steamroom`/`steam-vent` are younger). Any Steam-protocol reimplementation is contrary to SSA §2.G's "emulate ... communication protocols" clause; Valve has tolerated SteamKit2/DepotDownloader for years but has not licensed them.
- SteamKit2 itself needs a .NET runtime in the app bundle (notarizable; .NET self-contained publish works on arm64 macOS) — inferred, not researched.

### Gaps
- Could not read GameNative's source to confirm how "Bionic Steam" handles the Steam DRM wrapper / `steam_api` (no primary doc).
- No 2026 primary report on real Windows Steam under CrossOver 26 ARM64/FEX stability (only the FEX multiblock issue).
- No statement from Valve on SteamKit-based clients beyond the SSA text.

---

## Q5. Input: gamepads on macOS, Wine XInput/DirectInput, Steam Input in Wine

### Takeaway
Wine on macOS gets controllers via the OS (IOKit HID / Game Controller) and exposes them as both DirectInput and XInput; CodeWeavers tests Xbox One/Series, DualShock 4 and DualSense; the recurring failure modes are hidraw-mode regressions (DS4 L3/R3) and Steam Input conflicts with D3DMetal, usually fixed by disabling hidraw or Steam Input per game.

### Cited Findings
- CodeWeavers: "regularly test Xbox One, Xbox Series, DualShock 4, and DualSense 5 controllers"; "the game will need to support Windows XInput in order to receive any controller feedback through CrossOver" — [Using controllers in CrossOver](https://support.codeweavers.com/using-controllers-in-crossover)
- CodeWeavers forum: Steam Input problem "only with D3DMetal. If you disable Steam input, the controller should start working again"; CrossOver 25 tip: "Disable Hidraw" ON and Steam Input off per game — [Steam controller tips](https://www.codeweavers.com/compatibility/crossover/tips/steam/game-controller-setup-for-steam) (community)
- Wine bug 56602 (Wine 9.6, macOS): DualShock 4 L3/R3 do nothing with hidraw enabled; disabling hidraw via registry fixes it — [wine-bugs 56602](https://list.winehq.org/hyperkitty/list/wine-bugs@list.winehq.org/thread/E4W2TNXQYEJXI2BVUZTZXTYDEJW6M3VC/)
- Wine ≥3.3: gamepads appear on both DirectInput and XInput (bug 45688, older) — [wine-bugs 45688](https://list.winehq.org/hyperkitty/list/wine-bugs@list.winehq.org/thread/HZFDORQDOLOMPI5Z2S2QY4E3OHRRATRQ/)
- Heroic issue #4169: wired Xbox controller not recognized on macOS Sequoia with the GPTK Wine build — [Heroic #4169](https://github.com/Heroic-Games-Launcher/HeroicGamesLauncher/issues/4169)
- macOS 27 Game Controller: PlayStation Access controller supported; fixes for `GCController.light` persistence and `supportsHIDDevice:` — [macOS 27 release notes](https://developer.apple.com/documentation/macos-release-notes/macos-27-release-notes)
- VM route (steamac): host GameController → uinput virtual pad / DualSense via uhid passthrough; "Steam's SDL maps an emulated DualSense as a PS5 Controller, and Steam opens the pad's hidraw device through its HIDAPI driver"; rumble reaches the Mac; up to four players — [steamac wiki](https://github.com/fxgl/steamac/wiki)
- CrossOver 22.1 updated SDL for controller handling (2023, older) — [Phoronix](https://phoronix.com/news/CrossOver-22.1-Released)

### Inferences
- A launcher should expose per-game toggles for Wine hidraw and Steam Input, and default DualSense/DS4 to non-hidraw on macOS. Native Game Controller framework handling (for the launcher UI) and Wine's internal HID path are separate; Steam Input only works if the real Steam client is running inside the prefix.

### Gaps
- No current documentation of winemac.drv's HID backend internals (IOHIDManager vs GameController) found.
- No evidence either way on Steam Input working with DXMT (only D3DMetal conflict reports).

---

## Q6. Audio, networking, save sync, cloud (minor pain points)

### Takeaway
Few primary sources; the known items are CoreAudio latency in VM routes (~65 ms), Steam Cloud handled client-side by SteamKit reimplementations (Pluvia/GameNative), and Remote Play port conflicts in VM setups.

### Cited Findings
- steamac: audio latency ~65 ms on built-in speakers; mono/stereo mic only; multichannel beyond 5.1.2 untested; LAN Remote Play conflicts with the Mac's own Steam on UDP 27036 — [steamac wiki](https://github.com/fxgl/steamac/wiki)
- Pluvia/GameNative implement Steam Cloud saves in the client — [Pluvia](https://github.com/oxters168/Pluvia), [GameNative](https://github.com/utkarshdalal/GameNative)
- Gcenx GPTK builds bundle GStreamer.framework (1.28.1 in 3.0-3) because Media Foundation games need it — [Gcenx GPTK releases](https://github.com/Gcenx/game-porting-toolkit/releases)

### Inferences
- Media Foundation/GStreamer packaging is a real distribution burden (framework must be codesigned/notarized inside the bundle).

### Gaps
- No sources on Wine macOS CoreAudio/WASAPI latency specifics or networking issues in 2026.

---

## Q7. Distribution/legal: notarization & Gatekeeper, GPTK license, Rosetta removal, LGPL/zlib/Apache/MIT, Steam SSA

### Takeaway
A one-click launcher must be Developer ID-signed, hardened-runtime, notarized, and (for a native ARM64 path) carry a paid-team provisioning profile with the cross-architecture entitlement in every helper app; everything it downloads and executes (Wine, DXMT, GStreamer) must itself be signed/notarized or users face Gatekeeper prompts. The open-source components' licenses (Wine LGPL-2.1, DXMT LGPL-2.1, DXVK zlib, vkd3d-proton LGPL-2.1, MoltenVK Apache-2.0, FEX MIT, Box64 MIT) are compatible with redistribution; D3DMetal is not (non-commercial-only, ambiguous) and Rosetta cannot be bundled at all.

### Cited Findings
- Notarization: "Beginning in macOS 10.15, all software built after June 1, 2019, and distributed with Developer ID must be notarized"; requires Developer ID certificate, "Enable the Hardened Runtime capability for your app and command line targets," secure timestamp, no `get-task-allow`; "apps can load quarantined plug-ins ... only if the plug-in is notarized"; "a host app must include all the entitlements that prospective plug-ins require ... the host executable must declare ... `com.apple.security.cs.allow-unsigned-executable-memory` or `com.apple.security.cs.allow-jit`" — [Apple: Notarizing macOS software](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)
- Hardened-runtime exception entitlement keys (`com.apple.security.cs.allow-jit`, `allow-unsigned-executable-memory`, `disable-library-validation`, `allow-dyld-environment-variables`, `disable-executable-page-protection`) — [Apple: Hardened Runtime](https://developer.apple.com/documentation/security/hardened-runtime) (page body did not load; key names from the notarization doc cross-references and general knowledge — verify)
- Cross-architecture entitlement: paid team only; helper must be an app bundle with its own App ID and profile; unmanaged variant not provisionable (FB25187024, FB25285950) — [Apple forum 845976](https://developer.apple.com/forums/thread/845976), [852591](https://developer.apple.com/forums/thread/852591)
- macOS 27 Gatekeeper: `spctl --purge` added — [macOS 27 release notes](https://developer.apple.com/documentation/macos-release-notes/macos-27-release-notes)
- Rosetta: not bundle-able, system-provided, "not automatically restored after upgrading to macOS 27.0"; general support ends after macOS 27 — [Apple Rosetta doc](https://developer.apple.com/documentation/apple-silicon/about-the-rosetta-translation-environment), [macOS 27 release notes](https://developer.apple.com/documentation/macos-release-notes/macos-27-release-notes)
- GPTK/D3DMetal license: "distribute the Apple Software solely for non-commercial purposes" (quoted fragment); UTM and Apara have asked Apple for a clearer redistribution grant (FB24262864) — [Apple forum 841547](https://developer.apple.com/forums/thread/841547)
- Licenses: DXMT LGPL-2.1 — [3Shain/dxmt](https://github.com/3Shain/dxmt); DXVK zlib — [dxvk](https://github.com/doitsujin/dxvk); vkd3d-proton LGPL-2.1 — [vkd3d-proton](https://github.com/HansKristian-Work/vkd3d-proton); DepotDownloader GPL-2.0 — [DepotDownloader](https://github.com/SteamRE/DepotDownloader); Pluvia/GameNative GPL-3.0; Whisky GPL-3.0 — [Whisky](https://github.com/Whisky-App/Whisky); steamac Apache-2.0 with separate GPL source tarball — [steamac](https://github.com/fxgl/steamac/wiki)
- Steam SSA restrictions (rev. 2026-09-10) as quoted in Q4 — [SSA](https://store.steampowered.com/subscriber_agreement/)
- Community builds today are not notarized: Gcenx guide tells users to use `--no-quarantine` or disable Gatekeeper — [Gcenx/wine-on-mac](https://github.com/Gcenx/wine-on-mac); steamac notes builds are "ad-hoc signed unless they come from the notarized dist.sh pipeline" — [steamac wiki](https://github.com/fxgl/steamac/wiki)
- Whisky's author: "The revenue from CrossOver is what keeps Wine on Mac alive"; Whisky "adds no macOS-specific fixes, so it contributes practically nothing back to Wine" and "could harm CrossOver's profitability" (2025-04-09) — [Whisky maintenance notice](https://docs.getwhisky.app/maintenance-notice)

### Inferences
- Because Wine's LGPL requires source availability, CodeWeavers' ARM64-macOS Wine changes will become available (and Shanks is upstreaming them); an OSS project can legally rebuild them. FEX (MIT) changes may stay private.
- A launcher that downloads Wine builds at runtime must either notarize those builds itself (requiring the project to be the Developer ID signer of thousands of Wine DLL/dylib files — feasible with `codesign --deep` + notarytool, as steamac does) or accept Gatekeeper friction. "Downloads and executes translated binaries" is fine for Gatekeeper as long as the executing host (Wine) is signed; Windows PE files are not Mach-O and are not Gatekeeper-checked, but Wine's `wine64-preloader`/`wineserver`/dylibs are.
- JIT: Rosetta-translated Wine needs no JIT entitlement; native FEX-in-Wine needs `allow-jit` (MAP_JIT) at minimum and, for 4K-page/x18/TSO features, the restricted cross-architecture entitlement — meaning no free-team or unsigned distribution of the native path until Apple fixes FB25187024.

### Gaps
- Exact GPTK License.pdf wording not retrieved.
- No Apple statement on whether the macOS 28 "legacy games" Rosetta subset covers Wine-hosted x86-64 processes (Apple's answer was only "x86_64 macOS games should continue to work").
- No legal analysis found on SSA §2.G vs SteamKit-based open-source clients beyond the text itself.

---

## Q8. Prior art on macOS: Whisky, Kegworks/Sikarugir, Heroic, Porting Kit, PlayCover, Mac Source Ports, Gcenx, GPTK Homebrew tap, Apara, steamac — lessons

### Takeaway
Every successful macOS launcher is a thin GUI over CrossOver-derived Wine and Apple/Gcenx binaries; the one that got the most traction (Whisky) was abandoned because its author concluded it free-rode on CodeWeavers without contributing fixes. The projects that survive either monetize (Porting Kit, CrossOver, Apara) or own real engineering (steamac's VM stack, DXMT).

### Cited Findings
- Whisky: repo archived 2025-05-11, GPL-3.0, "built on CrossOver 22.1.1 and Apple's Game Porting Toolkit," credits D3DMetal and Gcenx's DXVK-macOS; notice (2025-04-09): no Wine 8+ upgrade, author lost interest as an unpaid student, original goal was a frontend for any engine "without bundling one," GPTK "muddied the project's direction"; "If Whisky no longer meets your needs, buy CrossOver" — [Whisky repo](https://github.com/Whisky-App/Whisky), [maintenance notice](https://docs.getwhisky.app/maintenance-notice)
- Kegworks (Wineskin successor) rebranded Sikarugir; backends WineD3D, DXVK, VKD3D (limited DX12), D3DMetal, DXMT; WineCX engines; targets Sequoia/Tahoe; wrappers must be recreated on migration — [MacHow2](https://machow2.com/kegworks-officially-renamed-to-sikarugir/) (secondary)
- Heroic: FAQ lists CrossOver support on macOS and "Wine, Proton, Crossover, DXVK, and VKD3D"; 2.15 added GPTK via Wine Manager; 2.18 fixed wrong wine binary with GPTK; Epic launcher fails under GPTK — [Heroic FAQ](https://heroicgameslauncher.com/faq), [GamingOnLinux 2.18](https://www.gamingonlinux.com/2025/07/heroic-games-launcher-2-18-adds-ge-proton-prioritisation-improved-ui-navigation-and-new-analytics/), [AppleGamingWiki](https://applegamingwiki.com/w/index.php?diff=8241)
- Porting Kit: commercial-ish (paulthetall), Homebrew cask 6.7.0 with a Tahoe build (2025-11-20) — [Homebrew cask](https://formulae.brew.sh/cask/porting-kit)
- PlayCover (iOS apps on M-series): conflicting license status; forks claim the original "became non-free" — [PlayCover fork](https://github.com/IsaacMarovitz/PlayCover) (secondary)
- Mac Source Ports: builds universal binaries, "signs and notarises them" — [GameTechWiki](https://emulation.gametechwiki.com/index.php/Source_Ports_on_macOS)
- Gcenx: provides WineHQ-style macOS builds, `wine-crossover` cask, DXVK-macOS, GPTK "3.0-x" community tarballs with GStreamer — [Gcenx/wine-on-mac](https://github.com/Gcenx/wine-on-mac), [Gcenx GPTK releases](https://github.com/Gcenx/game-porting-toolkit/releases)
- Apple's own GPTK usage pattern: Homebrew tap `apple/homebrew-apple` (Gcenx builds reference commit 2bc4428); GPTK 4 (2026) adds agent skills and Metal 4 evaluation — [Gcenx GPTK releases](https://github.com/Gcenx/game-porting-toolkit/releases), [Apple games what's new](https://developer.apple.com/games/whats-new/)
- Apara: "macOS compatibility platform for Windows programs — Apple Silicon only"; "public Mac build is not published yet"; explains failures such as kernel anti-cheat; not affiliated with Apple/CodeWeavers/Wine — [Apara-App on GitHub](https://github.com/Apara-App); its developer (yogvid) is the poster of the `__PAGEZERO` thread and the D3DMetal license request — [Apple forum 845976](https://developer.apple.com/forums/thread/845976), [841547](https://developer.apple.com/forums/thread/841547)
- steamac: 799 stars, Apache-2.0, notarized DMG, SteamOS 20261007 build, Proton 11.0-2 ARM64, FEX, KosmicKrisp/MoltenVK, optional DXMT, ~150 GB disk, Sentry telemetry default-on — [steamac wiki](https://github.com/fxgl/steamac/wiki)
- CodeWeavers (2023) on GPTK: GPTK is "powered by CrossOver source code" — [CodeWeavers 2023 blog](https://www.codeweavers.com/blog/mjohnson/2023/6/6/wine-comes-to-macos-apple-s-game-porting-toolkit-powered-by-crossover-source-code)

### Inferences
- The community lesson is explicit in Whisky's notice: a GUI that bundles CrossOver's engine without engineering contribution is both fragile (breaks on each macOS/Wine change) and socially corrosive to the one company funding Wine-on-Mac. A new project needs a differentiated engineering contribution (e.g., the Steam layer, a Darwin FEX, DXMT work, or the VM route) and a sustainability answer.
- steamac shows that a notarized, one-click, non-Rosetta product is possible today by accepting a VM (150 GB, ~2x GPU overhead on MoltenVK, better on KosmicKrisp) — the "muvm approach on a macOS host."

### Gaps
- No post-mortem for Kegworks→Sikarugir or Porting Kit usage numbers.
- Apara's architecture is not public (no build released).

---

## Q9. Feasibility tiers for an open-source Rust/Swift project, and the VM alternative (libkrun/krunkit + FEX + Proton)

### Takeaway
MVP (months, no new systems research): a notarized Swift/Rust app that downloads notarized Gcenx-style x86-64 Wine builds + DXMT/DXVK-macOS, downloads Windows depots with the user's credentials via DepotDownloader/SteamKit2, builds a prefix, maps controllers, and launches under Rosetta — but it is end-of-life with macOS 28 (2027). Intermediate: the same on native ARM64 Wine + a Darwin FEX once upstream Wine merges Shanks' patches — requires a paid Developer ID with the cross-architecture entitlement and a Darwin FEX (not public). Ambitious: a DX12-on-Metal-4 layer. Parallel pragmatic track: the steamac-style libkrun VM route already runs FEX + Proton 11 with Venus → KosmicKrisp on macOS 26+.

### Cited Findings
- libkrun/krunkit on macOS: "GPU support in the guest via Mesa's Venus Vulkan driver"; macOS ≥14, Apple Silicon; experimental; needs patched Mesa in guest — [Lima krunkit docs](https://lima-vm.io/docs/config/vmtype/krunkit/); Podman 5.2 made libkrun a provider, coupled with krunkit — [Podman blog 2024-07](https://blog.podman.io/2024/07/podman-and-libkrun/); stack described as Venus serialization → virglrenderer → MoltenVK → Metal over Hypervisor.framework — [KVM Forum 2024 slides](https://kvm-forum.qemu.org/2024/The_many_faces_of_virtio-gpu_F4XtKDi.pdf); Podman Desktop doc (older) warned the virtual GPU "only supports vulkan compute shaders, not rendering / draw" — [Podman Desktop GPU doc](https://podman-desktop.io/docs/podman/gpu); llama.cpp users confirm Venus in containers on M2 Max, slower than native — [llama.cpp #8042](https://github.com/ggml-org/llama.cpp/discussions/8042)
- steamac proves rendering works: guest Mesa Venus over virtio-gpu (blob, 16K alignment) → libkrun → virglrenderer (Venus) → MoltenVK (UTM fork + patches) or KosmicKrisp → Metal; 4K-page guest kernel with Apple TSO for FEX; Proton 11.0-2 ARM64 (ARM64EC) with x86-64 via FEX, or x86-64 Proton 10 under FEX; vkd3d-proton for D3D12; Stellar Blade Demo 1280x800 60 FPS on M4 Max, ~29 FPS KosmicKrisp vs ~18 MoltenVK on M1 Max; shader compile stutter 50–100 ms per new pipeline; FEX stopped once "due to a DEP check in its protected .exe"; "Proton Experimental and x86-64 Proton 11.0 need Steam Linux Runtime 4.0, which the arm64 client won't install"; anti-cheat that blocks VMs fails; suspend state lost on quit; DXMT path "is not a VM security boundary" — [steamac wiki](https://github.com/fxgl/steamac/wiki)
- Mesa Venus renderer requirements list Linux hosts — [Mesa Venus docs](https://docs.mesa3d.org/drivers/venus); UTM (QEMU) posted an RFC (Dec 2025, v2 since) to bring "Vulkan guest paravirtualization to macOS hosts," most work going to virglrenderer, not merged — [QEMU RFC on patchew](https://patchew.org/QEMU/20251203040754.94487-1-j@getutm.app/); UTM v5.0.1 release notes: "Vulkan 1.3 is now supported on Linux guests with VirtIO Venus drivers" — [UTM v5.0.1](https://github.com/utmapp/UTM/releases/tag/v5.0.1)
- Apple Virtualization.framework Linux graphics are described in a 2022 Apple forum attachment as virtio-GPU 2D only — [Apple forum attachment](https://developer.apple.com/forums/content/attachment/a3ccc7eb-93b4-49ee-816f-a18024c72560) (weak/old; see Gaps)
- Fedora Asahi runs FEX inside a 4K-page microVM (muvm/krun) for 16K hosts — [Fedora Asahi x86 docs](https://docs.fedoraproject.org/en-US/fedora-asahi-remix/x86-support/)
- GameNative on Android (the comparison target) shipped DRM support via a Bionic Steam client only on Proton 10 and reports "Not every game runs yet" — [GameNative README](https://github.com/utkarshdalal/GameNative), [v1.1.0 notes](https://newreleases.io/project/github/utkarshdalal/GameNative/release/v1.1.0)
- Apple's native-path prerequisites and the paid-team entitlement — [wine-devel](https://list.winehq.org/hyperkitty/list/wine-devel@list.winehq.org/thread/CKG5CEN2BE5VRXZ7O7NX4YUSBH3247WH/), [Apple forum 845976](https://developer.apple.com/forums/thread/845976)

### Inferences (effort tiers, qualitative)
- **Tier 0 — MVP "Rosetta era" (small team, 2–4 months):** Swift app shell (Game Controller, Sparkle-less self-update, notarization pipeline) + Rust core (depot download via `steamroom`/SteamKit2 subprocess, manifest/branch handling, prefix builder, per-game config DB). Engines: Gcenx Wine (WineHQ or CrossOver-source), DXMT, DXVK-macOS, optional user-supplied D3DMetal. Runs on macOS 14–27. Known ceiling: DRM-wrapped games need real Steam in the prefix (unsupported by Valve, fragile) or a reimplementation; DX12 weak; dies with macOS 28.
- **Tier 1 — Native ARM64 (medium, 6–12 months, blocked on externals):** needs (a) upstream Wine with Shanks' macOS-ARM64 loader/TSO/x18/4K patches, (b) a Darwin build of FEX's ARM64EC/WOW64 modules (CodeWeavers' is not public; upstream says not planned; community `FEX_MacOs` repo not reachable), (c) a paid Apple team to provision `com.apple.developer.cross-architecture-support` in each helper `.app`, (d) ARM64EC DXMT (build file exists upstream). Payoff: survives macOS 28, no Rosetta, 4K-page processes real.
- **Tier 2 — VM route (medium, mostly integration):** fork/learn from steamac: libkrun + Venus + KosmicKrisp + Valve's ARM64 SteamOS/Proton 11 + FEX. Gives real Steam client, Steam Input, DRM, updates, Proton compatibility DB for free; costs disk (150 GB), VM boundary issues (anti-cheat), ~2x GPU overhead on MoltenVK, less on KosmicKrisp, and no D3DMetal. Legally cleanest Steam story (official client) but ships Valve's SteamOS image (licensing not researched).
- **Tier 3 — Ambitious (research-grade):** a DX12→Metal 4 layer (placement sparse, argument tables, barriers, SM 6.x via Metal Shader Converter equivalent) or upstreaming a Darwin FEX host; multi-year.
- The Android handheld comparison (GameNative/Winlator: Box64/FEX + Wine/Proton + Turnip/DXVK/VKD3D + Bionic Steam) maps to macOS as: Box64/FEX → Rosetta (now) / Darwin FEX (future); Turnip → Metal via DXMT/KosmicKrisp; Bionic Steam → SteamKit2/steamroom + a client shim (not yet existing on macOS outside real Steam-in-Wine).

### Gaps
- No benchmark comparing CrossOver ARM64+FEX vs Rosetta Wine vs steamac VM on the same hardware.
- Could not verify Apple Virtualization.framework's current (2026) Linux 3D story from a primary doc; all evidence says 3D on macOS hosts comes from libkrun/virglrenderer or UTM/QEMU patches, not VZ.
- Licensing of redistributing Valve's SteamOS ARM64 image (as steamac does) not researched.
- No source on whether `steamroom`/`steam-vent` support the newer Steam auth (QR/JWT) flows; DepotDownloader does.
