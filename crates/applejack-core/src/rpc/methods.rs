//! The method table: every JSON-RPC method the shell may call, its typed
//! parameters, and whether its work package has landed. Adding a method means
//! adding one row to `methods!` and one example to [`Method::examples`].

use super::wire::{ErrorCode, RpcError};
use crate::host::HostFacts;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::path::PathBuf;

/// Whether a method answers for real or with [`ErrorCode::NotImplemented`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodStatus {
    Implemented,
    /// Answered with `NotImplemented` until the named work package lands.
    Pending {
        work_package: &'static str,
    },
}

macro_rules! methods {
    ($( $variant:ident = $name:literal : $params:ty => $status:expr ; )*) => {
        /// One variant per wire method, tagged by its dotted name.
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        #[serde(tag = "method", content = "params")]
        pub enum Method {
            $( #[serde(rename = $name)] $variant($params), )*
        }

        impl Method {
            /// Every method name, in table order.
            pub const NAMES: &'static [&'static str] = &[$( $name ),*];

            #[must_use]
            pub const fn name(&self) -> &'static str {
                match self { $( Self::$variant(_) => $name, )* }
            }

            #[must_use]
            pub const fn status(&self) -> MethodStatus {
                match self { $( Self::$variant(_) => $status, )* }
            }
        }
    };
}

const IMPLEMENTED: MethodStatus = MethodStatus::Implemented;

const fn pending(work_package: &'static str) -> MethodStatus {
    MethodStatus::Pending { work_package }
}

methods! {
    Hello = "hello": HelloParams => IMPLEMENTED;
    HostDoctor = "host.doctor": HostDoctorParams => IMPLEMENTED;
    EngineList = "engine.list": EngineListParams => pending("2");
    EngineInstall = "engine.install": EngineInstallParams => pending("2");
    EngineUse = "engine.use": EngineUseParams => pending("2");
    EngineDoctor = "engine.doctor": EngineDoctorParams => pending("2");
    EngineProbeCpuid = "engine.probe_cpuid": EngineProbeCpuidParams => pending("2");
    EngineD3dmetalImport = "engine.d3dmetal.import": EngineD3dmetalImportParams => pending("2");
    EngineD3dmetalRemove = "engine.d3dmetal.remove": EngineD3dmetalRemoveParams => pending("2");
    SteamLoginStart = "steam.login.start": SteamLoginStartParams => pending("4");
    SteamLoginComplete = "steam.login.complete": SteamLoginCompleteParams => pending("4");
    SteamLogout = "steam.logout": SteamLogoutParams => pending("4");
    SteamLibrary = "steam.library": SteamLibraryParams => pending("4");
    AppInstall = "app.install": AppInstallParams => pending("4");
    AppRun = "app.run": AppRunParams => pending("5");
    AppCancel = "app.cancel": AppCancelParams => pending("5");
    RuntimeSteamInstall = "runtime.steam.install": RuntimeSteamInstallParams => pending("5");
    RuntimeSteamUpdate = "runtime.steam.update": RuntimeSteamUpdateParams => pending("5");
    RuntimeSteamRepair = "runtime.steam.repair": RuntimeSteamRepairParams => pending("5");
    RuntimeSteamStop = "runtime.steam.stop": RuntimeSteamStopParams => pending("5");
    ConfigGet = "config.get": ConfigGetParams => pending("6a");
    ConfigSet = "config.set": ConfigSetParams => pending("6a");
    ConfigExport = "config.export": ConfigExportParams => pending("6a");
    ConfigImport = "config.import": ConfigImportParams => pending("6a");
    PrefixSnapshot = "prefix.snapshot": PrefixSnapshotParams => pending("2");
    PrefixRestore = "prefix.restore": PrefixRestoreParams => pending("2");
    PrefixReset = "prefix.reset": PrefixResetParams => pending("2");
    SessionList = "session.list": SessionListParams => pending("5");
}

