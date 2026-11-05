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
The shell's own controller list (plan 06, `Controllers.swift`) uses the
GameController framework and is display only: the game never sees it.

`hidraw` is Wine's second path: with it enabled, a game can open the pad as
a raw HID device and talk its own protocol, which is what Steam Input and
the DualSense's adaptive triggers want. On macOS it is also where the known
failures are:

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
`HKLM\System\CurrentControlSet\Services\winebus\Parameters\DisableHidraw`,
`1` for disabled, `0` for enabled) in the `seeds.reg` it imports when a
prefix is created and again whenever a setting changes between runs.

## The hidraw default

`game_config.hidraw_enabled` has three states:

| `hidraw_enabled` | Effective mode | Source |
|---|---|---|
| `true` | enabled | `config` |
| `false` | disabled | `config` |
| unset | disabled with a connected DualShock 4 or DualSense, else enabled | `controllers` |

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
```

When both Sony pads are connected the DualSense is named. A pad that is
paired but away does not count. A run of a title through the Steam client
prints the line only when it builds or may change the client prefix; while
the client is up and owns the prefix, settings wait for a stop, as every
other prefix setting does.

The inventory is read only when `hidraw_enabled` is unset: `system_profiler`
answers in about 0.2 s once warm but takes over a second on a cold start, and
an explicit setting has nothing to ask it.

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
hidraw_enabled   = (unset)
hidraw_effective = "false"
hidraw_source    = "controllers"
hidraw_reason    = "DualSense connected"
```

Over RPC the same answer is `config.resolve`
(`{appid, hidraw_enabled, hidraw_source, hidraw_reason}`), with
`hidraw_enabled` the effective value; see `docs/rpc-protocol.md`.

Start with the default. If a DualShock 4's sticks do not click, or a pad is
dead under D3DMetal, set `hidraw_enabled false`; if a game needs raw HID
access to the pad (adaptive triggers, a native DualSense mode) and the pad
works, set it `true`.

## `applejack doctor --controllers`

Prints one line per pad `system_profiler` lists and the default the whole
inventory gives, the text of a run's `prepare` line:

```text
DualSense Wireless Controller  bluetooth  0x054c:0x0ce6  dualsense  connected  hidraw default disabled
Xbox Wireless Controller  usb  0x045e:0x0b12  xbox  connected  hidraw default enabled
hidraw disabled (DualSense connected)
```

The columns are name, transport (`usb` or `bluetooth`), vendor and product
id, kind (`dualshock4`, `dualsense`, `xbox`, `other`), whether it is
connected, and the mode that pad alone would select. With nothing paired or
plugged in it prints `no controllers` and `hidraw enabled (no Sony pad
connected)`. Plain `applejack doctor` adds a `controllers  <n> connected`
row; `applejack doctor --json` carries the inventory as `controllers`, the
same field `host.doctor` returns.

The inventory comes from two pure parsers over `system_profiler
SPUSBDataType -json` and `SPBluetoothDataType -json`
(`crates/applejack-core/src/host/controllers.rs`, fixtures under
`crates/applejack-core/tests/fixtures/system_profiler/`):

- Bluetooth lists every paired device under `device_connected` or
  `device_not_connected`; a pad is one with the `Gamepad` minor type, or a
  known Sony pad's vendor and product ids.
- USB carries no device class, so a device is a pad when its ids name a
  known Sony pad, or when its name says `controller`, `gamepad`, or
  `joystick`. A Microsoft mouse is not an Xbox pad; an Xbox pad is.
- Kind by ids: Sony `0x054c` with DualShock 4 `0x05c4` or `0x09cc`, DualSense
  `0x0ce6` or Edge `0x0df2`; Microsoft `0x045e`, any product, is `xbox`;
  everything else is `other`. Headphones, mice, hubs, and phones are left
  out.

A report that fails or does not parse lists nothing: the inventory is a
default, never a refusal.

## In the shell

The Controllers view shows two lists: the pads macOS sees through the
GameController framework (display only) and, beside them, this inventory
from `host.doctor`, one row per pad with the columns above and the rule's
line under them (`hidraw disabled (DualSense connected)`); Refresh re-reads
both, and a core that cannot be reached shows its failure, not an empty
list. The game detail's `hidraw_enabled` toggle, while unset, takes the
position `config.resolve` reports and says why: "Default: off, DualSense
connected" or "Default: on, no Sony pad connected", re-read after every save
and Refresh. See `docs/shell.md`.

## Steam Input and the overlay

`game_config.steam_input` and `steam_overlay` exist and are stored, but
nothing reads them yet: the client keeps both per game in
`userdata/<id>/config/localconfig.vdf`, and the key names are unverified
(`docs/steam-runtime.md`). Until that lands, turn Steam Input off in the
Steam client's own controller settings for a title that misbehaves under
D3DMetal.
