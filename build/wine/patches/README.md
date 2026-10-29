# Wine patches

Every `*.patch` in this directory is applied by `build/wine/build.sh` with
`patch -p1` to the pinned Wine source after the msync patch set, in file-name
order. Each patch starts with a header block:

```
Origin: <upstream URL, merge request, or "applejack">
Reason: <why the payload needs it>
Drop-when: <the condition under which the patch is removed>
```

Patches are committed; archives and build output are not. The msync patches
are not stored here; they come from the `wine-msync` pin and gate G4b records
whether one applied.

No patches yet.
