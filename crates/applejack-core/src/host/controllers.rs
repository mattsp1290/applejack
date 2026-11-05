//! The controller inventory behind `applejack doctor --controllers` and the
//! hidraw default: pure parsers over `system_profiler SPUSBDataType -json`
//! and `SPBluetoothDataType -json`, and the rule that turns the inventory
//! into a [`HidrawDecision`] when the game configuration leaves
//! `hidraw_enabled` unset.

use std::collections::BTreeMap;
use std::fmt;
use std::process::Command;

use applejack_engine::HidrawMode;
use serde::{Deserialize, Serialize};

const SONY: u16 = 0x054c;
const MICROSOFT: u16 = 0x045e;
const DUALSHOCK_4: [u16; 2] = [0x05c4, 0x09cc];
/// `DualSense` and `DualSense` Edge.
const DUALSENSE: [u16; 2] = [0x0ce6, 0x0df2];

/// One pad `system_profiler` lists, over USB or Bluetooth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerFact {
    pub name: String,
    pub transport: Transport,
    /// Lowercase `0x` hex, four digits.
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
    /// Sony's `DualShock` 4 (`0x05c4`, `0x09cc`) and `DualSense` (`0x0ce6`,
    /// Edge `0x0df2`); every Microsoft product counts as an Xbox pad.
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

    /// The hidraw mode this pad alone, connected, would select.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HidrawDecision {
    pub mode: HidrawMode,
    pub source: HidrawSource,
    /// `config`, `DualSense connected`, `DualShock 4 connected`, or
    /// `no Sony pad connected`.
    pub reason: &'static str,
}

impl HidrawDecision {
    /// An explicit configuration always wins; otherwise the inventory
    /// decides: disabled with a connected `DualShock` 4 or `DualSense`, enabled
    /// with none.
    #[must_use]
    pub fn decide(config: Option<bool>, controllers: &[ControllerFact]) -> Self {
        match config {
            Some(enabled) => Self {
                mode: mode_of(enabled),
                source: HidrawSource::Config,
                reason: "config",
            },
            None => Self::from_controllers(controllers),
        }
    }

