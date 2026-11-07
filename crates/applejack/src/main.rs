//! Command-line entry point for applejack.

mod cmd;
mod qr;

use applejack_core::host::{self, HidrawDecision, HostFacts, Inventory};
use applejack_engine::HidrawMode;
use clap::{Parser, Subcommand};
use std::io::{self, Write};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "applejack",
    version,
    about = "Windows Steam game runner for Apple Silicon"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install, inspect, and roll back engines
    Engine(cmd::engine::EngineArgs),
    /// Sign in to Steam and list the owned library
    Steam(cmd::steam::SteamArgs),
    /// Download a title's Windows depots, update them, or verify them
    Install(cmd::install::InstallArgs),
    /// Launch an installed title; `app run` under its plan name
    Run(cmd::app::RunArgs),
    /// Run, cancel, and register titles
    App(cmd::app::AppArgs),
    /// List past and running sessions
    Session(cmd::session::SessionArgs),
    /// Read and write per-game configuration
    Config(cmd::config::ConfigArgs),
    /// Create, snapshot, and reset Wine prefixes
    Prefix(cmd::prefix::PrefixArgs),
    /// Manage the shared Steam client prefix
    Runtime(cmd::runtime::RuntimeArgs),
    /// Serve JSON-RPC over stdio for the app shell
    Serve(cmd::serve::ServeArgs),
    /// Report host readiness facts
    Doctor {
        /// Print the facts as JSON
        #[arg(long)]
        json: bool,
        /// Print only the controller inventory, one line per pad, and the
        /// hidraw default it gives a game whose `hidraw_enabled` is unset
        /// (with --json, the `controllers` member alone)
        #[arg(long)]
        controllers: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Doctor { json, controllers } => doctor(json, controllers),
        Command::Config(args) => cmd::config::run(args),
        Command::Engine(args) => cmd::engine::run(args),
        Command::Prefix(args) => cmd::prefix::run(args),
        Command::Serve(args) => cmd::serve::run(args),
        Command::Run(args) => cmd::app::run_title(&args),
        Command::App(args) => cmd::app::run(args),
        Command::Session(args) => cmd::session::run(&args),
        Command::Steam(args) => cmd::steam::run(&args),
        Command::Install(args) => cmd::install::run(args),
        Command::Runtime(args) => cmd::runtime::run(&args),
    }
}

fn doctor(json: bool, controllers: bool) -> ExitCode {
    if controllers {
        // Only the inventory: the other probes are not asked for.
        let inventory = host::controllers::inventory();
        return if json {
            emit_json(serde_json::to_string_pretty(&inventory))
        } else {
            emit(&render_controllers(&inventory))
        };
    }
    let facts = HostFacts::gather_with_controllers();
    if json {
        emit_json(serde_json::to_string_pretty(&facts))
    } else {
        emit(&render_facts(&facts))
    }
}