impl Method {
    /// Parses a wire request into the table. `null` params read as `{}`.
    ///
    /// # Errors
    ///
    /// `MethodNotFound` for a name outside the table, `InvalidParams` when
    /// the params do not fit the method's type.
    pub fn from_request(method: &str, params: Value) -> Result<Self, RpcError> {
        if !Self::NAMES.contains(&method) {
            return Err(RpcError::new(
                ErrorCode::MethodNotFound,
                format!("unknown method `{method}`"),
            ));
        }
        let params = if params.is_null() {
            Value::Object(Map::new())
        } else {
            params
        };
        serde_json::from_value(json!({"method": method, "params": params})).map_err(|error| {
            RpcError::new(
                ErrorCode::InvalidParams,
                format!("invalid params for `{method}`: {error}"),
            )
        })
    }

    /// The params as a JSON value, for logging and goldens.
    #[must_use]
    pub fn params(&self) -> Value {
        // The table's params are plain data; serializing them cannot fail.
        serde_json::to_value(self)
            .ok()
            .and_then(|mut tagged| tagged.get_mut("params").map(Value::take))
            .unwrap_or(Value::Null)
    }

    /// One example request per method, in table order; the goldens are
    /// generated from these.
    #[must_use]
    pub fn examples() -> Vec<Self> {
        vec![
            Self::Hello(HelloParams {
                protocol_version: super::PROTOCOL_VERSION,
            }),
            Self::HostDoctor(HostDoctorParams {}),
            Self::EngineList(EngineListParams {}),
            Self::EngineInstall(EngineInstallParams {
                engine_id: "rosetta-wine".into(),
            }),
            Self::EngineUse(EngineUseParams {
                engine_id: "rosetta-wine".into(),
                version: "10.0.1".into(),
            }),
            Self::EngineDoctor(EngineDoctorParams {}),
            Self::EngineProbeCpuid(EngineProbeCpuidParams {}),
            Self::EngineD3dmetalImport(EngineD3dmetalImportParams {
                path: PathBuf::from("/Volumes/Game Porting Toolkit"),
                license_acknowledged: true,
            }),
            Self::EngineD3dmetalRemove(EngineD3dmetalRemoveParams {}),
            Self::SteamLoginStart(SteamLoginStartParams {
                mode: LoginMode::Qr,
            }),
            Self::SteamLoginComplete(SteamLoginCompleteParams {
                session_id: "login-session-example".into(),
                steam_guard_code: Some("ABCDE".into()),
            }),
            Self::SteamLogout(SteamLogoutParams {}),
            Self::SteamLibrary(SteamLibraryParams { refresh: true }),
            Self::AppInstall(AppInstallParams {
                appid: 220,
                branch: Some("public".into()),
                update: false,
                verify: false,
            }),
            Self::AppRun(AppRunParams {
                appid: 220,
                probe_direct: false,
            }),
            Self::AppCancel(AppCancelParams {
                appid: 220,
                kill_prefix: false,
            }),
            Self::RuntimeSteamInstall(RuntimeSteamInstallParams {}),
            Self::RuntimeSteamUpdate(RuntimeSteamUpdateParams {}),
            Self::RuntimeSteamRepair(RuntimeSteamRepairParams { reinstall: false }),
            Self::RuntimeSteamStop(RuntimeSteamStopParams {}),
            Self::ConfigGet(ConfigGetParams { appid: 220 }),
            Self::ConfigSet(ConfigSetParams {
                appid: 220,
                key: "graphics_backend".into(),
                value: json!("dxmt"),
            }),
            Self::ConfigExport(ConfigExportParams {
                appid: 220,
                path: PathBuf::from("/tmp/220.preset.json"),
            }),
            Self::ConfigImport(ConfigImportParams {
                appid: 220,
                path: PathBuf::from("/tmp/220.preset.json"),
                accept_launch_overrides: false,
            }),
            Self::PrefixSnapshot(PrefixSnapshotParams {
                prefix: "220".into(),
            }),
            Self::PrefixRestore(PrefixRestoreParams {
                prefix: "220".into(),
                timestamp: "20261028T120000Z".into(),
            }),
            Self::PrefixReset(PrefixResetParams {
                prefix: "220".into(),
                keep_user_data: true,
            }),
            Self::SessionList(SessionListParams { appid: Some(220) }),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloParams {
    pub protocol_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloResult {
    pub protocol_version: u32,
    pub core_version: String,
}

/// The result of a method with nothing to return; encodes as `{}`. A result
/// is never `null` on the wire (see `to_result`), so the Swift mirror can
/// decode every success into a typed value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyResult {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostDoctorParams {}

pub type HostDoctorResult = HostFacts;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineListParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineInstallParams {
    pub engine_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineUseParams {
    pub engine_id: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineDoctorParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineProbeCpuidParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineD3dmetalImportParams {
    /// The mounted Game Porting Toolkit volume.
    pub path: PathBuf,
    /// The shell shows the license dialog; the core refuses without this.
    pub license_acknowledged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineD3dmetalRemoveParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LoginMode {
    Qr,
    Credentials {
        account_name: String,
        password: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SteamLoginStartParams {
    pub mode: LoginMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SteamLoginCompleteParams {
    pub session_id: String,
    pub steam_guard_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SteamLogoutParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SteamLibraryParams {
    #[serde(default)]
    pub refresh: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppInstallParams {
    pub appid: u32,
    pub branch: Option<String>,
    #[serde(default)]
    pub update: bool,
    #[serde(default)]
    pub verify: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppRunParams {
    pub appid: u32,
    #[serde(default)]
    pub probe_direct: bool,
}

/// Ends a running title; the whole prefix (`wineserver -k`) only on request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppCancelParams {
    pub appid: u32,
    #[serde(default)]
    pub kill_prefix: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSteamInstallParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSteamUpdateParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSteamRepairParams {
    #[serde(default)]
    pub reinstall: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSteamStopParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigGetParams {
    pub appid: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSetParams {
    pub appid: u32,
    pub key: String,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigExportParams {
    pub appid: u32,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigImportParams {
    pub appid: u32,
    pub path: PathBuf,
    #[serde(default)]
    pub accept_launch_overrides: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrefixSnapshotParams {
    pub prefix: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrefixRestoreParams {
    pub prefix: String,
    pub timestamp: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrefixResetParams {
    pub prefix: String,
    #[serde(default)]
    pub keep_user_data: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionListParams {
    pub appid: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn examples_cover_the_table_in_order() {
        let names: Vec<&str> = Method::examples().iter().map(Method::name).collect();
        assert_eq!(names, Method::NAMES);
    }

    #[test]
    fn names_are_unique_and_dotted() {
        let mut seen = std::collections::HashSet::new();
        for name in Method::NAMES {
            assert!(seen.insert(name), "duplicate method `{name}`");
            assert!(
                name.bytes().all(|byte| byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || byte == b'.'
                    || byte == b'_'),
                "method `{name}` is not lower.dotted"
            );
        }
    }

    #[test]
    fn from_request_distinguishes_unknown_method_from_bad_params() {
        let unknown = Method::from_request("nope", json!({})).unwrap_err();
        assert_eq!(unknown.code, ErrorCode::MethodNotFound);
        let bad = Method::from_request("engine.install", json!({})).unwrap_err();
        assert_eq!(bad.code, ErrorCode::InvalidParams);
        let extra =
            Method::from_request("hello", json!({"protocol_version": 1, "x": 1})).unwrap_err();
        assert_eq!(extra.code, ErrorCode::InvalidParams);
    }

    #[test]
    fn null_params_read_as_empty() {
        let method = Method::from_request("host.doctor", Value::Null).unwrap();
        assert_eq!(method, Method::HostDoctor(HostDoctorParams {}));
        assert_eq!(method.params(), json!({}));
    }

    #[test]
    fn method_tag_and_params_round_trip() {
        let method = Method::EngineUse(EngineUseParams {
            engine_id: "e".into(),
            version: "1".into(),
        });
        let value = serde_json::to_value(&method).unwrap();
        assert_eq!(
            value,
            json!({"method": "engine.use", "params": {"engine_id": "e", "version": "1"}})
        );
        assert_eq!(serde_json::from_value::<Method>(value).unwrap(), method);
    }
}