    /// The default with nothing configured.
    #[must_use]
    pub fn from_controllers(controllers: &[ControllerFact]) -> Self {
        let culprit = controllers
            .iter()
            .filter(|pad| pad.connected && pad.kind.hidraw_regresses())
            .map(|pad| pad.kind)
            // The DualSense is named first when both are connected.
            .max_by_key(|kind| *kind == ControllerKind::Dualsense);
        match culprit {
            Some(ControllerKind::Dualsense) => Self {
                mode: HidrawMode::Disabled,
                source: HidrawSource::Controllers,
                reason: "DualSense connected",
            },
            Some(_) => Self {
                mode: HidrawMode::Disabled,
                source: HidrawSource::Controllers,
                reason: "DualShock 4 connected",
            },
            None => Self {
                mode: HidrawMode::Enabled,
                source: HidrawSource::Controllers,
                reason: "no Sony pad connected",
            },
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

/// Runs both `system_profiler` reports and lists the pads in them, USB
/// first. A report that fails or does not parse lists nothing: the inventory
/// is a default, never a refusal.
#[must_use]
pub fn gather() -> Vec<ControllerFact> {
    let mut pads = report("SPUSBDataType")
        .and_then(|text| parse_usb(&text).ok())
        .unwrap_or_default();
    pads.extend(
        report("SPBluetoothDataType")
            .and_then(|text| parse_bluetooth(&text).ok())
            .unwrap_or_default(),
    );
    pads
}

fn report(data_type: &str) -> Option<String> {
    let output = Command::new("/usr/sbin/system_profiler")
        .args([data_type, "-json"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[derive(Deserialize)]
struct UsbReport {
    #[serde(rename = "SPUSBDataType", default)]
    buses: Vec<UsbItem>,
}

#[derive(Deserialize)]
struct UsbItem {
    #[serde(rename = "_name", default)]
    name: String,
    #[serde(default)]
    vendor_id: Option<String>,
    #[serde(default)]
    product_id: Option<String>,
    #[serde(rename = "_items", default)]
    items: Vec<UsbItem>,
}

/// The pads in `system_profiler SPUSBDataType -json`: every device under a
/// bus or hub whose vendor and product name a known pad, or whose name says
/// it is one. The USB report carries no device class, so a Microsoft mouse
/// is told from an Xbox pad by its name.
///
/// # Errors
///
/// The JSON does not have the report's shape.
pub fn parse_usb(text: &str) -> Result<Vec<ControllerFact>, serde_json::Error> {
    let report: UsbReport = serde_json::from_str(text)?;
    let mut pads = Vec::new();
    for bus in &report.buses {
        collect_usb(bus, &mut pads);
    }
    Ok(pads)
}

fn collect_usb(item: &UsbItem, pads: &mut Vec<ControllerFact>) {
    if let (Some(vendor), Some(product)) = (
        item.vendor_id.as_deref().and_then(parse_hex_id),
        item.product_id.as_deref().and_then(parse_hex_id),
    ) {
        let kind = ControllerKind::classify(vendor, product);
        if kind.hidraw_regresses() || names_a_pad(&item.name) {
            pads.push(ControllerFact {
                name: item.name.clone(),
                transport: Transport::Usb,
                vendor_id: hex_id(vendor),
                product_id: hex_id(product),
                connected: true,
                kind,
            });
        }
    }
    for child in &item.items {
        collect_usb(child, pads);
    }
}

fn names_a_pad(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    ["controller", "gamepad", "joystick"]
        .iter()
        .any(|word| name.contains(word))
}

#[derive(Deserialize)]
struct BluetoothReport {
    #[serde(rename = "SPBluetoothDataType", default)]
    entries: Vec<BluetoothEntry>,
}

/// Each device is an object with one key, the device's name.
#[derive(Deserialize)]
struct BluetoothEntry {
    #[serde(default)]
    device_connected: Vec<BTreeMap<String, BluetoothDevice>>,
    #[serde(default)]
    device_not_connected: Vec<BTreeMap<String, BluetoothDevice>>,
}

#[derive(Deserialize)]
struct BluetoothDevice {
    #[serde(rename = "device_minorType", default)]
    minor_type: Option<String>,
    #[serde(rename = "device_vendorID", default)]
    vendor_id: Option<String>,
    #[serde(rename = "device_productID", default)]
    product_id: Option<String>,
}

/// The pads in `system_profiler SPBluetoothDataType -json`: every paired
/// device with the `Gamepad` minor type, or a known pad's vendor and
/// product, connected or not.
///
/// # Errors
///
/// The JSON does not have the report's shape.
pub fn parse_bluetooth(text: &str) -> Result<Vec<ControllerFact>, serde_json::Error> {
    let report: BluetoothReport = serde_json::from_str(text)?;
    let mut pads = Vec::new();
    for entry in &report.entries {
        for (devices, connected) in [
            (&entry.device_connected, true),
            (&entry.device_not_connected, false),
        ] {
            for (name, device) in devices.iter().flatten() {
                if let Some(pad) = bluetooth_pad(name, device, connected) {
                    pads.push(pad);
                }
            }
        }
    }
    Ok(pads)
}

fn bluetooth_pad(name: &str, device: &BluetoothDevice, connected: bool) -> Option<ControllerFact> {
    let vendor = device.vendor_id.as_deref().and_then(parse_hex_id);
    let product = device.product_id.as_deref().and_then(parse_hex_id);
    let kind = match (vendor, product) {
        (Some(vendor), Some(product)) => ControllerKind::classify(vendor, product),
        _ => ControllerKind::Other,
    };
    let gamepad = device
        .minor_type
        .as_deref()
        .is_some_and(|minor| minor.eq_ignore_ascii_case("gamepad"));
    if !gamepad && !kind.hidraw_regresses() {
        return None;
    }
    Some(ControllerFact {
        name: name.to_owned(),
        transport: Transport::Bluetooth,
        vendor_id: vendor.map_or_else(|| "unknown".to_owned(), hex_id),
        product_id: product.map_or_else(|| "unknown".to_owned(), hex_id),
        connected,
        kind,
    })
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
        assert!(parse_usb("not json").is_err());
        assert!(parse_bluetooth("\"text\"").is_err());
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
        let pads = [pad(ControllerKind::Dualsense, true)];
        let decision = HidrawDecision::decide(Some(true), &pads);
        assert_eq!(decision.mode, HidrawMode::Enabled);
        assert_eq!(decision.source, HidrawSource::Config);
        assert_eq!(decision.to_string(), "hidraw enabled (config)");
        let decision = HidrawDecision::decide(Some(false), &[]);
        assert_eq!(decision.mode, HidrawMode::Disabled);
        assert_eq!(decision.to_string(), "hidraw disabled (config)");
    }

    #[test]
    fn unset_configuration_follows_the_connected_sony_pads() {
        let none = HidrawDecision::decide(None, &[]);
        assert_eq!(none.mode, HidrawMode::Enabled);
        assert_eq!(none.source, HidrawSource::Controllers);
        assert_eq!(none.to_string(), "hidraw enabled (no Sony pad connected)");

        let xbox = HidrawDecision::decide(None, &[pad(ControllerKind::Xbox, true)]);
        assert_eq!(xbox.to_string(), "hidraw enabled (no Sony pad connected)");

        let away = HidrawDecision::decide(None, &[pad(ControllerKind::Dualsense, false)]);
        assert_eq!(away.to_string(), "hidraw enabled (no Sony pad connected)");

        let ds4 = HidrawDecision::decide(None, &[pad(ControllerKind::Dualshock4, true)]);
        assert_eq!(ds4.mode, HidrawMode::Disabled);
        assert_eq!(ds4.to_string(), "hidraw disabled (DualShock 4 connected)");

        let both = HidrawDecision::decide(
            None,
            &[
                pad(ControllerKind::Dualshock4, true),
                pad(ControllerKind::Dualsense, true),
            ],
        );
        assert_eq!(both.to_string(), "hidraw disabled (DualSense connected)");
    }

    #[test]
    fn facts_serialize_in_snake_case() {
        let json = serde_json::to_value(pad(ControllerKind::Dualshock4, true)).unwrap();
        assert_eq!(json["kind"], "dualshock4");
        assert_eq!(json["transport"], "bluetooth");
        let json = serde_json::to_value(HidrawSource::Controllers).unwrap();
        assert_eq!(json, "controllers");
    }
}
