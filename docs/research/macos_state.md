# Running Windows (x86-64, DirectX) Steam games on Apple Silicon macOS — state as of October 2026

Scope note: "current" below means verified against sources dated 2026 unless a finding is explicitly marked with an earlier date. Several primary pages (codeweavers.com blog posts, appleinsider.com, Apple's Rosetta doc page, Fedora Asahi docs) returned HTTP 403 / bot-blocks during this research; where that happened I relied on secondary coverage and say so.

---

## KQ1. Apple Game Porting Toolkit (GPTK): versions, contents, license, AVX2/Rosetta, macOS 26 Tahoe / Metal 4

### Takeaway
GPTK is now at version 4 (beta announced WWDC26, June 2026; requires macOS 27 per third-party reporting; translates DX12 → Metal 4). Its lineage is 1.0 (June 2023) → 1.1 (Jan 2024) → 2.0 (June 2024, AVX2 via macOS 15 Rosetta) → 2.1 (Mar 2025) → 3.0 (betas Aug–Oct 2025, final Dec 2025, shipped in CrossOver 26 as "D3DMetal 3.0") → 4 (June 2026). The Wine half is open source (CrossOver-derived, published by Apple via the `apple/homebrew-apple` tap); D3DMetal is a closed binary whose license permits distribution "solely for non-commercial purposes," and Apple has not answered developer requests to clarify redistribution.

### Cited Findings

**What GPTK contains (Apple's current page, GPTK 4)**
- Apple's official page lists the current version as "Game Porting Toolkit 4" and describes an "Evaluation environment for Windows games" that lets you "run an unmodified Windows executable on Apple silicon to test compatibility, performance, and shaders," with "sparse buffers and textures, performance insights, HDR, upscaling, denoising, and frame generation," working with Metal HUD, Metal GPU capture and Metal System Trace. The page says the evaluation environment is "Also available through Homebrew and CrossOver (CodeWeavers)." — [Apple GPTK page](https://developer.apple.com/games/game-porting-toolkit)
- Other listed components: Metal shader converter (DXIL → Metal library; supports frame buffer fetch, function constants, intersection function buffers, debug info), Metal-cpp, Metal Developer Tools for Windows (Metal compiler, offline binary generator, texture converter), Mac Remote Developer Tools for Windows (build/debug CMake projects on a remote Mac from Visual Studio), and new open-source "game porting agent skills" in `github.com/apple/game-porting-toolkit`. "The evaluation environment now supports Metal 4." — [Apple GPTK page](https://developer.apple.com/games/game-porting-toolkit)
- Apple's WWDC26 games guide: "The evaluation environment now supports Metal 4, allowing you to test compatibility and performance against the latest API from the very start of your porting process." Agent skills "provide best practices and deep Metal knowledge for every step of the porting experience"; "agents can now capture, debug, and profile Metal workloads directly" via new command-line Metal tools. The same page describes Metal 4.1's "redesigned temporal upscaler that harnesses the Neural Engine and Neural Accelerators on M5 Pro and M5 Max." — [Apple WWDC26 Games guide](https://developer.apple.com/wwdc26/guides/games/)
- WWDC26 session 357 is "Speedrun your game port with agentic coding." — [Apple WWDC26 session 357](https://developer.apple.com/videos/play/wwdc2026/357/)
- Apple's page does NOT state DirectX version coverage, license terms, macOS requirements, or Wine/D3DMetal version numbers. — [Apple GPTK page](https://developer.apple.com/games/game-porting-toolkit)

**Version history**
- GPTK 1.0 released 6 June 2023 (WWDC23); "combines Wine with D3DMetal, which supports DirectX 11 and 12"; DX9 unsupported. — [AppleGamingWiki GPTK](https://www.applegamingwiki.com/wiki/Game_Porting_Toolkit)
- CodeWeavers confirmed on 6 June 2023 that GPTK's Wine is "powered by CrossOver source code." — [CodeWeavers blog, 2023-06-06](https://www.codeweavers.com/blog/mjohnson/2023/6/6/wine-comes-to-macos-apple-s-game-porting-toolkit-powered-by-crossover-source-code)
- Gcenx's redistributed builds: 1.1 (8 Jan, not linked against GStreamer so Media Foundation games don't work), 1.1-gstreamer (18 Jan), 2.1 (12 Mar), 3.0 beta 3/4/5 (6 Aug / 5 Sep / 10 Oct), 3.0 (5 Dec), 3.0-1/3.0-2 (13 Feb), 3.0-3 (3 Mar, latest). Years are not printed on the page; ordering implies 1.1 = 2024, 2.1 = 2025, 3.0 = Dec 2025, 3.0-x = 2026. Every release says Wine sources are "based on commit apple/homebrew-apple@2bc4428" and carries the notice "please ensure you comply with Apple's License.pdf for D3DMetal." 3.0 series requires "Apple Silicon Macs running macOS 14 Sonoma or higher," 16 GB RAM recommended. — [Gcenx GPTK releases](https://github.com/Gcenx/game-porting-toolkit/releases)
- CrossOver's changelog pins D3DMetal versions to CrossOver releases: CrossOver 25.0.0 (11 Mar 2025) "Update to D3DMetal 2.1"; CrossOver 26.0.0 (10 Feb 2026) "Update to D3DMetal 3.0." — [CodeWeavers changelog](https://www.codeweavers.com/crossover/changelog)
- GPTK 2 (June 2024, WWDC24) "promised AVX/AVX2 support"; "GPTK 2 reportedly supports AVX2 on macOS 15.0 and later." Rosetta before Sequoia does not support AVX/AVX2; affected games listed include Death Stranding, Red Dead Redemption 2, Starfield. Wiki recommends Sequoia for AVX/AVX2 games, Sonoma otherwise, Tahoe "if you want to enable experimental MetalFX integration," and warns Ventura causes many steamwebhelper.exe crashes. — [AppleGamingWiki GPTK](https://www.applegamingwiki.com/wiki/Game_Porting_Toolkit)
- GPTK 3 was announced at WWDC25 alongside Metal 4 (9 June 2025). — [AppleInsider, 2025-06-09](https://appleinsider.com/articles/25/06/09/metal-4-game-porting-toolkit-3-boost-frame-rate-ray-tracing-performance) (page returned 403 on fetch; title/date only from search index)
- GPTK 4 announced at WWDC26 (8 June 2026) "ushers in support for agentic coding." — [AppleInsider, 2026-06-08](https://appleinsider.com/articles/26/06/08/game-porting-toolkit-4-ushers-in-support-for-agentic-coding) (403 on fetch)
- A 9 Sep 2026 third-party first look says GPTK 4 beta "translates DirectX 12 to Metal 4, bringing in MetalFX frame interpolation and neural rendering features," requires **macOS 27**, is Apple-silicon-only, "DirectX 11 titles fall back to Metal 3 and gain less," and "CrossOver: not yet supported; Apple's WWDC26 games guide says CrossOver support is coming." Reported benchmark deltas vs prior GPTK: Cyberpunk 2077 ~+10% (M3 Max / M4 Mac mini), Subnautica 2 ~33→35 fps (+6%, 16 GB M4 Mac mini), RDR2 +7 fps (~25%, "MacBook Neo"), 007 First Light 60–70 fps 1080p medium FSR quality after a 20-minute shader compile, GTA V 106→176 fps (+66%, M4 Pro MacBook Pro). The site states it did not run GPTK 4 itself; numbers are compiled from AppleInsider and Macworld. — [Macs Only, 2026-09-09](https://macsonly.com/article/2026/first-look-game-porting-toolkit-4-beta)
- Apple's own WWDC26 test reportedly showed Cyberpunk 2077 on M3 Max at "10% higher frame rate compared to Metal 3." — [AppleInsider, 2026-06-17](https://appleinsider.com/articles/26/06/17/apples-game-porting-toolkit-4-is-a-big-improvement-for-modern-game-coders) (403 on fetch; claim via search summary)

**Metal 4 (WWDC25 session 205 "Discover Metal 4")**
- Metal 4 adds explicit memory management, a separate `MTL4Compiler` with explicit control over shader compilation timing and QoS inheritance, unified command encoders (`MTL4RenderCommandEncoder` with attachment map; unified compute encoder handles blit + acceleration-structure work), `MTL4ArgumentTable` (bindless-friendly argument tables), tensors as first-class types in the API and MSL plus a machine-learning command encoder (CoreML networks via the Metal toolchain), **MetalFX frame interpolation**, **MetalFX denoising** ("during the upscale step to remove noise from ray-traced images rendered with fewer rays"), and placement sparse resources. Supported on "Apple M1 and later, and A14 Bionic and later." — [Apple WWDC25 session 205](https://developer.apple.com/videos/play/wwdc2025/205/)

**License**
- Apple Developer Forums thread "Update license of D3DMetal for redistribution" (created Aug 2026) by the UTM developer: "CrossOvers and Whisky both redistribute D3DMetal in their app. We would like to do so as well but the included license does not make it clear. Please update the license... FB24262864 is also filed." A reply from the maker of "Apara, a commercial Mac app that runs Windows games on Apple silicon" says "Direct3D 12 titles run well only through D3DMetal, and today every customer has to download the Game Porting Toolkit from Apple themselves because the licence doesn't make clear whether a commercial app may redistribute it." A third poster quotes the license: "distribute the Apple Software solely for non-commercial purposes." **No Apple reply in the thread.** — [Apple Dev Forums thread 841547](https://developer.apple.com/forums/thread/841547)
- Secondary quotes of the 2023 license: "limited, non-exclusive, non-transferable, personal copyright license to (i) install, internally use, and test the Apple Software for the sole purpose of developing, testing..."; commentators conclude "You cannot ship a game with D3DMetal." — [Colin Cornaby, June 2023](https://www.colincornaby.me/2023/06/mac-games-at-wwdc-2023/); [Lemmy comment, Aug 2023](https://old.lemmy.dbzer0.com/comment/1632820)
- Kegworks/Sikarugir README: D3DMetal's "license is restrictive and doesn't allow commercial ports." — [Kegworks GitHub](https://github.com/Kegworks-App/Kegworks)

### Inferences
- The D3DMetal license is the binding constraint for any third-party product: CrossOver's inclusion is presumably under a separate agreement with Apple (CodeWeavers wrote the Wine half for Apple), while hobby projects (Whisky, Kegworks, Heroic, Gcenx) rely on the "non-commercial" clause. A commercial product that is not CodeWeavers (Apara, UTM) currently must have the user download GPTK themselves.
- Since CrossOver 26's "D3DMetal 3.0" corresponds to GPTK 3 and Apple says CrossOver support for GPTK 4/Metal 4 is "coming," CrossOver 27 (expected early 2027) is the likely vehicle for D3DMetal 4.
- "AVX2 support" is a Rosetta feature of macOS 15+, not a GPTK feature per se; GPTK 2 was the first version advertised to use it.

### Gaps
- I could not retrieve the actual GPTK 4 License.pdf text or Apple's download page; the "non-commercial purposes" clause is only quoted by a forum user.
- Apple's page does not list GPTK 4's Wine base version or D3DMetal version number; no source gives them.
- GPTK 3's exact release date on Apple's side (vs Gcenx's 5 Dec 2025 build) is not confirmed from Apple.
- Whether GPTK 4 strictly requires macOS 27 comes from one third-party article; Apple's page gives no requirement.

---

## KQ2. CodeWeavers CrossOver for Mac: version, architecture, Steam support, price, games, gaps

### Takeaway
CrossOver 26.3 (21 July 2026) is current: Wine 11.0, D3DMetal 3.0, DXMT 0.72, vkd3d 1.18, Intel-Wine-under-Rosetta-2 with MSync; one-time license US$74 (12 months of updates) or US$494 lifetime. CrossOver 27 (expected early 2027) will be Apple-silicon-only, require macOS 14 Sonoma+, and end 32-bit bottles; a native ARM64 (Rosetta-free, FEX-based) preview exists but lacks D3DMetal/DX12 and is "not expected to be useful beyond testing." Kernel anti-cheat remains a hard blocker.

### Cited Findings

**Release history (official changelog)**
- 26.3.0 (21 Jul 2026): Diablo IV launch fix after game update; Epic Games Launcher downloads; GOG Galaxy loading. 26.2.0 (9 Jun 2026): Helldivers 2 launch fix; "Added additional warnings for 32-bit bottles." 26.1.0 (9 Apr 2026): Death Stranding 2 workaround; mouse input in many Unity games; Battle.net install fix. 26.0.0 (10 Feb 2026): "CrossOver 26 includes Wine 11.0, with over 6,000 improvements"; Wine Mono 10.4.1; vkd3d 1.18; "Update to D3DMetal 3.0."; "Update to DXMT v0.72"; UI updates for macOS Tahoe; fixes for Helldivers 2, Kingdom Come: Deliverance II, Clair Obscur: Expedition 33, Age of Empires IV, Darktide, God of War Ragnarök, Starfield, Final Fantasy VII Rebirth and ~15 others. — [CodeWeavers changelog](https://www.codeweavers.com/crossover/changelog)
- 25.1.1 (15 Sep 2025): "Fix for Intel Tahoe." 25.1.0 (12 Aug 2025): "Fix for Steam downloads with msync enabled," Steam connection fixes, controller fixes. 25.0.0 (11 Mar 2025): Wine 10.0; "Update to MoltenVK 1.2.10."; "Update to D3DMetal 2.1."; "Inclusion of DXMT, a Metal-based implementation of D3D11 on macOS."; per-game settings database; GOG Galaxy and Epic Games Store support; games incl. RDR2, Street Fighter 6, Tekken 8, Path of Exile 2. 24.0.0 (22 Feb 2024): Wine 9.0, MoltenVK 1.2.5, vkd3d 1.10. — [CodeWeavers changelog](https://www.codeweavers.com/crossover/changelog)
- Diablo IV and Overwatch "need both CrossOver 26.1 and macOS Tahoe 26.5 or later"; "macOS Tahoe requires CrossOver 25.1.1 or later." — [MacHow2 review, updated 2026-08-12](https://machow2.com/crossover-mac-review/)

**Architecture**
- Stable CrossOver 26 "needs Rosetta 2 on Apple Silicon" (Intel Wine components under Rosetta). Graphics backends selectable per bottle: Auto (database-recommended), DXMT (DX11 → Metal), D3DMetal (Apple's DX11/DX12 → Metal), DXVK (DX10/11 → Vulkan via MoltenVK), Wine (WineD3D). MSync is an optional sync setting; "DLSS is available through MetalFX with D3DMetal or DXMT, if the game supports it." — [MacHow2 review](https://machow2.com/crossover-mac-review/)
- Dec 2025 technical write-up: Rosetta 2 translates x86_64 → ARM64 (CPU overhead) plus Wine API translation (OS overhead). D3DMetal handles only DX11/DX12; DX9 goes DirectX → DXVK → MoltenVK → Metal. MSync "maps Wine sync primitives to macOS kqueue or Mach semaphores instead of emulating Linux eventfd." Kernel anti-cheat (EAC, BattlEye) cannot run under Wine (ARC Raiders failed). — [carette.xyz deep dive, 2025-12-20](https://www.carette.xyz/posts/deep_dive_into_crossover/)
- CrossOver Preview with native **Linux** ARM64 landed 6 Nov 2025 ("Twist our ARM64"). — [CodeWeavers blog, 2025-11-06](https://www.codeweavers.com/blog/mjohnson/2025/11/6/twist-our-arm64-heres-the-latest-crossover-preview); [GamingOnLinux](https://gamingonlinux.com/2025/11/codeweavers-launch-a-new-crossover-preview-adding-linux-arm64-support)
- Native **Mac** ARM64 preview: "An ARM64 Preview build arrived in July 2026. It needs macOS 26.5, doesn't yet support D3DMetal or DirectX 12, and isn't recommended for everyday use." CrossOver 27 "expected in early 2027, with native ARM64 support." — [MacHow2 review](https://machow2.com/crossover-mac-review/). Another report says CodeWeavers announced the native ARM64 Mac preview on 31 July and is "targeting CrossOver 28" for it, and that the ARM64 path "uses FEX-based translation"; builds with "arm64" in the name include ARM64 DXMT and "the team recommends DXMT for games"; on older macOS "users fall back to Intel Wine and Rosetta"; CodeWeavers says the ARM64 builds are "not expected to be useful beyond testing at this point." — [iThinkDiff](https://www.ithinkdiff.com/?p=343215). A German report dates the first Mac ARM64 preview to August 2026 and says CrossOver 27 will be fully Apple Silicon "next year." — [Borncity](https://borncity.com/news/crossover-fuer-mac-arm64-version-ohne-rosetta-2-verfuegbar/). **Conflict:** CrossOver 27 vs 28 as the target for shipping native ARM64; July vs August 2026 for the preview.

**CrossOver 27 roadmap (CodeWeavers blog 11 June 2026, "What's in and what's out for CrossOver 27" — 403 on fetch; via secondary coverage)**
- "CrossOver Mac 27 will only run on macOS Sonoma or newer, and only on Apple silicon Macs"; existing Intel users can keep using older versions; 32-bit bottles stop working ("it does mean the end of 32-bit bottle support") because "Rosetta 2 translates only 64-bit Intel code"; CodeWeavers says "almost all 32-bit applications run just fine in a 64-bit bottle" and recommends exporting data from 32-bit bottles first; ~97% of users are already on Sonoma or newer. — [Gigazine, 2026-06-12](https://gigazine.net/gsc_news/en/20260612-crossover-27-removes-legacy-support-mac-intel); [AppleInsider, 2026-06-11](https://appleinsider.com/articles/26/06/11/crossover-a-windows-to-mac-gaming-tool-goes-apple-silicon-only); [CodeWeavers blog URL](https://www.codeweavers.com/blog/mjohnson/2026/6/11/whats-in-and-whats-out-for-crossover-27)
- Parallels published a competitive blog, "CrossOver drops Intel Mac support: what it means for you." — [Parallels blog](https://www.parallels.com/blogs/crossover-drops-intel-mac-support-what-it-means-for-you/)

**Pricing**
- CrossOver+ US$74 (£60) for 12 months of updates and support; the purchased version keeps working after support ends; CrossOver Life US$494 lifetime; 14-day free trial. — [MacHow2 review](https://machow2.com/crossover-mac-review/)
- Whisky's author: "It's not a subscription it's a license... It costs less to upgrade your license to a new version than it does to buy a license upfront." — [Whisky maintenance notice](https://docs.getwhisky.app/maintenance-notice)

**Steam inside CrossOver**
- Windows Steam "runs in a bottle and is slower to start than the native Mac client." — [MacHow2 review](https://machow2.com/crossover-mac-review/)
- CodeWeavers tip page (updated 16 June 2026): current workaround for steamwebhelper crashes on Mac is to install Steam in a "new Win 7 64-bit bottle." CodeWeavers staff have said "every time Steam updates, it is breaking the bottle in CrossOver" and a new bottle is the fix. — [CodeWeavers Steam tips](https://www.codeweavers.com/compatibility/crossover/tips/steam/mitigation-to-some-steam-issues-on-mac)
- CrossOver 26.0 "fixed an issue with Steam reopening after being shut down"; 25.1 fixed "Steam downloads with msync enabled." — [CodeWeavers changelog](https://www.codeweavers.com/crossover/changelog)
- AppleGamingWiki lists Steam and Battle.net black screens, Steam crashes with external monitors, "Windows version too old" errors, missing .NET/MSVCRT runtimes, controller problems, and Retina pixelation as recurring GPTK/CrossOver issues. — [AppleGamingWiki GPTK](https://www.applegamingwiki.com/wiki/Game_Porting_Toolkit)

**Anti-cheat**
- "Many anti-cheat systems don't work, and they often block online play. Kernel-level systems such as Vanguard are generally incompatible." — [MacHow2 review](https://machow2.com/crossover-mac-review/)
- EAC "detects actual system architecture (ARM64) and says it's 'unsupported host machine architecture'" (Insurgency Sandstorm). — [AppleGamingWiki Insurgency Sandstorm](https://applegamingwiki.com/wiki/Insurgency_Sandstorm)
- Epic offers EAC to developers who ship native Mac/Linux builds — a different case from Windows builds under Wine. — [desdelinux](https://blog.desdelinux.net/en/epic-games-easy-anti-cheat-service-is-now-compatible-with-linux-and-mac/)
- "Anti-cheat and DRM are unchanged, and Battlefield 6 refuses to run under Wine" (GPTK 4 era). — [Macs Only](https://macsonly.com/article/2026/first-look-game-porting-toolkit-4-beta)
- Apple Developer Forums post reports "D3DMetal's GPU synchronization adds heavy CPU overhead for many D3D12 games on Apple Silicon" (unverified forum claim). — [Apple Dev Forums Graphics & Games topic](https://developer.apple.com/forums/topics/graphics-and-games)

### Inferences
- The 26.2.0 "additional warnings for 32-bit bottles" entry is the in-product runway for the CrossOver 27 removal.
- Because the native ARM64 preview has no D3DMetal/DX12, CodeWeavers appears to be betting on DXMT (and possibly a future ARM64 D3DMetal from Apple) for a Rosetta-free future; this matters because Apple's Rosetta sunset (KQ8) removes the current x86-64 Wine path after macOS 27.
- "32-bit Windows games work via WoW64 + Rosetta" is consistent with CodeWeavers' statement that almost all 32-bit apps run in a 64-bit bottle (Wine's WoW64 runs 32-bit PE code on 64-bit Unix libs, so only x86-64 host code needs Rosetta). Confirmed indirectly, not by a CodeWeavers technical statement.

### Gaps
- CodeWeavers' own CrossOver 26 and CrossOver 27 blog posts returned 403; all details are via changelog + secondary coverage.
- No official CodeWeavers FPS claims were found; performance data is user-reported (see KQ7).
- The CrossOver 27 vs 28 target for shipping native ARM64 is unresolved across sources.

---

## KQ3. Whisky, Kegworks/Sikarugir, Heroic, Gcenx, PlayCover, Lutris

### Takeaway
Whisky was discontinued in April 2025 (notice dated 2025-04-09; repo archived 11 May 2025) with the author stating "Whisky harms Wine on Mac" and recommending CrossOver. Kegworks (Wineskin successor, now branded Sikarugir) and Heroic (GPTK since 2.15, DXMT since 2.19, v2.22 May 2026) remain the free front-ends, all consuming Gcenx's Wine/GPTK builds. PlayCover is iOS-only and Lutris has no macOS build.

### Cited Findings
- Whisky README: "Whisky is no longer actively maintained... Apps and games may break at any time." Built on CrossOver 22.1.1. Repository "archived by the owner on May 11, 2025." — [Whisky GitHub](https://github.com/Whisky-App/Whisky)
- Maintenance notice by Isaac Marovitz (last changed 2025-04-09): WhiskyWine will not get further updates, will not move to Wine 8+, and will not receive app-specific fixes (e.g. for Steam); occasional fixes only if macOS breaks the app (as in 15.4). Reasons: "I lost interest in the project"; "TLDR; Whisky harms Wine on Mac" — Whisky "contributes almost nothing to Wine while relying on CrossOver's work"; "The revenue from CrossOver is what keeps Wine on Mac alive"; "There's no conspiracy, it's simply just not the right thing to do." Recommends buying CrossOver. — [Whisky maintenance notice](https://docs.getwhisky.app/maintenance-notice)
- Kegworks repo (renamed Sikarugir, "the successor to Wineskin"): supports macOS 14.6+, Apple Silicon needs Rosetta 2; engine options: D3DMetal (DX11/12, "license is restrictive and doesn't allow commercial ports"), DXVK (DX10/11), D9VK (DX8/9), DXMT (DX10/11, default), CNC-DDRAW, WineD3D (DX7 and below). Credits Gcenx for maintaining the Wine engines. Warns that sikarugir.com is not affiliated with the project. — [Kegworks GitHub](https://github.com/Kegworks-App/Kegworks)
- Heroic 2.15.0 added GPTK support via Wine Manager; Heroic 2.19 (Jan 2026) added DXMT support on macOS; Heroic 2.22 "Hajrudin" released 14 May 2026. — [AlternativeTo, Jan 2026](https://alternativeto.net/news/2026/1/heroic-games-launcher-2-19-brings-zoom-games-integration-dxmt-support-on-macos-and-more); [iMore on 2.15](https://www.imore.com/mac/one-of-the-best-game-launchers-for-mac-just-got-macos-sonomas-best-gaming-feature)
- Gcenx packages Apple's GPTK Wine + D3DMetal as `game-porting-toolkit` releases (see KQ1). — [Gcenx releases](https://github.com/Gcenx/game-porting-toolkit/releases)
- DXMT itself targets Wine on macOS; Zagrios maintains a fork synced to upstream main as of 2026-10-07. — [Zagrios/dxmt](https://github.com/Zagrios/dxmt)

### Inferences
- All free front-ends share the same supply chain (Gcenx builds of Wine/GPTK + Apple's D3DMetal binary), so their compatibility ceiling is roughly CrossOver-minus-CodeWeavers' per-game fixes.
- PlayCover (iOS/iPadOS apps on Apple Silicon) and Lutris (Linux-only) are out of scope for Windows DirectX titles; I found nothing in 2026 changing that.

### Gaps
- No source gave Kegworks/Sikarugir's latest release number or date.
- Heroic 2.22's macOS-specific changelog was not retrieved.

---

## KQ4. Open-source DX→Metal translation: DXMT, DXVK/MoltenVK, VKD3D-Proton, KosmicKrisp, Metal 4

### Takeaway
DXMT (3Shain/CodeWeavers-adjacent) is the maturing open DX10/11→Metal layer: v0.80 (23 Apr 2026) is the last MIT release before relicensing to LGPL and a planned v1.0, and D3D12 work is actively being merged (part 10 on 26 Oct 2026) but marked "DO NOT USE." MoltenVK reached Vulkan 1.3 (May 2025) and 1.4 (Aug 2025) conformance; LunarG's Mesa-based KosmicKrisp (Metal 4, macOS 26+, Apple Silicon only) is Vulkan 1.4 conformant and is positioned as "the future of Vulkan on Apple." No working VKD3D-Proton-on-macOS path was found.

### Cited Findings

**DXMT**
- README: "A Metal-based translation layer for Direct3D 11 and 10 which allows running 3D applications on macOS using Wine." License files show LGPL-2.1 plus LICENSE.OLD. Dev builds via GitHub Actions. — [3Shain/dxmt](https://github.com/3Shain/dxmt)
- Releases (years inferred from ordering/footer): 0.50 (26 Apr 2025) proper Unix calls + 32-bit WoW64 support; 0.51 (13 May 2025); 0.60 (15 Jun 2025) Direct3D 10, HDR/color space, D3D11 fence + multithread layer, NV vendor extension; 0.61 (12 Aug 2025) lower memory esp. WoW64; 0.70 (17 Oct 2025) new tessellation pipeline on Metal mesh shaders, indirect tessellation; 0.71 (13 Nov 2025) faster first-launch shader compile (AIR shaders + PSO cache cached), DLSS-SR fix; 0.72 (11 Dec 2025) shared resources via D3DKMT (needs Wine 10.18+), experimental Intel Mac support; 0.73 (21 Jan 2026) fast-math change; 0.74 (10 Mar 2026) better MSAA perf, exclusive fullscreen, swapchain gamma; 0.80 (23 Apr 2026) timestamp queries, MTLFence/intrapass barriers, nvapi UAV overlap. "Version 0.80 is the last release under the MIT license, and the project is changing to LGPL. A v1.0 release is planned." On Sonoma, unsupported Metal 3.2 intrinsics become no-ops → upgrade to Sequoia+. — [DXMT releases](https://github.com/3Shain/dxmt/releases)
- D3D12: PR #249 "D3D12 implementation, part 10" merged 26 Oct 2026, adding geometry-shader support (DXIL GS parsing, emitStream/cutStream, SV_RenderTargetArrayIndex, SV_ClipDistance, etc.), `WriteBufferImmediate()`, feature-level reporting; author comment: "**DO NOT USE**." DXIL shader PRs parts 1–4 merged mid-to-late Oct 2026. — [dxmt PR 249](https://github.com/3Shain/dxmt/pull/249); [dxmt actions](https://github.com/3Shain/dxmt/actions)
- CrossOver 25 introduced DXMT (Mar 2025); CrossOver 26 ships DXMT 0.72. — [CodeWeavers changelog](https://www.codeweavers.com/crossover/changelog)
- One macgamingdb Elden Ring report on M4 Pro with DXMT (CrossOver 25.0.1) was "Unplayable — game did not start," while D3DMetal reports on the same chip ran 60 fps. — [macgamingdb Elden Ring](https://macgamingdb.app/games/1245620)

**MoltenVK**
- v1.3.0 (2 May 2025): Vulkan 1.3 support, Metal residency sets, barriers via Metal fences, `MVK_CONFIG_USE_MTLHEAP` on by default. v1.4.0 (20 Aug 2025): Vulkan 1.4 support. v1.4.1 (30 Nov 2025): KHR_maintenance9, shader_fma, swapchain_maintenance1; Metal 4 and Apple10 GPU family logging; new descriptor set/pool implementation; occlusion queries across render passes. v1.4.2 (24 Jul 2026): `VK_EXT_sampler_filter_minmax` on Apple10 GPUs with macOS/iOS 26, `gl_DrawID`, argument-buffer alignment fixes, min target macOS 12. — [MoltenVK releases](https://github.com/KhronosGroup/MoltenVK/releases). **Conflict:** Wikipedia lists "1.4.2 / 30 November 2025"; the releases page ordering gives 1.4.1 = 30 Nov 2025 and 1.4.2 = 24 Jul 2026. — [Wikipedia MoltenVK](https://en.wikipedia.org/wiki/MoltenVK)
- LunarG (29 Jan 2026): MoltenVK is "nearly conformant" with Vulkan 1.4 "with only minor missing functionality"; it supports x86_64 and older OSes. KosmicKrisp (LunarG, Mesa-based, Google-sponsored for the Android emulator) is "fully conformant" but requires Metal 4 and Apple Silicon on macOS 26+; described as "the future of Vulkan on Apple." Both ship in the Vulkan SDK. — [LunarG, State of Vulkan on Apple Jan 2026](https://www.lunarg.com/?p=4048)
- KosmicKrisp passed Vulkan 1.3 CTS Sept 2025 and later Vulkan 1.4 CTS; it is upstreamed to Mesa; Phoronix reported a "parity to MoltenVK" PR in Feb 2026 and later "MoltenVK feature parity." — [LunarG 1.3 conformance](https://www.lunarg.com/lunarg-achieves-vulkan-1-3-conformance-with-kosmickrisp-on-apple-silicon/); [Phoronix KosmicKrisp 1.4](https://www.phoronix.com/news/KosmicKrisp-Vulkan-1.4-2026); [Phoronix parity](https://www.phoronix.com/news/KosmicKrisp-Parity)
- Older winesapOS README: "Vulkan support via MoltenVK is incomplete due to missing functionality in Apple's Metal API." (dated; pre-2025) — [winesapOS README 3.2.1](https://github.com/LukeShortCloud/winesapOS/blob/3.2.1/README.md)

**VKD3D / DX12 open-source**
- CrossOver bundles vkd3d 1.18 (Wine's vkd3d, used on Linux); no source documents VKD3D or VKD3D-Proton functioning over MoltenVK/KosmicKrisp on macOS for games. CrossOver's DX12 path on Mac is D3DMetal only, and Apara's developer states "Direct3D 12 titles run well only through D3DMetal." — [CodeWeavers changelog](https://www.codeweavers.com/crossover/changelog); [Apple Dev Forums 841547](https://developer.apple.com/forums/thread/841547)

### Inferences
- DXMT's D3D12 branch is the only open-source route to a Rosetta-free DX12 on Mac in sight; given "DO NOT USE" as of 26 Oct 2026, it is months (at least) from playable.
- KosmicKrisp requiring Metal 4 means DXVK-over-KosmicKrisp could in principle gain Metal 4 features (argument tables, placement sparse) that MoltenVK on Metal 3 lacks, but no gaming-oriented benchmark of DXVK on KosmicKrisp was found.
- The DXVK/MoltenVK path remains the DX9/DX10 fallback in CrossOver; for DX11 it is now the third choice behind D3DMetal and DXMT.

### Gaps
- No source enumerated which Vulkan 1.3/1.4 features MoltenVK still emulates or lacks (geometry shaders, transform feedback, descriptor-indexing limits) as of 2026; LunarG's article says only "minor missing functionality."
- No head-to-head DXMT vs D3DMetal vs DXVK benchmark table from a reputable outlet in 2026 was found; only user reports.
- VKD3D-Proton on macOS: no 2025–2026 source either confirming or denying it runs; treat as "no known working path."

---

## KQ5. Alternatives: Parallels, VMware Fusion, UTM/QEMU, Asahi Linux + FEX/muvm, Linux VMs with Rosetta

### Takeaway
Parallels Desktop 27 (25 Aug 2026) remains capped at DirectX 11.1 / OpenGL 4.3 for Windows 11 ARM guests (DX12 games do not run; x86 games run via Microsoft's Prism inside the guest); its full-x86 VM mode is "very slow." VMware Fusion offers DX11 for Windows 11 ARM guests. Asahi Linux's Steam-via-FEX+muvm stack works on M1/M2 only: M3 gained installer support in Sept 2026 but still has no GPU driver, and M4/M5 are early bring-up. Apple's Virtualization framework gives Linux VMs Rosetta but no AVX and no 3D/Vulkan acceleration.

### Cited Findings

**Parallels**
- Parallels KB 129497 (last review 25 Aug 2026): "3D acceleration is currently supported at the level of DirectX 11.1 and OpenGL 4.3"; "Some of the latest AAA games may not launch or run"; "Windows 11 on Arm can run thousands of x86 applications. However, some of them may not work today"; no nested virtualization. — [Parallels KB 129497](https://kb.parallels.com/en/129497). Older localized copies say OpenGL 3.3. — [Parallels KB es](https://kb.parallels.com/es/129497)
- Parallels Desktop 27 shipped 25 Aug 2026; Apple-silicon-only (M1–M6); own OpenGL 4.3 driver on Metal (Pro edition; "up to 160% faster" OpenGL per Parallels); "Modern games needing DirectX 12 won't run. Version 27 does not change this." Pricing AU$115/yr Standard, AU$140 Pro, AU$175 Business. — [iTWire, 2026-09-04](https://itwire.com/your-it-news/home-it/parallels-desktop-27-lifts-opengl-to-4-3-on-apple-silicon-and-that-one-number-does-more-work-than-it-looks)
- Parallels' full x86-64 VM emulation (announced Jan 2025) is "apparently incredibly slow, taking several minutes just to boot"; a Mac mini M4 test found Edge took 55 s to open. — [OSNews](https://www.osnews.com/?p=141092); [Macwelt](https://www.macwelt.de/article/1529700/windows-11-am-mac-nutzen.html)
- Microsoft's Prism emulator (Windows 11 24H2) added AVX/AVX2 support; "native ports remain the superior option." — [Windows Latest](https://windowslatest.com/2024/06/20/windows-11-24h2-improves-gaming-peripherals-support-on-arm/amp); [Microsoft Learn x86 emulation](https://learn.microsoft.com/windows/arm/apps-on-arm-x86-emulation)
- Parallels forum (Parallels 20 era): "most DirectX 12 games and utilities don't work, and there is no ETA"; some DX12-listed titles run anyway. — [Parallels forum](https://forum.parallels.com/threads/anyone-try-a-directx-12-game-yet.365154/)
- A third-party blog claims DX12 "enhanced in version 26" — unsupported by Parallels' KB; treat as unreliable. — [Licendi blog](https://licendi.com/en/blog/parallels-desktop-20-vs-26-comparison-guide/)

**VMware Fusion**
- Broadcom KB: Apple Silicon Fusion runs only Arm Windows 11; DirectX 11 graphics available; "Arm Windows includes an emulation layer for Intel-based Windows apps and games that use DirectX 11." A July 2026 community post reports Fusion 26H1 (build 25388279) crashing with 3D acceleration in a CAD app. — [Broadcom KB 315609](https://knowledge.broadcom.com/external/article/315609); [Broadcom community, Jul 2026](https://community.broadcom.com/vmware-cloud-foundation/discussion/fusion-26h1-25388279-crashes-windows-11-arm-on-apple-silicon-when-wintrack-17-uses-3d-acceleration)

**UTM / QEMU**
- UTM's developer is asking Apple for D3DMetal redistribution rights (Aug 2026), implying UTM is building a Wine/GPTK-based path rather than relying on QEMU x86 emulation. — [Apple Dev Forums 841547](https://developer.apple.com/forums/thread/841547)

**Asahi Linux**
- Fedora Asahi Remix: muvm registers FEX as a binfmt provider so x86 binaries run transparently; `dnf install steam`; recommended launch option `FEX_X87REDUCEDPRECISION=1 %command%`. — [Fedora Asahi x86 docs (ja mirror)](https://docs.fedoraproject.org/jp/fedora-asahi-remix/x86-support/) (English page bot-blocked)
- FEX 2603 (Mar 2026) partially fixed the Steam web helper crash on AArch64 and hides big.LITTLE layouts to avoid anti-tamper issues; FEX 2604 (Apr 2026) improved memory savings; Valve sponsors FEX (Steam Frame). — [Phoronix FEX 2603](https://www.phoronix.com/news/FEX-2603-Released); [Phoronix FEX 2604](https://www.phoronix.com/news/FEX-2604-Released)
- Asahi progress report 7.1 (30 June 2026): M3 has audio, CPU freq, PCIe/WiFi/BT/NVMe/input working but "We still have a ways to go before we can start enabling Asahi Installer support for these machines"; M4/A18 Pro groundwork only; m1n1 1.6.0 moved GPU init into m1n1; macOS 27 beta broke the boot picker (APFS bootable flag) — fix tool shipped. — [Asahi progress report 7.1](https://asahilinux.org/2026/06/progress-report-7-1/); [Phoronix](https://www.phoronix.com/news/Asahi-Linux-AVD-Firmware-M3)
- Secondary coverage of a 6 Sept 2026 Asahi post: M3/M3 Pro/M3 Max (not Ultra) added to the installer, but "the GPU is still missing"; M4/M5 "not ready for daily Linux use yet." — [IT-Connect](https://www.it-connect.tech/asahi-linux-officially-supports-m3-macs-but-the-gpu-is-still-missing/); [Ecosistema Startup](https://ecosistemastartup.com/?p=105114); [MacG, Sept 2026](https://www.macg.co/logiciels/2026/09/asahi-linux-dompte-les-puces-m3-en-attendant-lacceleration-graphique-310885)
- User reports: M2 Pro user says most Steam games ~60 fps on Asahi; an M1 user says Asahi is "not there yet" and CrossOver/Parallels ran more games faster. — [Lemmy 1](https://lemmy.brendan.ie/comment/1279362); [Lemmy 2](https://lemmy.brendan.ie/comment/1276521)

**Linux VMs on macOS with Rosetta**
- Apple's Virtualization framework lets Linux VMs use Rosetta for x86-64 Linux binaries (macOS 13+). — [Apple docs](https://developer.apple.com/tutorials/data/documentation/virtualization/running-intel-binaries-in-linux-vms.md); [Phoronix](https://www.phoronix.com/news/macOS-13-Rosetta-Linux-Binaries)
- "There is no way to currently enable AVX1/2 on Rosetta Linux; on macOS, setting ROSETTA_ADVERTISE_AVX=1 works, but not in Linux VMs"; Wine-under-Rosetta in a VM ran some old games at <5 fps (forum post, dated). Linux guest graphics are 2D hand-off only; no source confirms Vulkan/3D acceleration in Apple VZ Linux guests. — [Parallels forum post](https://forum.parallels.com/posts/906955); [WWDC22 10002 notes](https://wwdcnotes.com/notes/wwdc22/10002)
- MacRumors: Apple said "Software running Intel binaries in Linux VMs also continues beyond macOS 27." — [MacRumors, 2026-02-16](https://www.macrumors.com/2026/02/16/macos-tahoe-26-4-rosetta-2-warnings/)

### Inferences
- For DX12 Steam titles, no VM route works today; CrossOver/GPTK is the only practical DX12 path on macOS, and Asahi+Proton (DXVK/vkd3d-proton on Honeykrisp) is the only one on M1/M2 hardware under Linux.
- Linux VMs with Rosetta are not viable for 3D games: no AVX, no GPU acceleration.

### Gaps
- No 2026 game FPS benchmarks for Parallels 26/27 or Fusion were found.
- The primary Asahi Sept 2026 blog post was not fetched; M3 installer claims rest on secondary coverage.
- Asahi's gaming status on M3 is presumably nil without GPU, but no source states that explicitly.

---

## KQ6. Steam on macOS: native client, library share, 32-bit, Proton

### Takeaway
Valve shipped a native Apple Silicon Steam client beta on 13 June 2025 (no stable GA confirmed in sources); macOS is ~1.9% of Steam users (Sept 2026 survey); native-Apple-Silicon game counts are small (~35 per a 2025 count). Valve ended 32-bit macOS game support with macOS 10.14 (Feb 2024 drop), ended 32-bit Windows client support 1 Jan 2026, and has no Proton for macOS (macOS support was removed from Proton in 2018).

### Cited Findings
- Steam Client Beta (13 June 2025): "Steam Client and Steam Helper apps now run natively on Apple Silicon"; no longer requires Rosetta 2. — [Mactrast, June 2025](https://www.mactrast.com/2025/06/new-steam-beta-brings-native-support-for-apple-silicon-powered-macs/amp/); [3DNews, 2025-06-13](https://3dnews.ru/1124401/)
- Steam Hardware & Software Survey, September 2026: Windows 95.03%, macOS 1.92% (−0.22), Linux 3.05%; top macOS versions 26.6.2 (0.69%), 27.0.0 (0.19%), 26.5.2 (0.17%). — [Steam HW survey](https://store.steampowered.com/hwsurvey/)
- Earlier figure: Macs 1.85% of Steam vs Linux 2.69%. — [Thurrott](https://www.thurrott.com/mobile/322110/steam-will-soon-run-natively-on-apple-silicon-macs)
- ~35 games had native Apple Silicon builds on Steam at the time of the native-client beta (non-exhaustive list). — [MacHow2](https://machow2.com/steam-updates-for-apple-silicon/)
- Valve: "macOS 10.14 was the last version to support running 32-bit games on macOS"; Steam ended support for macOS 10.13/10.14 on 15 Feb 2024. — [Blue's News](https://www.bluesnews.com/s/266277/steam-dropping-32-bit-macos-support); [iMore](https://imore.com/gaming/steam-is-dropping-support-for-old-versions-of-macos-and-thats-bad-news-for-beleaguered-mac-gamers)
- Steam dropped macOS 11 Big Sur on 15 Oct 2025 (requires macOS 12+). — [MacRumors, 2025-08-06](https://www.macrumors.com/2025/08/06/steam-client-requires-macOS-12-october-15/)
- Steam client ended support for 32-bit Windows on 1 Jan 2026; 32-bit games still run on 64-bit Windows. — [Neowin](https://www.neowin.net/news/valve-ends-32-bit-steam-support-as-the-app-becomes-64-bit-only/)
- Proton: macOS support was removed from Proton (2018 commit); no official reason in the issue. — [ValveSoftware/Proton issue 1344](https://github.com/ValveSoftware/Proton/issues/1344)
- Valve sponsors FEX for ARM (Steam Frame), i.e., ARM Linux, not macOS. — [Phoronix FEX 2603](https://www.phoronix.com/news/FEX-2603-Released)
- Steam community thread "Future of MacOS Support - Post Tahoe & Rosetta II Phase-Out" exists; user-only. — [Steam discussions](https://steamcommunity.com/discussions/forum/2/532101209809539145)

### Inferences
- Apple's 2026 Rosetta sunset (not Valve) is what "drops" the remaining Intel-only Mac Steam games; Valve's own 32-bit drop was 2024.
- Valve's ARM investment (FEX, Proton on ARM for Steam Frame) benefits Asahi Linux users indirectly but there is no evidence of any Valve macOS translation-layer plan.

### Gaps
- Could not confirm whether the native Apple Silicon Steam client left beta (stable GA date).
- No 2026 count of Apple-Silicon-native titles on Steam; the ~35 figure is mid-2025.
- No official Valve statement on Proton-for-macOS found.

---

## KQ7. Performance data: CrossOver/D3DMetal vs DXMT vs DXVK vs native, by chip

### Takeaway
Measured data: on an M4 Max, Notebookcheck found native Cyberpunk 2077 at 136 fps vs 49 fps via CrossOver at 1080p Low (CrossOver flat ~48 fps across presets, i.e., CPU/translation-bound) but nearly equal at 4K Ultra (23.6 vs 21.3); user databases put Elden Ring at a median 60 fps via CrossOver across M1–M5, BG3 60 fps (native and CrossOver), Hogwarts Legacy median 40 fps, RE4 60 fps CrossOver vs up to 120–170 fps native on M4 Max. No reputable DXMT-vs-D3DMetal benchmark table exists; no Winlator/Snapdragon head-to-head exists.

### Cited Findings

**Native vs CrossOver (same machine)** — MacBook Pro 16 M4 Max (40-core GPU), Notebookcheck, published 20 Nov 2025:
- Cyberpunk 2077, FSR off: 1080p Low native 136.1 / CrossOver 49.1; 1080p Medium 109 / 48.7; 1080p High 95.5 / 48.5; 1080p Ultra 84.3 / 47.4; 1440p Ultra 53.8 / 43; 4K Ultra 23.6 / 21.3. Overall "native advantage of 68%." 1440p RT Ultra native ~37 fps; 4K MetalFX Quality 23.6 → 34.1 fps. vs RTX 5060 Laptop (115 W): Legion 7 94.9 fps 1080p Ultra (+13% over M4 Max). AC Shadows native 1080p Ultra High 33 fps. — [Notebookcheck, 2025-11-20](https://www.notebookcheck.net/Cyberpunk-AC-Shadows-on-Apple-M4-Max-Gaming-performance-comparable-to-the-RTX-5060-Laptop.1166765.0.html)
- **Conflict:** a Turkish report says native was only ~3% faster than CrossOver (settings unknown); the 10% figure sometimes quoted is Notebookcheck's 4K-only delta. — [Donanimhaber](https://www.donanimhaber.com/cyberpunk-2077-mac-lerde-hangi-modelde-nasil-calisiyor--194068)
- Tom's Hardware headline: native Cyberpunk across Apple Silicon generations ranges "from around 13 to 105 fps at 1080p" depending on Mac and settings (full table not retrieved). — [Tom's Hardware, July 2025](https://www.tomshardware.com/laptops/gaming-laptops/cyberpunk-2077-has-been-tested-on-several-apple-silicon-generations-results-range-from-around-13-to-105fps-at-1080p-depending-on-mac-and-game-settings-tested)

**Tahoe / Metal 4 / GPTK 3 beta (CrossOver, Andrew Tsai & others, mid-2025)**
- M1 Max, macOS 26 beta, GPTK 3.0 beta 1 (20 June 2025): Black Myth: Wukong ~70 fps with DLSS (FSR lagged); Hogwarts Legacy DLSS ~45 base → ~90 indoors / 70–80 outdoors with frame gen (FSR ~30–40); Witcher 3 120+ low / 60–70 ultra HairWorks off (DX11, no frame gen); Spider-Man Remastered ~50 fps DLSS+MetalFX. Resolutions not stated. — [Notebookcheck, 2025-06-20](https://www.notebookcheck.net/Black-Myth-Wukong-Hogwarts-Legacy-and-the-Witcher-3-run-at-60-FPS-on-macOS-Tahoe-26-with-Metal-4.1040850.0.html)
- M3 Max, Tahoe dev beta, GPTK 3.0 Beta 1 + CrossOver Preview 0507 (3 July 2025): Starfield 60 fps 1080p medium MetalFX; Horizon Forbidden West, Jurassic World Evolution 2, Avatar: Frontiers of Pandora, Star Wars Outlaws, FF VII Rebirth now launch. Setup needed manual GPTK file swaps. — [Notebookcheck, 2025-07-03](https://www.notebookcheck.net/macOS-Tahoe-update-enables-Star-Wars-Outlaws-Horizon-Forbidden-West-and-more-to-run-on-Apple-Silicon-via-Crossover.1050257.0.html)
- Cyberpunk 2077 (pre-native) hit 60 fps on M1 Max with MetalFX under Tahoe beta. — [Notebookcheck](https://www.notebookcheck.net/YouTuber-tests-macOS-26-Tahoe-gaming-update-as-Cyberpunk-2077-hits-60-FPS-on-MacBook-Pro-M1-Max-with-MetalFX.1034516.0.html)

**GPTK 4 beta deltas (Sept 2026, compiled)** — see KQ1: Cyberpunk ~+10% (M3 Max), GTA V 106→176 fps (M4 Pro), RDR2 +25%, Subnautica 2 +6%. — [Macs Only](https://macsonly.com/article/2026/first-look-game-porting-toolkit-4-beta)

**User-reported (macgamingdb.app; CrossOver, D3DMetal unless noted; undated)**
- Elden Ring: 25 CrossOver reports, median 60 fps, avg 3.7/5. M4 Max 25.0 Ultra 4K 50 fps; M4 Max 24.0 High 1080p 60; M5 Pro 26.0 Ultra 1800x1168 60; M1 Max 26.0 High 2056x1329 70; M4 base 26.0 High 1080p 40; M1 base 25.0 Low 1280x600 45; M4 Pro 25.0.1 DXMT "Unplayable — did not start"; M4 base 25.0 DXVK High 1080p (no fps). — [macgamingdb Elden Ring](https://macgamingdb.app/games/1245620)
- CodeWeavers forum, Elden Ring on M4 Max 32-core: 20–30 fps with RTX on at 1920x1200 High; fine with RTX off; another M4 Max user hits the 60 fps cap, 70–80 uncapped. — [CodeWeavers forum](https://www.codeweavers.com/support/forums/general?t=27&msg=336157)
- Baldur's Gate 3: native 19 reports median 60; CrossOver 3 reports median 60 (M4 Max 25.0.1 High 90 fps; M4 Pro 25.1.1 Ultra 1080p 60; M5 base 26.0 High 60). Native M4 Max High 1080p 80, Ultra 3200x1800 70. — [macgamingdb BG3](https://macgamingdb.app/games/1086940)
- Hogwarts Legacy: 10 CrossOver reports, median 40 fps, avg 3.0/5. M4 Pro 24 GB 25.0 Medium 1080p 50 (shader-compilation stutter; "24 GB is the minimum for Medium"); M1 Max 32 GB Ultra 3024x1964 40; M2 base 8 GB Low 15 "Unplayable"; M5 base 16 GB 26.3 High 1440p 60 with microstutters; M4 base 16 GB 26.0 Medium 60 (stutter reduced by Engine.ini mods). — [macgamingdb Hogwarts](https://macgamingdb.app/games/990080)
- Resident Evil 4: CrossOver 3 reports median 60 (M1 base 25.1.1 Medium 1440x900 40 with heavy stutter; M5 base 26.0 High 65; M1 Pro 26.0 High 60); native M4 Max Ultra 1440p 120 fps, Mac Studio M4 Max Ultra 1080p 170 fps; M4 base Ultra 1080p 70 (MetalFX Quality). — [macgamingdb RE4](https://macgamingdb.app/games/2050650)
- SimplyMac: Cyberpunk 2077 on M4 Max via CrossOver 40–55 fps 1080p Medium/High (version unstated). — [SimplyMac](https://www.simplymac.com/mac/m4-gaming)
- AppleGamingWiki GPTK-era user list (2023, dated): FF VII Remake ~50 fps M1 Pro 1080p High; Cuphead ~90 fps M2 Pro; Vampyr 25–30 M1 Air Ultra. — [AppleGamingWiki GPTK](https://www.applegamingwiki.com/wiki/Game_Porting_Toolkit)

**Snapdragon / Winlator comparison**
- No direct Winlator-vs-CrossOver FPS data found. Synthetic GPU: Snapdragon 8 Elite 18 fps vs M4 (iPad) 26 fps in 3DMark Steel Nomad Light; Solar Bay 41 vs 54. — [NanoReview](https://nanoreview.net/ru/soc-compare/qualcomm-snapdragon-8-gen-4-vs-apple-m4-ipad)

### Inferences
- The flat ~48 fps CrossOver curve across 1080p presets on M4 Max indicates the bottleneck is CPU-side (Rosetta translation + Wine + D3DMetal sync), not GPU; the gap closes at 4K where the GPU dominates. This is consistent with the forum claim about D3DMetal's GPU-sync CPU overhead and with CodeWeavers' native-ARM64 effort.
- User-database medians cluster at 60 fps because many games are capped; uncapped titles (BG3 90, Elden Ring 70–80) show headroom on Max chips.
- Memory is a first-order constraint (Hogwarts 8 GB unplayable, 24 GB min for Medium per a user).

### Gaps
- No reputable, versioned DXMT vs D3DMetal vs DXVK benchmark table for 2026 (only user reports and a sparse DXMT entry).
- Tom's Guide / Tom's Hardware full Cyberpunk tables could not be retrieved (truncated pages).
- No Diablo IV or Control FPS data with hardware/settings was found; Witcher 3 only in the Tahoe beta test.
- Andrew Tsai's own videos were not fetched; all his numbers come via Notebookcheck.

---

## KQ8. Known blockers: anti-cheat, DX12 coverage, AVX-512, 32-bit, page size, shader stutter, memory, Rosetta sunset

### Takeaway
Apple's official position: "Rosetta 2 support for apps will end after macOS 27," with "a subset of Rosetta functionality aimed at supporting older unmaintained gaming titles, that rely on Intel-based frameworks" retained; macOS 27 betas ship a `game-test-tool` that "disables Rosetta" for non-game processes and "is not available outside of macOS beta releases." macOS Tahoe 26.4 (Feb 2026) began warning on every Rosetta app launch. Kernel anti-cheat (EAC/BattlEye/Vanguard) remains unsolvable under Wine; 32-bit bottles end in CrossOver 27; shader-compilation stutter and memory are the recurring practical complaints.

### Cited Findings

**Rosetta roadmap**
- Apple doc quoted by a developer: "Beyond this timeframe, we will keep a subset of Rosetta functionality aimed at supporting older unmaintained gaming titles, that rely on Intel-based frameworks." Apple Graphics & Games engineer (June 2026): "x86_64 macOS games should continue to work as they do on macOS 26. You can preview this behavior in macOS 27 by using game-test-tool as described in the release notes." — [Apple Dev Forums 830692](https://developer.apple.com/forums/thread/830692)
- macOS 27 release notes (quoted by Brendan Shanks): "Note that enabling legacy game support disables Rosetta, non-game processes might crash or behave unexpectedly"; "this feature is intended only for playing legacy Intel-based games and is not available outside of macOS beta releases" (`sudo game-test-tool enable`); Rosetta "is not automatically restored after upgrading to macOS 27.0." — [mjtsai.com, 2025-06-10 (updated)](https://mjtsai.com/blog/2025/06/10/ending-macos-intel-support/); [macOS 27 release notes](https://developer.apple.com/documentation/macos-release-notes/macos-27-release-notes) (body not retrievable)
- MacRumors (16 Feb 2026): macOS Tahoe 26.4 shows a popup that "the app will no longer work when support for Rosetta 2 ends"; Apple says "Rosetta 2 support for apps will end after macOS 27" and it "will continue to support older, unmaintained gaming titles with Rosetta" beyond macOS 27; Linux-VM Rosetta continues. The article does not say "macOS 28." — [MacRumors](https://www.macrumors.com/2026/02/16/macos-tahoe-26-4-rosetta-2-warnings/)
- macOS 27 "Golden Gate" is out (Sept 2026 per Steam survey showing 27.0.0 at 0.19%); Apple ended Intel Mac support with macOS 27. — [Steam HW survey](https://store.steampowered.com/hwsurvey/); [Gigazine](https://gigazine.net/gsc_news/en/20260612-crossover-27-removes-legacy-support-mac-intel)
- How Apple will enforce the games-only subset "is unknown." — [MacRumors forums](https://forums.macrumors.com/threads/macos-28-and-rosetta-2-rosetta-functionality-will-be-available-only-for-certain-older-unmaintained-games-that-rely-on-intel-based-frameworks.2485450/)

**Anti-cheat** — see KQ2 (EAC "unsupported host machine architecture"; Vanguard incompatible; Battlefield 6 refuses Wine).

**DX12 / ray tracing**
- CrossOver's DX12 path is D3DMetal only; Elden Ring with RTX on drops to 20–30 fps on M4 Max. — [CodeWeavers forum](https://www.codeweavers.com/support/forums/general?t=27&msg=336157)
- GPTK 4 brings MetalFX denoising/frame interpolation to the evaluation environment (DX12 → Metal 4), but DX11 titles stay on Metal 3. — [Macs Only](https://macsonly.com/article/2026/first-look-game-porting-toolkit-4-beta)

**AVX / AVX-512**
- AVX/AVX2 only via Rosetta on macOS 15+; a `ROSETTA_ADVERTISE_AVX=1` env var is referenced; not available in Linux VMs. — [AppleGamingWiki GPTK](https://www.applegamingwiki.com/wiki/Game_Porting_Toolkit); [Parallels forum post](https://forum.parallels.com/posts/906955)

**32-bit**
- CrossOver 27 ends 32-bit bottles; 64-bit bottles run "almost all 32-bit applications" (WoW64). DXMT added 32-bit (WoW64) support in 0.50 (Apr 2025). — [Gigazine](https://gigazine.net/gsc_news/en/20260612-crossover-27-removes-legacy-support-mac-intel); [DXMT releases](https://github.com/3Shain/dxmt/releases)

**Shader compilation stutter / memory**
- Hogwarts Legacy reports: shader-compilation stutter on M4 Pro; "24 GB is the minimum for Medium"; 8 GB M2 unplayable at 15 fps. 007 First Light needed "a 20-minute shader compile" under GPTK 4. DXMT 0.71 added caching of converted AIR shaders and PSO cache to speed first launch. CodeWeavers' Atomic Heart tip: force D3DMetal to get the shader cache to build. Gcenx recommends ≥16 GB RAM. — [macgamingdb Hogwarts](https://macgamingdb.app/games/990080); [Macs Only](https://macsonly.com/article/2026/first-look-game-porting-toolkit-4-beta); [DXMT releases](https://github.com/3Shain/dxmt/releases); [CodeWeavers Atomic Heart tip](https://www.codeweavers.com/compatibility/crossover/tips/atomic-heart/getting-around-shader-compilation-failure-due-to-video-memory-error); [Gcenx releases](https://github.com/Gcenx/game-porting-toolkit/releases)

**Page size**
- Fedora Asahi uses muvm (a microVM) to give 4K-page x86 binaries a compatible environment under FEX on 16K-page Apple kernels. — [Fedora Asahi x86 docs](https://docs.fedoraproject.org/jp/fedora-asahi-remix/x86-support/)

### Inferences
- The decisive 2027 question is whether Apple's "legacy game" Rosetta subset will cover Wine/CrossOver's x86-64 Wine processes. Apple's engineer answered only for "x86_64 macOS games"; CodeWeavers is hedging via native ARM64 + FEX + DXMT, which implies they do not expect to rely on it.
- Until Apple ships an ARM64 D3DMetal, a Rosetta-free CrossOver has no DX12 path, so DX12 Steam games on macOS 28+ depend on either (a) Apple extending the games subset to Wine, (b) Apple shipping ARM64 D3DMetal, or (c) DXMT's D3D12 reaching usability.

### Gaps
- Apple's "About the Rosetta translation environment" doc page could not be fetched; the "subset... older unmaintained gaming titles" sentence is verified only via forum quotation.
- No Apple statement on whether Wine/CrossOver processes qualify as "legacy games."
- AVX-512: no source discusses it for Rosetta (Rosetta does not translate it; games requiring it are rare) — unsourced, listed as a gap.
- No source quantifies Metal shader-stutter severity across D3DMetal vs DXMT in 2026.
