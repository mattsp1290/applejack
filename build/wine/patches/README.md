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

Wine is LGPL: this directory is the source of the changes the bundle's `wine/`
carries. `THIRD_PARTY_NOTICES.md` in the bundle links it at the commit that last
changed it, which `build/assemble.sh` reads from Git, so a bundle meant to leave
this repository is assembled from a commit on `main`.
