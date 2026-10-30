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

No patches yet.
