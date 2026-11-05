//! The controller inventory behind `applejack doctor --controllers` and the
//! hidraw default: pure parsers over `system_profiler SPUSBDataType -json`
//! and `SPBluetoothDataType -json`, and the rule that turns the inventory
//! into a [`HidrawDecision`] when the game configuration leaves
//! `hidraw_enabled` unset.

use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use applejack_engine::HidrawMode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const SONY: u16 = 0x054c;
const MICROSOFT: u16 = 0x045e;
/// The `DualShock` 4, its second revision, and Sony's USB wireless adaptor
/// for it, which carries the same hidraw regression.
const DUALSHOCK_4: [u16; 3] = [0x05c4, 0x09cc, 0x0ba0];
/// `DualSense` and `DualSense` Edge.
const DUALSENSE: [u16; 2] = [0x0ce6, 0x0df2];

/// The reporter the inventory is read from.
pub const SYSTEM_PROFILER: &str = "/usr/sbin/system_profiler";

/// The bound on one report: the tool and every helper it started are
/// killed, and the report is unavailable, once half as much again has
/// passed (7.5 s). A warm report takes 0.1 to 0.3 s and a cold one about
/// 1.3 s on the measured host, so five seconds is several times the
/// slowest observed report and still shorter than a stall a user reads as
/// a hang. The tool's own `-timeout` (default 180 s) is set to twice this
/// so it never fires first: when it does fire it prints `{}` with exit 0,
/// which reads as `returned no report`, never as "no pads".
pub const PROFILER_TIMEOUT: Duration = Duration::from_secs(5);

/// The environment knob that replaces the host's inventory: `none` for no
/// pads, `unavailable` for a report that could not be read, or the path of
/// a JSON file holding an array of [`ControllerFact`] or an [`Inventory`]
/// object as `doctor --controllers --json` prints it. The CLI test suite
/// sets it so the binary never runs `system_profiler`.
pub const CONTROLLERS_ENV: &str = "APPLEJACK_CONTROLLERS";

/// One pad `system_profiler` lists, over USB or Bluetooth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerFact {
    pub name: String,
    pub transport: Transport,
    /// Lowercase `0x` hex, four digits; empty when the report has no id.
    pub vendor_id: String,
    pub product_id: String,
    /// A USB pad is plugged in; a Bluetooth one may be paired and away.
    pub connected: bool,
    pub kind: ControllerKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Usb,
    Bluetooth,
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Usb => "usb",
            Self::Bluetooth => "bluetooth",
        })
    }
}

/// Keyed by vendor and product id; see [`ControllerKind::classify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControllerKind {
    Dualshock4,
    Dualsense,
    Xbox,
    Other,
}

impl ControllerKind {
    /// Sony's `DualShock` 4 (`0x05c4`, `0x09cc`, the `0x0ba0` adaptor) and
    /// `DualSense` (`0x0ce6`, Edge `0x0df2`); every Microsoft product counts
    /// as an Xbox pad.
    #[must_use]
    pub fn classify(vendor: u16, product: u16) -> Self {
        match vendor {
            SONY if DUALSHOCK_4.contains(&product) => Self::Dualshock4,
            SONY if DUALSENSE.contains(&product) => Self::Dualsense,
            MICROSOFT => Self::Xbox,
            _ => Self::Other,
        }
    }

    /// The pads Wine's hidraw path is known to break (plan 06: the
    /// `DualShock` 4 `L3`/`R3` regression, wine-bugs 56602, and the same
    /// default for the `DualSense`).
    #[must_use]
    pub const fn hidraw_regresses(self) -> bool {
        matches!(self, Self::Dualshock4 | Self::Dualsense)
    }

    /// The hidraw mode this pad alone would select if it were connected.
    #[must_use]
    pub const fn hidraw_default(self) -> HidrawMode {
        if self.hidraw_regresses() {
            HidrawMode::Disabled
        } else {
            HidrawMode::Enabled
        }
    }

    /// The name on a `doctor` or `prepare` line.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Dualshock4 => "DualShock 4",
            Self::Dualsense => "DualSense",
            Self::Xbox => "Xbox",
            Self::Other => "other",
        }
    }
}

impl fmt::Display for ControllerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Dualshock4 => "dualshock4",
            Self::Dualsense => "dualsense",
            Self::Xbox => "xbox",
            Self::Other => "other",
        })
    }
}

/// What a read of the inventory produced: the pads listed, and why the
/// read is not to be trusted when one report failed to run, timed out, or
/// did not parse. An unavailable inventory never reads as "no pads".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inventory {
    #[serde(rename = "controllers", default)]
    pub pads: Vec<ControllerFact>,
    #[serde(
        rename = "controllers_unavailable",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub unavailable: Option<String>,
}

impl Inventory {
    /// A complete read.
    #[must_use]
    pub const fn available(pads: Vec<ControllerFact>) -> Self {
        Self {
            pads,
            unavailable: None,
        }
    }

    /// A read that could not be trusted, with `why`.
    #[must_use]
    pub fn unavailable(why: impl Into<String>) -> Self {
        Self {
            pads: Vec::new(),
            unavailable: Some(why.into()),
        }
    }

    #[must_use]
    pub const fn is_available(&self) -> bool {
        self.unavailable.is_none()
    }

    /// Whether the pads decide the hidraw default on their own: a complete
    /// read does, and so does a connected `DualShock` 4 or `DualSense` the
    /// one report that answered did list, since the report that failed can
    /// only hide pads, never make a listed one false. Otherwise the prefix's
    /// mode holds.
    #[must_use]
    pub fn decides_hidraw(&self) -> bool {
        self.is_available()
            || HidrawDecision::from_controllers(&self.pads).mode == HidrawMode::Disabled
    }
}

impl From<Vec<ControllerFact>> for Inventory {
    fn from(pads: Vec<ControllerFact>) -> Self {
        Self::available(pads)
    }
}

/// Where the effective `hidraw_enabled` came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HidrawSource {
    /// The game configuration sets it.
    Config,
    /// The configuration leaves it unset; the connected pads decide.
    Controllers,
}

/// The hidraw mode a run applies, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidrawDecision {
    pub mode: HidrawMode,
    pub source: HidrawSource,
    /// `config`, `DualSense connected`, `DualShock 4 connected`, `no Sony
    /// pad connected`, or `controller inventory unavailable (<why>)`.
    pub reason: String,
}

