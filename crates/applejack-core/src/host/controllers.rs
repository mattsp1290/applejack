//! The controller inventory behind `applejack doctor --controllers` and the
//! hidraw default: pure parsers over `system_profiler SPUSBDataType -json`
//! and `SPBluetoothDataType -json`, and the rule that turns the inventory
//! into a [`HidrawDecision`] when the game configuration leaves
//! `hidraw_enabled` unset.

use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
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

/// The bound passed to `system_profiler -timeout`, whose own default is
/// 180 s. A warm report takes 0.1 to 0.3 s and a cold one about 1.3 s on
/// the measured host, so five seconds is several times the slowest
/// observed report and still shorter than a stall a user reads as a hang.
pub const PROFILER_TIMEOUT: Duration = Duration::from_secs(5);

/// The environment knob that replaces the host's inventory: `none` for no
/// pads, `unavailable` for a report that could not be read, or the path of
/// a JSON array of [`ControllerFact`]. Test suites set it so the binary
/// never runs `system_profiler`.
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
    /// enabled with none. An inventory that could not be read keeps
    /// `current`, the prefix's mode, and gives a new prefix `Disabled`.
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
        match &inventory.unavailable {
            Some(why) => Self {
                mode: current.unwrap_or(HidrawMode::Disabled),
                source: HidrawSource::Controllers,
                reason: format!("controller inventory unavailable ({why})"),
            },
            None => Self::from_controllers(&inventory.pads),
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
    let pads = fs::read_to_string(path)
        .map_err(|error| error.to_string())
        .and_then(|text| {
            serde_json::from_str::<Vec<ControllerFact>>(&text).map_err(|error| error.to_string())
        });
    match pads {
        Ok(pads) => Inventory::available(pads),
        Err(why) => Inventory::unavailable(format!("{CONTROLLERS_ENV} {}: {why}", path.display())),
    }
}

/// Runs both `system_profiler` reports, each bounded by
/// [`PROFILER_TIMEOUT`], and lists the pads in them, USB first. A report
/// that fails to run, times out, or does not parse makes the inventory
/// unavailable with the reason; the pads of the other report are still
/// listed.
#[must_use]
pub fn gather() -> Inventory {
    gather_from(Path::new(SYSTEM_PROFILER), PROFILER_TIMEOUT)
}

type Parser = fn(&str) -> Result<Vec<ControllerFact>, serde_json::Error>;

/// [`gather`] with the reporter and its bound chosen; tests point it at a
/// shim.
#[must_use]
pub fn gather_from(program: &Path, timeout: Duration) -> Inventory {
    let mut inventory = Inventory::default();
    let mut failures = Vec::new();
    let reports: [(&str, Parser); 2] = [
        ("SPUSBDataType", parse_usb),
        ("SPBluetoothDataType", parse_bluetooth),
    ];
    for (data_type, parse) in reports {
        let pads = report(program, data_type, timeout)
            .and_then(|text| parse(&text).map_err(|error| format!("does not parse: {error}")));
        match pads {
            Ok(pads) => inventory.pads.extend(pads),
            Err(why) => failures.push(format!("{data_type} {why}")),
        }
    }
    dedupe(&mut inventory.pads);
    if !failures.is_empty() {
        inventory.unavailable = Some(failures.join("; "));
    }
    inventory
}

/// One report's JSON. The tool is told the bound and the wait allows it
/// half as much again to print; past that it is killed and the report is
/// a failure, never a hang.
fn report(program: &Path, data_type: &str, timeout: Duration) -> Result<String, String> {
    let seconds = timeout.as_secs().max(1).to_string();
    let mut child = Command::new(program)
        .args([data_type, "-json", "-timeout", &seconds])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot run {}: {error}", program.display()))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "no stdout pipe".to_owned())?;
    // Drained on its own thread so a long report cannot fill the pipe and
    // wait on us while we wait on it.
    let reader = std::thread::spawn(move || {
        let mut text = Vec::new();
        stdout.read_to_end(&mut text).map(|_| text)
    });
    let deadline = Instant::now() + timeout + timeout / 2;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("timed out".to_owned());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(format!("cannot wait for the report: {error}")),
        }
    };
    let text = reader
        .join()
        .map_err(|_| "the reader thread panicked".to_owned())?
        .map_err(|error| format!("cannot read the report: {error}"))?;
    if !status.success() {
        return Err(format!("exited with {status}"));
    }
    Ok(String::from_utf8_lossy(&text).into_owned())
}