fn emit_json(encoded: serde_json::Result<String>) -> ExitCode {
    match encoded {
        Ok(text) => emit(&text),
        Err(error) => {
            eprintln!("applejack doctor: cannot encode facts: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Writes `text` to stdout. A reader that closes the pipe early is not an
/// error for a report command, so `BrokenPipe` exits 0.
fn emit(text: &str) -> ExitCode {
    let mut out = io::stdout().lock();
    match writeln!(out, "{text}").and_then(|()| out.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("applejack doctor: {error}");
            ExitCode::FAILURE
        }
    }
}

fn render_facts(facts: &HostFacts) -> String {
    let free_disk = facts
        .free_disk_bytes
        .map_or_else(|| "unknown".to_owned(), |bytes| format!("{bytes} bytes"));
    let rosetta =
        facts.rosetta_present.map_or(
            "unknown",
            |present| {
                if present { "present" } else { "absent" }
            },
        );
    let identities = facts
        .developer_id_identities
        .map_or_else(|| "unknown".to_owned(), |count| count.to_string());
    let controllers = facts.controllers_unavailable.as_ref().map_or_else(
        || {
            let connected = facts.controllers.iter().filter(|pad| pad.connected).count();
            format!("{connected} connected")
        },
        |why| format!("unavailable ({why})"),
    );
    let rows: [(&str, String); 7] = [
        ("macOS version", facts.macos_version.clone()),
        (
            "architecture",
            facts.arch.clone().unwrap_or_else(|| "unknown".to_owned()),
        ),
        ("Rosetta", rosetta.to_owned()),
        ("free disk (data dir)", free_disk),
        ("screen recording", facts.screen_recording.to_string()),
        ("Developer ID identities", identities),
        ("controllers", controllers),
    ];
    let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    rows.iter()
        .map(|(label, value)| format!("{label:<width$}  {value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One line per pad, then the default the whole inventory gives: the text a
/// run's `prepare: hidraw` line carries. The per-pad column says what the
/// rule does with that pad as it is now, so it never contradicts the last
/// line: only a connected Sony pad disables hidraw; an away one says what it
/// would do if connected. An unavailable inventory says so instead of the
/// decision line, since a run then keeps an existing prefix's mode.
fn render_controllers(inventory: &Inventory) -> String {
    let mut lines: Vec<String> = inventory
        .pads
        .iter()
        .map(|pad| {
            let state = if pad.connected {
                "connected"
            } else {
                "not connected"
            };
            let default = match (pad.connected, pad.kind.hidraw_default()) {
                (true, HidrawMode::Disabled) => "hidraw default disabled",
                (true, HidrawMode::Enabled) => "hidraw default enabled",
                (false, HidrawMode::Disabled) => "would disable hidraw if connected",
                (false, HidrawMode::Enabled) => "would leave hidraw enabled if connected",
            };
            format!(
                "{}  {}  {}:{}  {}  {state}  {default}",
                pad.name, pad.transport, pad.vendor_id, pad.product_id, pad.kind
            )
        })
        .collect();
    match &inventory.unavailable {
        Some(why) if !inventory.decides_hidraw() => lines.push(format!(
            "controller inventory unavailable ({why}): a new prefix gets hidraw disabled, \
             an existing prefix keeps its mode"
        )),
        _ => {
            if lines.is_empty() {
                lines.push("no controllers".to_owned());
            }
            lines.push(HidrawDecision::from_inventory(inventory, None).to_string());
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use applejack_core::host::{ControllerFact, ControllerKind, Transport};

    const USB_PADS: &str = include_str!(
        "../../applejack-core/tests/fixtures/system_profiler/usb-dualsense-and-xbox.json"
    );
    const BLUETOOTH_AWAY: &str = include_str!(
        "../../applejack-core/tests/fixtures/system_profiler/bluetooth-dualsense-not-connected.json"
    );

    #[test]
    fn controllers_render_one_line_per_pad_then_the_decision() {
        let mut pads = host::controllers::parse_usb(USB_PADS).unwrap();
        pads.extend(host::controllers::parse_bluetooth(BLUETOOTH_AWAY).unwrap());
        let text = render_controllers(&Inventory::available(pads));
        assert_eq!(
            text,
            concat!(
                "DualSense Wireless Controller  usb  0x054c:0x0ce6  dualsense  connected  ",
                "hidraw default disabled\n",
                "Xbox Wireless Controller  usb  0x045e:0x0b12  xbox  connected  ",
                "hidraw default enabled\n",
                "DualSense Wireless Controller  bluetooth  0x054c:0x0ce6  dualsense  ",
                "not connected  would disable hidraw if connected\n",
                "hidraw disabled (DualSense connected)",
            )
        );
    }

    #[test]
    fn an_away_pad_never_contradicts_the_decision_line() {
        let pads = host::controllers::parse_bluetooth(BLUETOOTH_AWAY).unwrap();
        let text = render_controllers(&Inventory::available(pads));
        assert_eq!(
            text,
            concat!(
                "DualSense Wireless Controller  bluetooth  0x054c:0x0ce6  dualsense  ",
                "not connected  would disable hidraw if connected\n",
                "hidraw enabled (no Sony pad connected)",
            )
        );
        let other = ControllerFact {
            name: "Example Gamepad".to_owned(),
            transport: Transport::Bluetooth,
            vendor_id: "0x1234".to_owned(),
            product_id: String::new(),
            connected: false,
            kind: ControllerKind::Other,
        };
        let text = render_controllers(&Inventory::available(vec![other]));
        assert_eq!(
            text.lines().next(),
            Some(
                "Example Gamepad  bluetooth  0x1234:  other  not connected  \
                 would leave hidraw enabled if connected"
            )
        );
    }

    #[test]
    fn no_pads_and_an_unavailable_inventory_render_their_own_lines() {
        assert_eq!(
            render_controllers(&Inventory::default()),
            "no controllers\nhidraw enabled (no Sony pad connected)"
        );
        let text = render_controllers(&Inventory::unavailable("SPBluetoothDataType timed out"));
        assert_eq!(
            text,
            "controller inventory unavailable (SPBluetoothDataType timed out): a new prefix gets \
             hidraw disabled, an existing prefix keeps its mode"
        );
    }

    #[test]
    fn a_connected_sony_pad_in_a_partial_inventory_prints_the_decision_not_the_sentence() {
        let mut pads = host::controllers::parse_usb(USB_PADS).unwrap();
        let inventory = Inventory {
            pads: pads.clone(),
            unavailable: Some("SPBluetoothDataType timed out".to_owned()),
        };
        let text = render_controllers(&inventory);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert_eq!(
            lines[2],
            "hidraw disabled (DualSense connected; SPBluetoothDataType timed out)"
        );
        // The Xbox pad alone does not decide: the sentence stands.
        pads.retain(|pad| pad.kind == ControllerKind::Xbox);
        let inventory = Inventory {
            pads,
            unavailable: Some("SPBluetoothDataType timed out".to_owned()),
        };
        let text = render_controllers(&inventory);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(
            lines[1]
                .starts_with("controller inventory unavailable (SPBluetoothDataType timed out): "),
            "{text}"
        );
    }

    #[test]
    fn the_doctor_row_says_connected_or_unavailable() {
        let facts = HostFacts {
            macos_version: "26.0".to_owned(),
            arch: None,
            rosetta_present: None,
            free_disk_bytes: None,
            screen_recording: host::ScreenRecordingState::Unknown,
            developer_id_identities: None,
            controllers: Vec::new(),
            controllers_unavailable: None,
        };
        let row = |text: String| {
            text.lines()
                .find(|line| line.starts_with("controllers"))
                .map(str::to_owned)
        };
        assert_eq!(
            row(render_facts(&facts)).as_deref(),
            Some("controllers              0 connected")
        );
        let facts = facts.with_controllers(Inventory::unavailable("why"));
        assert_eq!(
            row(render_facts(&facts)).as_deref(),
            Some("controllers              unavailable (why)")
        );
    }
}