impl HidrawDecision {
    /// An explicit configuration always wins; otherwise the inventory
    /// decides: disabled with a connected `DualShock` 4 or `DualSense`,
    /// enabled with none. An inventory with a report missing still
    /// disables for a connected Sony pad the other report listed; with no
    /// such pad it keeps `current`, the prefix's mode, and gives a new
    /// prefix `Disabled`.
    #[must_use]
    pub fn decide(
        config: Option<bool>,
        inventory: &Inventory,
        current: Option<HidrawMode>,
    ) -> Self {
        match config {
            Some(enabled) => Self {
                mode: mode_of(enabled),
                source: HidrawSource::Config,
                reason: "config".to_owned(),
            },
            None => Self::from_inventory(inventory, current),
        }
    }

    /// The default with nothing configured; see [`Self::decide`].
    #[must_use]
    pub fn from_inventory(inventory: &Inventory, current: Option<HidrawMode>) -> Self {
        let listed = Self::from_controllers(&inventory.pads);
        match &inventory.unavailable {
            Some(why) if listed.mode == HidrawMode::Disabled => Self {
                reason: format!("{}; {why}", listed.reason),
                ..listed
            },
            Some(why) => Self {
                mode: current.unwrap_or(HidrawMode::Disabled),
                source: HidrawSource::Controllers,
                reason: format!("controller inventory unavailable ({why})"),
            },
            None => listed,
        }
    }

    /// The default a complete inventory gives.
    #[must_use]
    pub fn from_controllers(controllers: &[ControllerFact]) -> Self {
        let culprit = controllers
            .iter()
            .filter(|pad| pad.connected && pad.kind.hidraw_regresses())
            .map(|pad| pad.kind)
            // The DualSense is named first when both are connected.
            .max_by_key(|kind| *kind == ControllerKind::Dualsense);
        let (mode, reason) = match culprit {
            Some(ControllerKind::Dualsense) => (HidrawMode::Disabled, "DualSense connected"),
            Some(_) => (HidrawMode::Disabled, "DualShock 4 connected"),
            None => (HidrawMode::Enabled, "no Sony pad connected"),
        };
        Self {
            mode,
            source: HidrawSource::Controllers,
            reason: reason.to_owned(),
        }
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        matches!(self.mode, HidrawMode::Enabled)
    }
}

/// The `prepare: hidraw enabled (no Sony pad connected)` form.
impl fmt::Display for HidrawDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = if self.enabled() {
            "enabled"
        } else {
            "disabled"
        };
        write!(f, "hidraw {mode} ({})", self.reason)
    }
}

const fn mode_of(enabled: bool) -> HidrawMode {
    if enabled {
        HidrawMode::Enabled
    } else {
        HidrawMode::Disabled
    }
}

/// The inventory the process should use: the [`CONTROLLERS_ENV`] knob when
/// it is set, else the host's.
#[must_use]
pub fn inventory() -> Inventory {
    std::env::var_os(CONTROLLERS_ENV).map_or_else(gather, |knob| inventory_from_knob(&knob))
}

/// The two shapes a [`CONTROLLERS_ENV`] file may take.
#[derive(Deserialize)]
#[serde(untagged)]
enum KnobFile {
    Pads(Vec<ControllerFact>),
    Inventory(Inventory),
}

/// The inventory a [`CONTROLLERS_ENV`] value stands for.
#[must_use]
pub fn inventory_from_knob(knob: &OsStr) -> Inventory {
    if knob == "none" {
        return Inventory::default();
    }
    if knob == "unavailable" {
        return Inventory::unavailable(format!("{CONTROLLERS_ENV}=unavailable"));
    }
    let path = Path::new(knob);
    let file = fs::read_to_string(path)
        .map_err(|error| error.to_string())
        .and_then(|text| {
            serde_json::from_str::<KnobFile>(&text).map_err(|error| error.to_string())
        });
    match file {
        Ok(KnobFile::Pads(pads)) => Inventory::available(pads),
        Ok(KnobFile::Inventory(inventory)) => inventory,
        Err(why) => Inventory::unavailable(format!("{CONTROLLERS_ENV} {}: {why}", path.display())),
    }
}

/// Runs both `system_profiler` reports at once, each bounded by
/// [`PROFILER_TIMEOUT`], and lists the pads in them, USB first. A report
/// that fails to run, times out, returns no report, or does not parse makes
/// the inventory unavailable with the reason; the pads of the other report
/// are still listed.
#[must_use]
pub fn gather() -> Inventory {
    gather_from(&profiler(), PROFILER_TIMEOUT)
}

/// The reporter [`gather`] runs: [`SYSTEM_PROFILER`], or the program a
/// test installed in its place.
fn profiler() -> PathBuf {
    #[cfg(test)]
    if let Some(program) = tests::profiler_override() {
        return program;
    }
    PathBuf::from(SYSTEM_PROFILER)
}

type Parser = fn(&str) -> Result<Vec<ControllerFact>, ReportError>;

/// [`gather`] with the reporter and its bound chosen; tests point it at a
/// shim.
#[must_use]
pub fn gather_from(program: &Path, timeout: Duration) -> Inventory {
    let reports: [(&str, Parser); 2] = [
        ("SPUSBDataType", parse_usb),
        ("SPBluetoothDataType", parse_bluetooth),
    ];
    // Both at once: two stalled reports cost one bound, not two.
    let results = std::thread::scope(|scope| {
        let handles = reports.map(|(data_type, parse)| {
            scope.spawn(move || {
                report(program, data_type, timeout)
                    .and_then(|text| parse(&text).map_err(|error| error.to_string()))
            })
        });
        handles.map(|handle| {
            handle
                .join()
                .unwrap_or_else(|_| Err("the report thread panicked".to_owned()))
        })
    });
    let mut inventory = Inventory::default();
    let mut failures = Vec::new();
    for ((data_type, _), result) in reports.into_iter().zip(results) {
        match result {
            Ok(pads) => inventory.pads.extend(pads),
            Err(why) => failures.push(format!("{data_type} {why}")),
        }
    }
    if !failures.is_empty() {
        inventory.unavailable = Some(failures.join("; "));
    }
    inventory
}