/// The report as an object. `[]` is what an empty report would be and
/// lists nothing; any other non-object is not a report.
fn report_object(text: &str) -> Result<Option<serde_json::Map<String, Value>>, serde_json::Error> {
    match serde_json::from_str::<Value>(text)? {
        Value::Object(map) => Ok(Some(map)),
        Value::Array(items) if items.is_empty() => Ok(None),
        other => Err(serde::de::Error::custom(format!(
            "expected a report object, found {}",
            json_kind(&other)
        ))),
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
/// is told from an Xbox pad by its name. Each device is read leniently: an
/// odd or missing field drops or blanks that field, never the report.
///
/// # Errors
///
/// The text is not JSON, or not a report object.
pub fn parse_usb(text: &str) -> Result<Vec<ControllerFact>, serde_json::Error> {
    let Some(report) = report_object(text)? else {
        return Ok(Vec::new());
    };
    let mut pads = Vec::new();
    for bus in array_of(report.get("SPUSBDataType")) {
        collect_usb(bus, &mut pads);
    }
    dedupe(&mut pads);
    Ok(pads)
}

fn collect_usb(item: &Value, pads: &mut Vec<ControllerFact>) {
    let Some(item) = item.as_object() else {
        return;
    };
    let vendor = item.get("vendor_id").and_then(id_of);
    let product = item.get("product_id").and_then(id_of);
    pads.extend(pad_of(
        text_of(item.get("_name")),
        Transport::Usb,
        vendor,
        product,
        false,
        true,
    ));
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
/// The text is not JSON, or not a report object.
pub fn parse_bluetooth(text: &str) -> Result<Vec<ControllerFact>, serde_json::Error> {
    let Some(report) = report_object(text)? else {
        return Ok(Vec::new());
    };
    let mut pads = Vec::new();
    for entry in array_of(report.get("SPBluetoothDataType")) {
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

/// One row per pad: the same name and ids over the same transport, listed
/// twice, is one pad, connected if either listing says so.
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
mod tests {
    use super::*;

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
    fn empty_and_malformed_reports() {
        assert_eq!(parse_usb(USB_EMPTY).unwrap(), []);
        assert_eq!(parse_bluetooth("{}").unwrap(), []);
        // `[]` is the shape an empty report would take, so it lists nothing
        // rather than failing.
        assert_eq!(parse_bluetooth("[]").unwrap(), []);
        assert_eq!(parse_usb("[]").unwrap(), []);
        assert!(parse_usb("not json").is_err());
        assert!(parse_bluetooth("\"text\"").is_err());
        assert!(parse_usb("[1]").is_err());
        assert!(parse_bluetooth("null").is_err());
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
        // Pads the other report did list do not make the read trustworthy.
        let partial = Inventory {
            pads: vec![pad(ControllerKind::Xbox, true)],
            unavailable: Some("SPBluetoothDataType exited with exit status: 1".to_owned()),
        };
        assert_eq!(
            HidrawDecision::decide(None, &partial, Some(HidrawMode::Disabled)).mode,
            HidrawMode::Disabled
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

    #[test]
    fn a_report_that_times_out_is_unavailable_and_the_other_pads_still_list() {
        let dir = tempfile::tempdir().unwrap();
        // `exec` so the kill lands on the sleeper, not a shell in front of it.
        let tool = shim(dir.path(), USB_PADS, "exec sleep 30");
        let started = Instant::now();
        let inventory = gather_from(&tool, Duration::from_millis(400));
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "the wait is bounded: {elapsed:?}"
        );
        assert_eq!(
            inventory.unavailable.as_deref(),
            Some("SPBluetoothDataType timed out")
        );
        assert_eq!(inventory.pads.len(), 2, "{inventory:?}");
        assert_eq!(
            HidrawDecision::from_inventory(&inventory, None).to_string(),
            "hidraw disabled (controller inventory unavailable (SPBluetoothDataType timed out))"
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

        let tool = shim(dir.path(), "not json", "echo '[]'");
        let inventory = gather_from(&tool, Duration::from_secs(5));
        let why = inventory.unavailable.as_deref().unwrap();
        assert!(why.starts_with("SPUSBDataType does not parse: "), "{why}");

        let missing = dir.path().join("no-such-tool");
        let inventory = gather_from(&missing, Duration::from_secs(5));
        let why = inventory.unavailable.as_deref().unwrap();
        assert!(why.starts_with("SPUSBDataType cannot run "), "{why}");
        assert!(why.contains("; SPBluetoothDataType cannot run "), "{why}");
    }

    #[test]
    fn a_shim_that_answers_both_reports_is_a_complete_inventory() {
        let dir = tempfile::tempdir().unwrap();
        let tool = shim(dir.path(), USB_EMPTY, "echo '{}'");
        let inventory = gather_from(&tool, Duration::from_secs(5));
        assert_eq!(inventory, Inventory::default());
        assert!(inventory.is_available());
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
