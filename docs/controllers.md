# Controllers

How gamepads reach a Windows title under applejack, what the launcher
decides for them, and how to see and override it.

## How a pad reaches Wine on macOS

A pad paired or plugged into the Mac is a macOS HID device. Wine's
`winebus.sys` reads it through IOKit and presents it to the game as both a
DirectInput and an XInput device; nothing of applejack's sits in that path.
CodeWeavers tests Xbox One, Xbox Series, DualShock 4, and DualSense pads
this way, and a game has to support XInput to see one
([Using controllers in CrossOver](https://support.codeweavers.com/using-controllers-in-crossover)).
The shell's own controller list
(`apps/Applejack/Sources/ApplejackShell/ViewModels/ControllersViewModel.swift`,
`Views/ControllersView.swift`) uses the GameController framework and is
display only: the game never sees it.

`hidraw` is Wine's second path: with it enabled, a game can open the pad as
a raw HID device and talk the pad's own protocol instead of going through
DirectInput or XInput. On macOS it is also where the known failures are:

- DualShock 4 L3 and R3 do nothing under Wine 9.6 with hidraw enabled;
  disabling hidraw in the prefix registry fixes it
  ([wine-bugs 56602](https://list.winehq.org/hyperkitty/list/wine-bugs@list.winehq.org/thread/E4W2TNXQYEJXI2BVUZTZXTYDEJW6M3VC/)).
- Steam Input conflicts with D3DMetal: the pad stops working "only with
  D3DMetal. If you disable Steam input, the controller should start working
  again"; the CrossOver 25 tip is "Disable Hidraw" on and Steam Input off,
  per game
  ([CodeWeavers Steam controller tips](https://www.codeweavers.com/compatibility/crossover/tips/steam/game-controller-setup-for-steam),
  community).
- There is no evidence either way on Steam Input under DXMT; only the
  D3DMetal conflict is reported (`docs/research/macos_build_gaps.md`, Q5).

The engine writes the toggle as a registry value at the bundle's
`[registry] hidraw_key` (for the Rosetta engine
`HKLM\System\CurrentControlSet\Services\WineBus\DisableHidraw`, per
`build/bundle-spec.toml` and `docs/engines/payload-build.md`; `1` for
disabled, `0` for enabled) in the `seeds.reg` it imports when a prefix is
created and again whenever a setting changes between runs. The test stub
engine under `crates/applejack-engine/tests/fixtures/` uses a different key;
it is not the one in a user's prefix.

## The hidraw default

`game_config.hidraw_enabled` has three states:

| `hidraw_enabled` | Effective mode | Source |
|---|---|---|
| `true` | enabled | `config` |
| `false` | disabled | `config` |
| unset, inventory read | disabled with a connected DualShock 4 or DualSense, else enabled | `controllers` |
| unset, one report unavailable, the other lists a connected DualShock 4 or DualSense | disabled | `controllers` |
| unset, inventory unavailable, no connected Sony pad known | the prefix's current mode; disabled for a prefix not built yet | `controllers` |

An explicit value always wins. When it is unset, every run reads the
controller inventory (below) and decides again, so a prefix flips with the
pad: plug a DualSense in and the next run disables hidraw and reapplies the
registry seed through the same path a changed setting takes; unplug it and
the run after that enables it again. The run reports the decision as one
line before the prefix step:

```text
prepare: hidraw disabled (DualSense connected)
prepare: hidraw disabled (DualShock 4 connected)
prepare: hidraw enabled (no Sony pad connected)
prepare: hidraw enabled (config)
prepare: hidraw disabled (config)
prepare: hidraw disabled (controller inventory unavailable (SPBluetoothDataType timed out))
prepare: hidraw enabled (controller inventory unavailable (SPBluetoothDataType timed out))
prepare: hidraw disabled (DualSense connected; SPBluetoothDataType timed out)
```

When both Sony pads are connected the DualSense is named. A pad that is
paired but away does not count.

An inventory is unavailable when one of the two `system_profiler` reports
failed to run, timed out, returned no report, or did not parse. That never
reads as "no pads". A report that failed can only hide pads, never make a
listed one false, so a connected DualShock 4 or DualSense the other report
did list still disables hidraw, with both facts on the line (`DualSense
connected; SPBluetoothDataType timed out`). With no connected Sony pad
known, the run keeps the prefix's current mode, applies nothing, and says
why on the `prepare` line; a prefix that does not exist yet is built with
hidraw disabled, the mode that is safe for a Sony pad. Pads the other
report did list are still shown by `doctor`, but an Xbox pad or an away
Sony pad does not make the read trustworthy.

The inventory is read only when `hidraw_enabled` is unset: `system_profiler`
answers in about 0.2 s once warm and takes over a second on a cold start,
and an explicit setting has nothing to ask it. The two reports run at once,
and each is bounded at 7.5 s (`PROFILER_TIMEOUT`, five seconds, several
times the slowest report measured, and half as much again): past that the
tool and every process it started are killed, as one process group, and the
report is `timed out`. The bound is on the read of the report, not on the
tool's exit alone, so a helper that kept stdout open after the tool was
gone cannot hold a run. The tool is told `-timeout 10` so its own give-up
(default 180 s) never comes first; when a reporter does not answer it, the
tool prints `{}` with exit 0, which reads as `returned no report`, never as
"no pads". So a wedged Bluetooth daemon costs a run at most 7.5 s and an
"unavailable" line, never a silent three-minute stall and never a false
"no Sony pad connected".

### The Steam client prefix

`applejack runtime steam install` builds the shared `steam-client` prefix
with the same rule, read once at install, and prints the same `prepare:`
line before `prefix: creating prefix steam-client`. The client then starts
with the inventory's default, so the first title run through it has nothing
to flip. A run of a title through the client prints the `prepare` line only
while the client is down; while the client is up and owns the prefix, the
mode it was started with stays until `applejack runtime steam stop`, as
every other prefix setting does.

### Overriding it per game

```sh
applejack config <appid> set hidraw_enabled false   # always off for this title
applejack config <appid> set hidraw_enabled true    # always on
applejack config <appid> set hidraw_enabled ""      # back to the default rule
applejack config <appid> show
```

`show` keeps `hidraw_enabled` as stored, so its line still pastes into
`set`, and adds what a run would apply now:

```text
hidraw_enabled        = (unset)
hidraw_effective      = "false"
hidraw_source         = "controllers"
hidraw_reason         = "DualSense connected"
```

Over RPC the same answer is `config.resolve`
(`{appid, hidraw_enabled, hidraw_source, hidraw_reason}`), with
`hidraw_enabled` the effective value; see `docs/rpc-protocol.md`. When the
inventory is unavailable and no connected Sony pad is known, both report
the mode of the prefix the run would use (the title's own, or
`steam-client` for a title on the client path), and `false` when that
prefix is not built yet; a prefix directory that exists but cannot be
opened is the error the run would give, not "not built yet".

Start with the default. If a DualShock 4's sticks do not click, or a pad is
dead under D3DMetal, set `hidraw_enabled false`; if a game talks the pad's
own HID protocol and the pad works, set it `true`.

## `applejack doctor --controllers`

Prints one line per pad `system_profiler` lists and the default the whole
inventory gives, the text of a run's `prepare` line:

```text
DualSense Wireless Controller  usb  0x054c:0x0ce6  dualsense  connected  hidraw default disabled
Xbox Wireless Controller  usb  0x045e:0x0b12  xbox  connected  hidraw default enabled
DualSense Wireless Controller  bluetooth  0x054c:0x0ce6  dualsense  not connected  would disable hidraw if connected
hidraw disabled (DualSense connected)
```

The columns are name, transport (`usb` or `bluetooth`), vendor and product
id (blank when the report has none), kind (`dualshock4`, `dualsense`,
`xbox`, `other`), whether it is connected, and what the rule does with that
pad as it is now: a connected pad reads `hidraw default disabled` or
`hidraw default enabled`; a pad that is paired but away reads `would disable
hidraw if connected` or `would leave hidraw enabled if connected`, since it
does not count toward the last line. With nothing paired or plugged in it
prints `no controllers` and `hidraw enabled (no Sony pad connected)`. When a
report could not be read it prints the pads it did get and then, unless
one of them is a connected Sony pad (then the decision line, `hidraw
disabled (DualSense connected; SPBluetoothDataType timed out)`), instead of
the decision line,

```text
controller inventory unavailable (SPBluetoothDataType timed out): a new prefix gets hidraw disabled, an existing prefix keeps its mode
```

Plain `applejack doctor` adds a `controllers  <n> connected` row, or
`controllers  unavailable (<why>)`. `applejack doctor --json` carries the
inventory as `controllers` and, only when a report failed,
`controllers_unavailable`, the same members `host.doctor` returns;
`doctor --controllers --json` prints those members alone.

The inventory comes from two pure parsers over `system_profiler
SPUSBDataType -json` and `SPBluetoothDataType -json`
(`crates/applejack-core/src/host/controllers.rs`, fixtures under
`crates/applejack-core/tests/fixtures/system_profiler/`):

- Bluetooth lists every paired device under `device_connected` or
  `device_not_connected`; a pad is one with the `Gamepad` minor type, a
  known Sony pad's vendor and product ids, or a name that says `controller`,
  `gamepad`, or `joystick` (an Xbox pad reporting no minor type). A device
  under both lists is one pad, connected.
- USB carries no device class, so a device is a pad when its ids name a
  known Sony pad, or when its name says `controller`, `gamepad`, or
  `joystick`. A Microsoft mouse is not an Xbox pad; an Xbox pad is. A hub
  (a device with `_items`) is never a pad by its name, so a "USB 3.0 Hub
  Controller" is left out. Two identical pads are two rows.
- Kind by ids: Sony `0x054c` with DualShock 4 `0x05c4`, `0x09cc`, or the
  `0x0ba0` USB wireless adaptor, DualSense `0x0ce6` or Edge `0x0df2`;
  Microsoft `0x045e`, any product, is `xbox`; everything else is `other`.
  A pad with an id missing is `other` with that id blank. Headphones, mice,
  hubs, and phones are left out.
- Each device is read leniently: an id written as a number is still an id,
  and an odd or missing field blanks that field or drops that device, never
  the report. The report's own array must be there: `{"SPUSBDataType": []}`
  (the real empty report) is no pads; `{}` or a `null` array is `returned no
  report`, and text that is not JSON, not a report object, or has the array
  as something else `does not parse`. All three make the inventory
  unavailable.

### Pinning the inventory (tests only)

`APPLEJACK_CONTROLLERS` replaces the host's inventory for the `applejack`
binary and the core: `none` for no pads, `unavailable` for a read that
failed, or the path of a JSON file holding either an array of pads in the
`host.doctor` shape or the object `doctor --controllers --json` prints
(`{"controllers": [...], "controllers_unavailable": "..."}`), so that
output pins itself. The CLI suite sets it, and the core suites pin the
inventory through `Core::with_controllers`, so no test runs
`system_profiler`; the core's own lib test installs a recording stand-in
for the tool and fails if `HostFacts::gather` or the engine doctor ever
reach it. It is not a user setting and may change without notice.

## In the shell

The Controllers view shows two lists: the pads macOS sees through the
GameController framework (display only) and, beside them, this inventory
from `host.doctor`, one row per pad with the columns above in the same
words (`hidraw default disabled` for a connected Sony pad, `would disable
hidraw if connected` for one that is away) and the rule's line under them
(`hidraw disabled (DualSense connected)`). When `controllers_unavailable`
is set the view lists what the other report gave; a connected DualShock 4
or DualSense among them still decides, and the rule's line carries the
failure after the pad (`hidraw disabled (DualSense connected; SPUSBDataType
timed out)`), the same rule as `Inventory::decides_hidraw` above, mirrored
in Swift by `HidrawDefault.decide(_ inventory:)`. Without such a pad the
view shows the `controller inventory unavailable (<why>): ...` sentence
above in place of the rule's line, never "no pads". Refresh re-reads both,
and a core that cannot be reached shows its failure, not an empty list.
The game detail's `hidraw_enabled` toggle, while unset, takes the position
`config.resolve` reports and says why with `hidraw_reason` as the core
gives it: "Default: off, DualSense connected", "Default: on, no Sony pad
connected", "Default: off, controller inventory unavailable
(SPBluetoothDataType timed out)", or "Default: off, DualSense connected;
SPUSBDataType timed out", re-read after every save and Refresh. See
`docs/shell.md`.

## Steam Input and the overlay

`game_config.steam_input` and `steam_overlay` exist and are stored, but
nothing reads them yet: the client keeps both per game in
`userdata/<id>/config/localconfig.vdf`, and the key names are unverified
(`docs/steam-runtime.md`). Until that lands, turn Steam Input off in the
Steam client's own controller settings for a title that misbehaves under
D3DMetal. This follow-up is tracked here; the repository has no issue
tracker project for it.

`applejack_config::resolve` (`crates/applejack-config/src/resolve.rs`)
still reads an unset `hidraw_enabled` as `false`; the run path does not use
that resolver, and whoever wires it in must route the unset case through
`applejack_core::host::HidrawDecision`.