/// One report's JSON, read within `timeout` and half as much again: past
/// that the tool and every process it started are killed and the report is
/// a failure, never a hang. The read, not only the child's exit, is what
/// the bound covers, since a helper that inherited stdout would otherwise
/// hold the pipe open after the tool is gone.
fn report(program: &Path, data_type: &str, timeout: Duration) -> Result<String, String> {
    // The tool's own give-up prints `{}` with exit 0; told twice our bound
    // it never comes first, so a stall reads as `timed out`.
    let tool_timeout = (timeout * 2).as_secs().max(1).to_string();
    let mut child = Command::new(program)
        .args([data_type, "-json", "-timeout", &tool_timeout])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Its own process group, so the kill reaches a helper the tool forked.
        // That also takes it out of the terminal's foreground group: a Ctrl-C
        // ends us, not it; it then exits on `SIGPIPE` or its own `-timeout`.
        .process_group(0)
        .spawn()
        .map_err(|error| format!("cannot run {}: {error}", program.display()))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "no stdout pipe".to_owned())?;
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = sender.send(stdout.read_to_end(&mut text).map(|_| text));
    });
    let deadline = Instant::now() + timeout + timeout / 2;
    let read = match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(read) => read,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            end(&mut child);
            return Err("timed out".to_owned());
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            end(&mut child);
            return Err("the reader thread panicked".to_owned());
        }
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                end(&mut child);
                return Err("timed out".to_owned());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                end(&mut child);
                return Err(format!("cannot wait for the report: {error}"));
            }
        }
    };
    let text = read.map_err(|error| format!("cannot read the report: {error}"))?;
    if !status.success() {
        return Err(format!("exited with {status}"));
    }
    Ok(String::from_utf8_lossy(&text).into_owned())
}

/// Kills the tool's whole process group (its pid, by `process_group(0)`)
/// and reaps the tool. `/bin/kill` carries the group signal because the
/// crate forbids unsafe code and takes no `libc`.
fn end(child: &mut Child) {
    let _ = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{}", child.id())])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

/// Why a report's text lists no pads.
#[derive(Debug)]
pub enum ReportError {
    /// The report lacks its own data type's array, or has it `null`: the
    /// `{}` that `system_profiler -timeout` prints when the reporter did
    /// not answer in time. Only a present, empty array is "no pads".
    NoReport,
    /// Not JSON, not a report object, or the data type is not an array.
    Malformed(serde_json::Error),
}

impl fmt::Display for ReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoReport => f.write_str("returned no report"),
            Self::Malformed(error) => write!(f, "does not parse: {error}"),
        }
    }
}

impl std::error::Error for ReportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NoReport => None,
            Self::Malformed(error) => Some(error),
        }
    }
}

impl From<serde_json::Error> for ReportError {
    fn from(error: serde_json::Error) -> Self {
        Self::Malformed(error)
    }
}

/// The `data_type` array of a report.
fn report_items(text: &str, data_type: &str) -> Result<Vec<Value>, ReportError> {
    let Value::Object(mut report) = serde_json::from_str::<Value>(text)? else {
        let found = json_kind(&serde_json::from_str::<Value>(text)?);
        return Err(ReportError::Malformed(serde::de::Error::custom(format!(
            "expected a report object, found {found}"
        ))));
    };
    match report.remove(data_type) {
        None | Some(Value::Null) => Err(ReportError::NoReport),
        Some(Value::Array(items)) => Ok(items),
        Some(other) => Err(ReportError::Malformed(serde::de::Error::custom(format!(
            "{data_type} is {}, not an array",
            json_kind(&other)
        )))),
    }
}

const fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn array_of(value: Option<&Value>) -> &[Value] {
    value.and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn text_of(value: Option<&Value>) -> String {
    value.and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// An id as the reports write it (`"0x054C"`, `"0x054c  (Sony Corporation)"`)
/// or, leniently, as a bare number.
fn id_of(value: &Value) -> Option<u16> {
    match value {
        Value::String(text) => parse_hex_id(text),
        Value::Number(number) => number.as_u64().and_then(|id| u16::try_from(id).ok()),
        _ => None,
    }
}

/// The pads in `system_profiler SPUSBDataType -json`: every device under a
/// bus or hub whose vendor and product name a known pad, or whose name says
/// it is one. The USB report carries no device class, so a Microsoft mouse
/// is told from an Xbox pad by its name; a hub (a device with `_items`) is
/// never a pad by its name. Each device is read leniently: an odd or
/// missing field drops or blanks that field, never the report. Two
/// identical pads are two rows.
///
/// # Errors
///
/// The text is not JSON or not a report object, or the report has no
/// `SPUSBDataType` array.
pub fn parse_usb(text: &str) -> Result<Vec<ControllerFact>, ReportError> {
    let mut pads = Vec::new();
    for bus in &report_items(text, "SPUSBDataType")? {
        collect_usb(bus, &mut pads);
    }
    Ok(pads)
}

fn collect_usb(item: &Value, pads: &mut Vec<ControllerFact>) {
    let Some(item) = item.as_object() else {
        return;
    };
    let vendor = item.get("vendor_id").and_then(id_of);
    let product = item.get("product_id").and_then(id_of);
    let hub = item.contains_key("_items");
    pads.extend(
        pad_of(
            text_of(item.get("_name")),
            Transport::Usb,
            vendor,
            product,
            false,
            true,
        )
        .filter(|pad| !hub || pad.kind.hidraw_regresses()),
    );
    for child in array_of(item.get("_items")) {
        collect_usb(child, pads);
    }
}

fn names_a_pad(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    ["controller", "gamepad", "joystick"]
        .iter()
        .any(|word| name.contains(word))
}

/// The pads in `system_profiler SPBluetoothDataType -json`: every paired
/// device under `device_connected` or `device_not_connected` with the
/// `Gamepad` minor type, a known pad's vendor and product, or a name that
/// says it is a pad. A device listed under both is one pad, connected. Each
/// device is read leniently, as [`parse_usb`] does.
///
/// # Errors
///
/// The text is not JSON or not a report object, or the report has no
/// `SPBluetoothDataType` array.
pub fn parse_bluetooth(text: &str) -> Result<Vec<ControllerFact>, ReportError> {
    let mut pads = Vec::new();
    for entry in &report_items(text, "SPBluetoothDataType")? {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        for (list, connected) in [("device_connected", true), ("device_not_connected", false)] {
            // Each device is an object with one key, the device's name.
            for device in array_of(entry.get(list)) {
                let Some(device) = device.as_object() else {
                    continue;
                };
                for (name, fields) in device {
                    let field = |key: &str| fields.as_object().and_then(|fields| fields.get(key));
                    let gamepad = field("device_minorType")
                        .and_then(Value::as_str)
                        .is_some_and(|minor| minor.eq_ignore_ascii_case("gamepad"));
                    pads.extend(pad_of(
                        name.clone(),
                        Transport::Bluetooth,
                        field("device_vendorID").and_then(id_of),
                        field("device_productID").and_then(id_of),
                        gamepad,
                        connected,
                    ));
                }
            }
        }
    }
    dedupe(&mut pads);
    Ok(pads)
}

/// A device as a pad when its ids name a known Sony pad, the report calls it
/// a gamepad, or its name says it is one; `None` for everything else.
fn pad_of(
    name: String,
    transport: Transport,
    vendor: Option<u16>,
    product: Option<u16>,
    gamepad: bool,
    connected: bool,
) -> Option<ControllerFact> {
    let kind = match (vendor, product) {
        (Some(vendor), Some(product)) => ControllerKind::classify(vendor, product),
        _ => ControllerKind::Other,
    };
    if !gamepad && !kind.hidraw_regresses() && !names_a_pad(&name) {
        return None;
    }
    Some(ControllerFact {
        name,
        transport,
        vendor_id: vendor.map(hex_id).unwrap_or_default(),
        product_id: product.map(hex_id).unwrap_or_default(),
        connected,
        kind,
    })
}

/// One row per Bluetooth pad: the same name and ids listed under both the
/// connected and the not-connected list is one pad, connected.
fn dedupe(pads: &mut Vec<ControllerFact>) {
    let mut kept: Vec<ControllerFact> = Vec::with_capacity(pads.len());
    for pad in pads.drain(..) {
        if let Some(seen) = kept.iter_mut().find(|seen| {
            seen.transport == pad.transport
                && seen.name == pad.name
                && seen.vendor_id == pad.vendor_id
                && seen.product_id == pad.product_id
        }) {
            seen.connected |= pad.connected;
        } else {
            kept.push(pad);
        }
    }
    *pads = kept;
}

/// `0x054C`, `0x054c  (Sony Corporation)`, or bare hex, to the id.
#[must_use]
pub fn parse_hex_id(text: &str) -> Option<u16> {
    let token = text.split_whitespace().next()?;
    let digits = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))
        .unwrap_or(token);
    u16::from_str_radix(digits, 16).ok()
}

