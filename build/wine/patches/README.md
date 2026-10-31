# Wine patches

Every `*.patch` in this directory is applied by `build/wine/build.sh` with
`patch -p1` to the pinned Wine source after the msync patch set, in file-name
order. Patches may depend on earlier ones: the script applies the whole set
in order to a clone of the tree first and touches the real tree only when
every patch applied there, so a set that fails leaves the tree unpatched.
`--preflight` runs that trial on the tree without the msync patch applied.
Each patch starts with a header block:

```
Origin: <upstream URL, merge request, or "applejack">
Reason: <why the payload needs it>
Drop-when: <the condition under which the patch is removed>
```

Patches are committed; archives and build output are not. The msync patches
are not stored here; they come from the `wine-msync` pin and gate G4b records
whether one applied.

| Patch | Touches | Source |
| --- | --- | --- |
| `0001-dxmt-macdrv-abi.patch` | `dlls/ntdll/unix/virtual.c`, `dlls/winemac.drv/{macdrv.h,window.c}` | Adapted to Wine 11.0 from cyyever/wine c5375ee66cb, the Wine 11.9 reference in <https://github.com/3Shain/dxmt/issues/170>. LGPL-2.1-or-later, as Wine. |

`0001-dxmt-macdrv-abi.patch` gives DXMT 0.80 the three things it needs from
Wine 11.0: `RTLD_GLOBAL` on Unix libraries (11.0 opens them in
`get_builtin_unix_funcs`, not in `load_unixlib_by_name` as 11.9 does), an
exported `macdrv_functions` table, and a `get_win_data` that returns DXMT's
layout with a view in it. It differs from the reference in two ways. The view
belongs to a client surface instead of being created bare, so win32u resizes it
and disposes of it with the window; a bare view would keep its first frame. The
struct is per window under the win data lock instead of one shared static, so
two windows cannot overwrite each other's. See "DXMT under Wine 11.0" in
`docs/engines/rosetta-wine.md` for the limits.

Wine is LGPL: this directory is the source of the changes the bundle's `wine/`
carries, and `THIRD_PARTY_NOTICES.md` points here.
