//! Host facts behind `applejack doctor`.

use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

const DATA_DIR_RELATIVE: &str = "Library/Application Support/applejack";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostFacts {
    pub macos_version: String,
    pub arch: Option<String>,
    pub rosetta_present: Option<bool>,
    pub free_disk_bytes: Option<u64>,
    pub screen_recording: ScreenRecordingState,
    pub developer_id_identities: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenRecordingState {
    Unknown,
}

impl fmt::Display for ScreenRecordingState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

impl HostFacts {
    #[must_use]
    pub fn gather() -> Self {
        Self {
            macos_version: capture("/usr/bin/sw_vers", ["-productVersion"])
                .filter(|captured| captured.success)
                .and_then(|captured| parse_product_version(&captured.stdout))
                .unwrap_or_else(|| "unknown".to_owned()),
            // hw.machine is the chip the kernel runs on; `std::env::consts::ARCH`
            // is the binary's compile target and reads `aarch64` under Rosetta too.
            arch: capture("/usr/sbin/sysctl", ["-n", "hw.machine"])
                .filter(|captured| captured.success)
                .and_then(|captured| parse_hw_machine(&captured.stdout)),
            rosetta_present: capture("/usr/bin/arch", ["-x86_64", "/usr/bin/true"])
                .map(|captured| captured.success),
            free_disk_bytes: free_disk_bytes(&data_dir()),
            screen_recording: ScreenRecordingState::Unknown,
            developer_id_identities: capture(
                "/usr/bin/security",
                ["find-identity", "-v", "-p", "codesigning"],
            )
            .filter(|captured| captured.success)
            .map(|captured| count_developer_id_identities(&captured.stdout)),
        }
    }
}

#[must_use]
pub fn data_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map_or_else(|| PathBuf::from("/"), PathBuf::from)
        .join(DATA_DIR_RELATIVE)
}

#[must_use]
pub fn parse_product_version(text: &str) -> Option<String> {
    let version = text.trim();
    (!version.is_empty()).then(|| version.to_owned())
}

#[must_use]
pub fn parse_hw_machine(text: &str) -> Option<String> {
    let machine = text.trim();
    (!machine.is_empty() && !machine.contains(char::is_whitespace)).then(|| machine.to_owned())
}

/// Reads the `Available` column of `df -Pk` output and returns it in bytes.
///
/// The column is located relative to the `Capacity` field (the only one that
/// ends in `%`) because a filesystem name may contain spaces.
#[must_use]
pub fn parse_df_available_bytes(text: &str) -> Option<u64> {
    let row = text.lines().rev().find(|line| !line.trim().is_empty())?;
    let fields: Vec<&str> = row.split_whitespace().collect();
    let capacity = fields.iter().rposition(|field| field.ends_with('%'))?;
    let kibibytes: u64 = fields.get(capacity.checked_sub(1)?)?.parse().ok()?;
    kibibytes.checked_mul(1024)
}

#[must_use]
pub fn count_developer_id_identities(text: &str) -> usize {
    text.lines()
        .filter(|line| line.contains("\"Developer ID Application:"))
        .count()
}

struct Captured {
    success: bool,
    stdout: String,
}

fn capture<I, S>(program: &str, args: I) -> Option<Captured>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new(program).args(args).output().ok()?;
    Some(Captured {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
    })
}