fn hex_id(id: u16) -> String {
    format!("{id:#06x}")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, PoisonError};

    /// The program [`gather`] runs in place of [`SYSTEM_PROFILER`] while a
    /// test holds one installed; see [`install_profiler`].
    static PROFILER_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);
    /// Held for as long as a [`ProfilerGuard`] lives, so two tests never
    /// share the slot. Separate from the slot's own lock, which
    /// [`profiler_override`] takes on the installing test's thread.
    static INSTALLED: Mutex<()> = Mutex::new(());

    pub(crate) fn profiler_override() -> Option<PathBuf> {
        PROFILER_OVERRIDE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Makes `program` the reporter until the guard drops. One test at a
    /// time: the guard holds [`INSTALLED`], and a second caller waits.
    pub(crate) fn install_profiler(program: &Path) -> ProfilerGuard {
        let installed = INSTALLED.lock().unwrap_or_else(PoisonError::into_inner);
        *PROFILER_OVERRIDE
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(program.to_path_buf());
        ProfilerGuard {
            _installed: installed,
        }
    }

    pub(crate) struct ProfilerGuard {
        _installed: MutexGuard<'static, ()>,
    }

    impl Drop for ProfilerGuard {
        fn drop(&mut self) {
            *PROFILER_OVERRIDE
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = None;
        }
    }

    const BLUETOOTH_CONNECTED: &str =
        include_str!("../../tests/fixtures/system_profiler/bluetooth-dualsense-connected.json");
    const BLUETOOTH_NOT_CONNECTED: &str =
        include_str!("../../tests/fixtures/system_profiler/bluetooth-dualsense-not-connected.json");
    const USB_PADS: &str =
        include_str!("../../tests/fixtures/system_profiler/usb-dualsense-and-xbox.json");
    const USB_EMPTY: &str = include_str!("../../tests/fixtures/system_profiler/usb-empty.json");

    fn pad(kind: ControllerKind, connected: bool) -> ControllerFact {
        ControllerFact {
            name: kind.label().to_owned(),
            transport: Transport::Bluetooth,
            vendor_id: "0x0000".to_owned(),
            product_id: "0x0000".to_owned(),
            connected,
            kind,
        }
    }

    fn pads(pads: &[ControllerFact]) -> Inventory {
        Inventory::available(pads.to_vec())
    }

    fn bluetooth(device_connected: &Value, device_not_connected: &Value) -> String {
        serde_json::json!({"SPBluetoothDataType": [{
            "device_connected": device_connected,
            "device_not_connected": device_not_connected,
        }]})
        .to_string()
    }

    #[test]
    fn bluetooth_report_lists_the_connected_pad_and_nothing_else() {
        let pads = parse_bluetooth(BLUETOOTH_CONNECTED).unwrap();
        assert_eq!(
            pads,
            [ControllerFact {
                name: "DualSense Wireless Controller".to_owned(),
                transport: Transport::Bluetooth,
                vendor_id: "0x054c".to_owned(),
                product_id: "0x0ce6".to_owned(),
                connected: true,
                kind: ControllerKind::Dualsense,
            }]
        );
    }

    #[test]
    fn bluetooth_report_lists_a_paired_pad_that_is_away_as_not_connected() {
        let pads = parse_bluetooth(BLUETOOTH_NOT_CONNECTED).unwrap();
        assert_eq!(pads.len(), 1);
        assert_eq!(pads[0].kind, ControllerKind::Dualsense);
        assert!(!pads[0].connected);
        assert_eq!(
            HidrawDecision::from_controllers(&pads).mode,
            HidrawMode::Enabled
        );
    }

    #[test]
    fn usb_report_lists_pads_under_the_bus_and_under_a_hub() {
        let pads = parse_usb(USB_PADS).unwrap();
        assert_eq!(
            pads,
            [
                ControllerFact {
                    name: "DualSense Wireless Controller".to_owned(),
                    transport: Transport::Usb,
                    vendor_id: "0x054c".to_owned(),
                    product_id: "0x0ce6".to_owned(),
                    connected: true,
                    kind: ControllerKind::Dualsense,
                },
                ControllerFact {
                    name: "Xbox Wireless Controller".to_owned(),
                    transport: Transport::Usb,
                    vendor_id: "0x045e".to_owned(),
                    product_id: "0x0b12".to_owned(),
                    connected: true,
                    kind: ControllerKind::Xbox,
                },
            ]
        );
    }

    #[test]
    fn an_empty_report_lists_no_pads_only_with_its_array_present() {
        // The U18a capture of a host with nothing plugged in.
        assert_eq!(parse_usb(USB_EMPTY).unwrap(), []);
        assert_eq!(
            parse_bluetooth(r#"{"SPBluetoothDataType": []}"#).unwrap(),
            []
        );
        // What `system_profiler -timeout` prints when a reporter gave up
        // (measured: `{\n\n}`, 4 bytes, exit 0), and its null twin.
        for text in ["{}", "{\n\n}", r#"{"SPBluetoothDataType": null}"#] {
            let error = parse_bluetooth(text).unwrap_err();
            assert!(matches!(error, ReportError::NoReport), "{text}: {error:?}");
            assert_eq!(error.to_string(), "returned no report");
        }
        for text in [
            "{}",
            r#"{"SPUSBDataType": null}"#,
            r#"{"SPUSBDataType_v2": []}"#,
        ] {
            assert!(
                matches!(parse_usb(text), Err(ReportError::NoReport)),
                "{text}"
            );
        }
    }

    #[test]
    fn malformed_reports_do_not_parse() {
        let error = parse_usb(r#"{"SPUSBDataType": {}}"#).unwrap_err();
        assert_eq!(
            error.to_string(),
            "does not parse: SPUSBDataType is an object, not an array"
        );
        let error = parse_bluetooth("[]").unwrap_err();
        assert_eq!(
            error.to_string(),
            "does not parse: expected a report object, found an array"
        );
        assert!(parse_usb("not json").is_err());
        assert!(parse_bluetooth("\"text\"").is_err());
        assert!(parse_usb("[1]").is_err());
        assert!(parse_bluetooth("null").is_err());
    }

    #[test]
    fn a_hub_whose_name_says_controller_is_not_a_pad_and_two_identical_pads_are_two() {
        let dualsense = serde_json::json!({
            "_name": "DualSense Wireless Controller",
            "vendor_id": "0x054c  (Sony Corporation)",
            "product_id": "0x0ce6",
            "location_id": "0x01100000",
        });
        let mut twin = dualsense.clone();
        twin["location_id"] = serde_json::json!("0x01200000");
        let text = serde_json::json!({"SPUSBDataType": [{
            "_name": "USB31Bus",
            "_items": [{
                "_name": "USB 3.0 Hub Controller",
                "vendor_id": "0x2109",
                "product_id": "0x0817",
                "_items": [dualsense, twin],
            }],
        }]})
        .to_string();
        let pads = parse_usb(&text).unwrap();
        assert_eq!(pads.len(), 2, "{pads:?}");
        assert!(pads.iter().all(|pad| pad.kind == ControllerKind::Dualsense));
    }

    #[test]
    fn one_odd_device_does_not_empty_the_bluetooth_report() {
        // A numeric vendor id is still an id; a string where the device's
        // fields should be is a device with no fields, and it names no pad.
        let text = bluetooth(
            &serde_json::json!([
                {"DualSense Wireless Controller": {
                    "device_minorType": "Gamepad",
                    "device_vendorID": 1356,
                    "device_productID": "0x0CE6",
                }},
                {"Example Headphones": "odd"},
                {"Example Mouse": {"device_minorType": ["Mouse"], "device_vendorID": true}},
            ]),
            &serde_json::json!([]),
        );
        let pads = parse_bluetooth(&text).unwrap();
        assert_eq!(pads.len(), 1, "{pads:?}");
        assert_eq!(pads[0].kind, ControllerKind::Dualsense);
        assert_eq!(pads[0].vendor_id, "0x054c");
        assert!(pads[0].connected);
    }

    #[test]
    fn a_bluetooth_xbox_pad_without_a_minor_type_is_kept_by_its_name_and_ids() {
        let text = bluetooth(
            &serde_json::json!([{"Xbox Wireless Controller": {
                "device_vendorID": "0x045E",
                "device_productID": "0x0B13",
            }}]),
            &serde_json::json!([]),
        );
        let pads = parse_bluetooth(&text).unwrap();
        assert_eq!(pads.len(), 1, "{pads:?}");
        assert_eq!(pads[0].kind, ControllerKind::Xbox);
        assert_eq!(pads[0].product_id, "0x0b13");
    }

    #[test]
    fn a_usb_pad_without_a_product_id_is_kept_as_other_with_the_id_blank() {
        let text = serde_json::json!({"SPUSBDataType": [{
            "_name": "USB31Bus",
            "_items": [{"_name": "Example Gamepad", "vendor_id": "0x1234  (Example)"}]
        }]})
        .to_string();
        let pads = parse_usb(&text).unwrap();
        assert_eq!(
            pads,
            [ControllerFact {
                name: "Example Gamepad".to_owned(),
                transport: Transport::Usb,
                vendor_id: "0x1234".to_owned(),
                product_id: String::new(),
                connected: true,
                kind: ControllerKind::Other,
            }]
        );
    }

    #[test]
    fn a_pad_under_both_bluetooth_lists_is_one_connected_pad() {
        let device = serde_json::json!({"DualSense Wireless Controller": {
            "device_minorType": "Gamepad",
            "device_vendorID": "0x054C",
            "device_productID": "0x0CE6",
        }});
        let text = bluetooth(
            &serde_json::json!([device.clone()]),
            &serde_json::json!([device.clone()]),
        );
        let pads = parse_bluetooth(&text).unwrap();
        assert_eq!(pads.len(), 1, "{pads:?}");
        assert!(pads[0].connected);
        let text = bluetooth(&serde_json::json!([]), &serde_json::json!([device]));
        assert!(!parse_bluetooth(&text).unwrap()[0].connected);
    }

    #[test]
    fn ids_parse_with_and_without_the_vendor_name() {
        assert_eq!(parse_hex_id("0x054C"), Some(0x054c));
        assert_eq!(parse_hex_id("0x054c  (Sony Corporation)"), Some(0x054c));
        assert_eq!(parse_hex_id("054c"), Some(0x054c));
        assert_eq!(parse_hex_id(""), None);
        assert_eq!(parse_hex_id("0x12345"), None);
    }

    #[test]
    fn kinds_follow_vendor_and_product() {
        assert_eq!(
            ControllerKind::classify(0x054c, 0x05c4),
            ControllerKind::Dualshock4
        );
        assert_eq!(
            ControllerKind::classify(0x054c, 0x09cc),
            ControllerKind::Dualshock4
        );
        assert_eq!(
            ControllerKind::classify(0x054c, 0x0ba0),
            ControllerKind::Dualshock4
        );
        assert_eq!(
            ControllerKind::classify(0x054c, 0x0ce6),
            ControllerKind::Dualsense
        );
        assert_eq!(
            ControllerKind::classify(0x054c, 0x0df2),
            ControllerKind::Dualsense
        );
        assert_eq!(
            ControllerKind::classify(0x054c, 0x0001),
            ControllerKind::Other
        );
        assert_eq!(
            ControllerKind::classify(0x045e, 0x0b12),
            ControllerKind::Xbox
        );
        assert_eq!(
            ControllerKind::classify(0x045e, 0x0001),
            ControllerKind::Xbox
        );
        assert_eq!(
            ControllerKind::classify(0x057e, 0x2009),
            ControllerKind::Other
        );
    }

    #[test]
    fn the_configuration_wins_over_the_inventory() {
        let dualsense = pads(&[pad(ControllerKind::Dualsense, true)]);
        let decision = HidrawDecision::decide(Some(true), &dualsense, None);
        assert_eq!(decision.mode, HidrawMode::Enabled);
        assert_eq!(decision.source, HidrawSource::Config);
        assert_eq!(decision.to_string(), "hidraw enabled (config)");
        let decision = HidrawDecision::decide(Some(false), &Inventory::default(), None);
        assert_eq!(decision.mode, HidrawMode::Disabled);
        assert_eq!(decision.to_string(), "hidraw disabled (config)");
        let unavailable = Inventory::unavailable("SPUSBDataType timed out");
        let decision = HidrawDecision::decide(Some(true), &unavailable, Some(HidrawMode::Disabled));
        assert_eq!(decision.to_string(), "hidraw enabled (config)");
    }

    #[test]
    fn unset_configuration_follows_the_connected_sony_pads() {
        let none = HidrawDecision::decide(None, &Inventory::default(), None);
        assert_eq!(none.mode, HidrawMode::Enabled);
        assert_eq!(none.source, HidrawSource::Controllers);
        assert_eq!(none.to_string(), "hidraw enabled (no Sony pad connected)");

        let xbox = HidrawDecision::decide(None, &pads(&[pad(ControllerKind::Xbox, true)]), None);
        assert_eq!(xbox.to_string(), "hidraw enabled (no Sony pad connected)");

        let away =
            HidrawDecision::decide(None, &pads(&[pad(ControllerKind::Dualsense, false)]), None);
        assert_eq!(away.to_string(), "hidraw enabled (no Sony pad connected)");

        let ds4 =
            HidrawDecision::decide(None, &pads(&[pad(ControllerKind::Dualshock4, true)]), None);
        assert_eq!(ds4.mode, HidrawMode::Disabled);
        assert_eq!(ds4.to_string(), "hidraw disabled (DualShock 4 connected)");

        let both = HidrawDecision::decide(
            None,
            &pads(&[
                pad(ControllerKind::Dualshock4, true),
                pad(ControllerKind::Dualsense, true),
            ]),
            None,
        );
        assert_eq!(both.to_string(), "hidraw disabled (DualSense connected)");
    }

    #[test]
    fn an_unavailable_inventory_keeps_the_prefix_and_disables_a_new_one() {
        let unavailable = Inventory::unavailable("SPBluetoothDataType timed out");
        let fresh = HidrawDecision::decide(None, &unavailable, None);
        assert_eq!(fresh.mode, HidrawMode::Disabled);
        assert_eq!(fresh.source, HidrawSource::Controllers);
        assert_eq!(
            fresh.to_string(),
            "hidraw disabled (controller inventory unavailable (SPBluetoothDataType timed out))"
        );
        let kept = HidrawDecision::decide(None, &unavailable, Some(HidrawMode::Enabled));
        assert_eq!(kept.mode, HidrawMode::Enabled);
        assert_eq!(
            kept.to_string(),
            "hidraw enabled (controller inventory unavailable (SPBluetoothDataType timed out))"
        );
        let kept = HidrawDecision::decide(None, &unavailable, Some(HidrawMode::Disabled));
        assert_eq!(kept.mode, HidrawMode::Disabled);
        // An Xbox pad the other report did list does not make the read
        // trustworthy: the report that failed could hide a Sony pad.
        let partial = Inventory {
            pads: vec![pad(ControllerKind::Xbox, true)],
            unavailable: Some("SPBluetoothDataType exited with exit status: 1".to_owned()),
        };
        assert!(!partial.decides_hidraw());
        assert_eq!(
            HidrawDecision::decide(None, &partial, Some(HidrawMode::Disabled)).mode,
            HidrawMode::Disabled
        );
        let kept = HidrawDecision::decide(None, &partial, Some(HidrawMode::Enabled));
        assert_eq!(
            kept.to_string(),
            "hidraw enabled (controller inventory unavailable (SPBluetoothDataType exited with exit status: 1))"
        );
        assert_eq!(
            HidrawDecision::decide(None, &partial, None).to_string(),
            "hidraw disabled (controller inventory unavailable (SPBluetoothDataType exited with exit status: 1))"
        );
    }

    #[test]
    fn a_connected_sony_pad_decides_even_when_the_other_report_is_unavailable() {
        // The report that failed can only hide pads; it never undoes the
        // DualSense the USB report did list.
        let partial = Inventory {
            pads: vec![pad(ControllerKind::Dualsense, true)],
            unavailable: Some("SPBluetoothDataType exited with exit status: 1".to_owned()),
        };
        assert!(partial.decides_hidraw());
        for current in [None, Some(HidrawMode::Enabled), Some(HidrawMode::Disabled)] {
            let decision = HidrawDecision::decide(None, &partial, current);
            assert_eq!(decision.mode, HidrawMode::Disabled, "{current:?}");
            assert_eq!(decision.source, HidrawSource::Controllers);
            assert_eq!(
                decision.to_string(),
                "hidraw disabled (DualSense connected; SPBluetoothDataType exited with exit status: 1)"
            );
        }
        let ds4 = Inventory {
            pads: vec![pad(ControllerKind::Dualshock4, true)],
            unavailable: Some("SPUSBDataType timed out".to_owned()),
        };
        assert_eq!(
            HidrawDecision::decide(None, &ds4, Some(HidrawMode::Enabled)).to_string(),
            "hidraw disabled (DualShock 4 connected; SPUSBDataType timed out)"
        );
        // An away Sony pad does not count, so the prefix's mode holds.
        let away = Inventory {
            pads: vec![pad(ControllerKind::Dualsense, false)],
            unavailable: Some("SPUSBDataType timed out".to_owned()),
        };
        assert!(!away.decides_hidraw());
        assert_eq!(
            HidrawDecision::decide(None, &away, Some(HidrawMode::Enabled)).mode,
            HidrawMode::Enabled
        );
        // The configuration still wins.
        assert_eq!(
            HidrawDecision::decide(Some(true), &partial, None).to_string(),
            "hidraw enabled (config)"
        );
    }

    /// A `system_profiler` stand-in: `SPUSBDataType` answers with a fixture,
    /// `SPBluetoothDataType` as `bluetooth` says.
    fn shim(dir: &Path, usb: &str, bluetooth: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let usb_path = dir.join("usb.json");
        fs::write(&usb_path, usb).unwrap();
        let script = dir.join("system_profiler");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  SPUSBDataType) cat '{}' ;;\n  *) {bluetooth} ;;\nesac\n",
                usb_path.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// The bound the shim tests hand `gather_from`: the report that must
    /// succeed (a `cat` through `sh`) has three seconds under a loaded
    /// host, and the stalled one costs the test those three seconds.
    const SHIM_TIMEOUT: Duration = Duration::from_secs(2);
    /// Well past the 3 s deadline, well short of any shim's sleep.
    const SHIM_BOUND: Duration = Duration::from_secs(10);

    /// Whether any process of the group `pgid` is still alive, polled for
    /// up to two seconds so a kill that has landed but not yet been reaped
    /// does not read as a survivor.
    fn group_alive(pgid: &str) -> bool {
        let started = Instant::now();
        loop {
            let listed = Command::new("/usr/bin/pgrep")
                .args(["-g", pgid])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if !listed {
                return false;
            }
            if started.elapsed() > Duration::from_secs(2) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn pid_alive(pid: &str) -> bool {
        Command::new("/bin/kill")
            .args(["-0", pid])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn pid_in(dir: &Path, name: &str) -> String {
        fs::read_to_string(dir.join(name))
            .unwrap()
            .trim()
            .to_owned()
    }

    #[test]
    fn a_report_that_times_out_is_unavailable_and_the_other_pads_still_list() {
        let dir = tempfile::tempdir().unwrap();
        // The sleeper is the tool itself.
        let tool = shim(dir.path(), USB_PADS, "exec sleep 30");
        let started = Instant::now();
        let inventory = gather_from(&tool, SHIM_TIMEOUT);
        let elapsed = started.elapsed();
        assert!(elapsed < SHIM_BOUND, "the wait is bounded: {elapsed:?}");
        assert_eq!(
            inventory.unavailable.as_deref(),
            Some("SPBluetoothDataType timed out")
        );
        assert_eq!(inventory.pads.len(), 2, "{inventory:?}");
        assert_eq!(
            HidrawDecision::from_inventory(&inventory, None).to_string(),
            "hidraw disabled (DualSense connected; SPBluetoothDataType timed out)"
        );
        let xbox_only: Vec<_> = inventory
            .pads
            .into_iter()
            .filter(|pad| pad.kind == ControllerKind::Xbox)
            .collect();
        let inventory = Inventory {
            pads: xbox_only,
            unavailable: Some("SPBluetoothDataType timed out".to_owned()),
        };
        assert_eq!(
            HidrawDecision::from_inventory(&inventory, None).to_string(),
            "hidraw disabled (controller inventory unavailable (SPBluetoothDataType timed out))"
        );
    }

    #[test]
    fn a_shell_in_front_of_the_sleeper_leaves_no_survivor() {
        let dir = tempfile::tempdir().unwrap();
        // No `exec`: the sleeper is the shell's child, in the tool's group.
        let snippet = format!(
            "echo $$ > '{}'; sleep 31; echo '{{}}'",
            dir.path().join("pgid").display()
        );
        let tool = shim(dir.path(), USB_EMPTY, &snippet);
        let started = Instant::now();
        let inventory = gather_from(&tool, SHIM_TIMEOUT);
        let elapsed = started.elapsed();
        assert!(elapsed < SHIM_BOUND, "the wait is bounded: {elapsed:?}");
        assert_eq!(
            inventory.unavailable.as_deref(),
            Some("SPBluetoothDataType timed out")
        );
        let pgid = pid_in(dir.path(), "pgid");
        assert!(!group_alive(&pgid), "the group {pgid} survived the kill");
    }

    #[test]
    fn a_helper_that_holds_stdout_after_the_tool_exits_does_not_hold_the_read() {
        let dir = tempfile::tempdir().unwrap();
        // The tool exits 0 at once; its helper keeps the pipe's write end.
        let snippet = format!(
            "echo $$ > '{}'; sleep 12 & echo $! > '{}'; echo '{{\"SPBluetoothDataType\": []}}'; exit 0",
            dir.path().join("pgid").display(),
            dir.path().join("helper").display()
        );
        let tool = shim(dir.path(), USB_EMPTY, &snippet);
        let started = Instant::now();
        let inventory = gather_from(&tool, SHIM_TIMEOUT);
        let elapsed = started.elapsed();
        assert!(elapsed < SHIM_BOUND, "the wait is bounded: {elapsed:?}");
        assert_eq!(
            inventory.unavailable.as_deref(),
            Some("SPBluetoothDataType timed out"),
            "a report whose pipe never closed is not trusted"
        );
        let pgid = pid_in(dir.path(), "pgid");
        assert!(!group_alive(&pgid), "the group {pgid} survived the kill");
        let helper = pid_in(dir.path(), "helper");
        assert!(!pid_alive(&helper), "the helper {helper} survived the kill");
    }

    #[test]
    fn two_stalled_reports_cost_one_bound() {
        let dir = tempfile::tempdir().unwrap();
        // Both reports stall: the shim is replaced by a script that sleeps
        // whatever it is asked (the mode set by `shim` stays).
        let tool = shim(dir.path(), USB_EMPTY, "exec sleep 30");
        fs::write(&tool, "#!/bin/sh\nexec sleep 30\n").unwrap();
        let started = Instant::now();
        let inventory = gather_from(&tool, SHIM_TIMEOUT);
        let elapsed = started.elapsed();
        assert!(
            elapsed < SHIM_TIMEOUT * 2 + SHIM_TIMEOUT / 2,
            "both reports share one bound: {elapsed:?}"
        );
        assert_eq!(
            inventory.unavailable.as_deref(),
            Some("SPUSBDataType timed out; SPBluetoothDataType timed out")
        );
    }

    #[test]
    fn a_report_that_fails_or_does_not_parse_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let tool = shim(dir.path(), USB_EMPTY, "exit 1");
        let inventory = gather_from(&tool, Duration::from_secs(5));
        assert_eq!(
            inventory.unavailable.as_deref(),
            Some("SPBluetoothDataType exited with exit status: 1")
        );
        assert_eq!(inventory.pads, []);

        let tool = shim(
            dir.path(),
            "not json",
            r#"echo '{"SPBluetoothDataType": []}'"#,
        );
        let inventory = gather_from(&tool, Duration::from_secs(5));
        let why = inventory.unavailable.as_deref().unwrap();
        assert!(why.starts_with("SPUSBDataType does not parse: "), "{why}");
        assert!(!why.contains("SPBluetoothDataType"), "{why}");

        let missing = dir.path().join("no-such-tool");
        let inventory = gather_from(&missing, Duration::from_secs(5));
        let why = inventory.unavailable.as_deref().unwrap();
        assert!(why.starts_with("SPUSBDataType cannot run "), "{why}");
        assert!(why.contains("; SPBluetoothDataType cannot run "), "{why}");
    }

    #[test]
    fn a_shim_that_answers_both_reports_is_a_complete_inventory() {
        let dir = tempfile::tempdir().unwrap();
        let tool = shim(
            dir.path(),
            USB_EMPTY,
            r#"echo '{"SPBluetoothDataType": []}'"#,
        );
        let inventory = gather_from(&tool, Duration::from_secs(5));
        assert_eq!(inventory, Inventory::default());
        assert!(inventory.is_available());
    }

    #[test]
    fn the_tools_own_partial_report_is_unavailable_not_no_pads() {
        let dir = tempfile::tempdir().unwrap();
        // `system_profiler -timeout` gives up with exit 0 and `{}`.
        let tool = shim(dir.path(), USB_PADS, "printf '{\\n\\n}'");
        let inventory = gather_from(&tool, Duration::from_secs(5));
        assert_eq!(
            inventory.unavailable.as_deref(),
            Some("SPBluetoothDataType returned no report")
        );
        assert_eq!(inventory.pads.len(), 2, "{inventory:?}");
        assert!(
            inventory
                .pads
                .iter()
                .all(|pad| pad.transport == Transport::Usb),
            "{inventory:?}"
        );
    }

    #[test]
    fn the_tool_is_told_twice_the_bound_so_its_own_give_up_never_comes_first() {
        let dir = tempfile::tempdir().unwrap();
        let args = dir.path().join("args");
        let tool = shim(
            dir.path(),
            USB_EMPTY,
            &format!(
                "echo \"$@\" > '{}'; echo '{{\"SPBluetoothDataType\": []}}'",
                args.display()
            ),
        );
        let _ = gather_from(&tool, Duration::from_secs(5));
        assert_eq!(
            fs::read_to_string(&args).unwrap().trim(),
            "SPBluetoothDataType -json -timeout 10"
        );
    }

    #[test]
    fn the_environment_knob_stands_in_for_the_host() {
        assert_eq!(
            inventory_from_knob(OsStr::new("none")),
            Inventory::default()
        );
        let unavailable = inventory_from_knob(OsStr::new("unavailable"));
        assert_eq!(
            unavailable.unavailable.as_deref(),
            Some("APPLEJACK_CONTROLLERS=unavailable")
        );
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pads.json");
        fs::write(
            &file,
            serde_json::to_string(&[pad(ControllerKind::Dualsense, true)]).unwrap(),
        )
        .unwrap();
        let fixed = inventory_from_knob(file.as_os_str());
        assert_eq!(fixed, pads(&[pad(ControllerKind::Dualsense, true)]));
        // `doctor --controllers --json`'s own shape round-trips, the
        // unavailable member included.
        let object = dir.path().join("inventory.json");
        let partial = Inventory {
            pads: vec![pad(ControllerKind::Xbox, true)],
            unavailable: Some("SPBluetoothDataType timed out".to_owned()),
        };
        fs::write(&object, serde_json::to_string(&partial).unwrap()).unwrap();
        assert_eq!(inventory_from_knob(object.as_os_str()), partial);
        fs::write(&object, serde_json::to_string(&fixed).unwrap()).unwrap();
        assert_eq!(inventory_from_knob(object.as_os_str()), fixed);
        let missing = inventory_from_knob(dir.path().join("missing.json").as_os_str());
        assert!(
            missing
                .unavailable
                .as_deref()
                .is_some_and(|why| why.starts_with("APPLEJACK_CONTROLLERS ")),
            "{missing:?}"
        );
    }

    #[test]
    fn facts_serialize_in_snake_case() {
        let json = serde_json::to_value(pad(ControllerKind::Dualshock4, true)).unwrap();
        assert_eq!(json["kind"], "dualshock4");
        assert_eq!(json["transport"], "bluetooth");
        let json = serde_json::to_value(HidrawSource::Controllers).unwrap();
        assert_eq!(json, "controllers");
        let json = serde_json::to_value(Inventory::default()).unwrap();
        assert_eq!(json, serde_json::json!({"controllers": []}));
        let json = serde_json::to_value(Inventory::unavailable("why")).unwrap();
        assert_eq!(json["controllers_unavailable"], "why");
    }
}