fn free_disk_bytes(data_dir: &Path) -> Option<u64> {
    // The data directory does not exist until first install; df on its
    // nearest existing ancestor reports the same volume.
    let probe = data_dir.ancestors().find(|path| path.exists())?;
    capture("/bin/df", [OsStr::new("-Pk"), probe.as_os_str()])
        .filter(|captured| captured.success)
        .and_then(|captured| parse_df_available_bytes(&captured.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SW_VERS_SAMPLE: &str = "26.6.2\n";

    const HW_MACHINE_SAMPLE: &str = "arm64\n";

    const DF_SAMPLE: &str = concat!(
        "Filesystem   1024-blocks       Used  Available Capacity  Mounted on\n",
        "/dev/disk3s5  7811085600 6633997744 1141219520    86%    /System/Volumes/Data\n",
    );

    // autofs reports a filesystem name with a space; the sizes are synthetic
    // so a column slip cannot pass by coincidence.
    const DF_SPACED_FILESYSTEM_SAMPLE: &str = concat!(
        "Filesystem    1024-blocks Used Available Capacity Mounted on\n",
        "map auto_home          40   10        30    25%   /System/Volumes/Data/home\n",
    );

    // Same shape as a host with one Apple Development identity and no
    // Developer ID; the name, hash, and team id are synthetic.
    const SECURITY_NO_DEVELOPER_ID_SAMPLE: &str = concat!(
        "  1) 0000000000000000000000000000000000000000 \"Apple Development: Example Developer (TEAMID0000)\"\n",
        "     1 valid identities found\n",
    );

    // Same format with the identity kinds a signing host carries.
    const SECURITY_WITH_DEVELOPER_ID_SAMPLE: &str = concat!(
        "  1) 0000000000000000000000000000000000000000 \"Apple Development: Example Developer (TEAMID0000)\"\n",
        "  2) 1111111111111111111111111111111111111111 \"Developer ID Application: Example Developer (TEAMID0000)\"\n",
        "  3) 2222222222222222222222222222222222222222 \"Developer ID Installer: Example Developer (TEAMID0000)\"\n",
        "     3 valid identities found\n",
    );

    const SECURITY_EMPTY_SAMPLE: &str = "     0 valid identities found\n";

    #[test]
    fn product_version_is_trimmed() {
        assert_eq!(
            parse_product_version(SW_VERS_SAMPLE).as_deref(),
            Some("26.6.2")
        );
        assert_eq!(parse_product_version("  \n"), None);
    }

    #[test]
    fn hw_machine_is_a_single_trimmed_token() {
        assert_eq!(
            parse_hw_machine(HW_MACHINE_SAMPLE).as_deref(),
            Some("arm64")
        );
        assert_eq!(parse_hw_machine(""), None);
        assert_eq!(parse_hw_machine("  \n"), None);
        assert_eq!(parse_hw_machine("arm64 x86_64\n"), None);
    }

    #[test]
    fn df_available_column_converts_to_bytes() {
        assert_eq!(
            parse_df_available_bytes(DF_SAMPLE),
            Some(1_141_219_520 * 1024)
        );
    }

    #[test]
    fn df_available_column_survives_spaces_in_filesystem_name() {
        assert_eq!(
            parse_df_available_bytes(DF_SPACED_FILESYSTEM_SAMPLE),
            Some(30 * 1024)
        );
    }

    #[test]
    fn df_rejects_malformed_output() {
        assert_eq!(parse_df_available_bytes(""), None);
        assert_eq!(
            parse_df_available_bytes("Filesystem 1024-blocks Used Available\n"),
            None
        );
        assert_eq!(
            parse_df_available_bytes("/dev/disk3s5 1 2 lots 3% /\n"),
            None
        );
    }

    #[test]
    fn developer_id_count_matches_application_identities_only() {
        assert_eq!(
            count_developer_id_identities(SECURITY_NO_DEVELOPER_ID_SAMPLE),
            0
        );
        assert_eq!(
            count_developer_id_identities(SECURITY_WITH_DEVELOPER_ID_SAMPLE),
            1
        );
        assert_eq!(count_developer_id_identities(SECURITY_EMPTY_SAMPLE), 0);
    }

    #[test]
    fn screen_recording_serializes_as_unknown() {
        let facts = HostFacts {
            macos_version: "26.6.2".to_owned(),
            arch: None,
            rosetta_present: None,
            free_disk_bytes: None,
            screen_recording: ScreenRecordingState::Unknown,
            developer_id_identities: None,
        };
        let json = serde_json::to_value(&facts).expect("serializes");
        assert_eq!(json["screen_recording"], "unknown");
        assert!(json["arch"].is_null());
        assert!(json["free_disk_bytes"].is_null());
        assert!(json["rosetta_present"].is_null());
        assert!(json["developer_id_identities"].is_null());
    }

    #[test]
    fn data_dir_lives_under_application_support() {
        assert!(data_dir().ends_with(DATA_DIR_RELATIVE));
    }
}
