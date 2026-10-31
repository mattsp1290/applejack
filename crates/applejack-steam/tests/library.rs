//! The library against synthetic PICS buffers.

#[allow(dead_code, reason = "each test binary uses part of the shared fake")]
mod support;

use applejack_steam::Error;
use applejack_steam::auth::License;
use applejack_steam::library::{DRM_UNKNOWN, DepotFilter, Library, OwnedApp};
use applejack_steam::session::AppInfo;
use support::FakeSession;

fn info(appid: u32, text: &str) -> AppInfo {
    AppInfo {
        appid,
        change_number: 7,
        text: Some(text.to_owned()),
    }
}

/// Windows x64 and x86 content, a German pack, a Linux depot, a redistributable
/// served from another app, and unowned DLC.
const GAME: &str = r#""appinfo"
{
	"appid"		"100"
	"common"
	{
		"name"		"Synthetic Game"
		"type"		"Game"
		"oslist"		"windows,linux"
	}
	"config"
	{
		"installdir"		"Synthetic Game"
	}
	"depots"
	{
		"101"
		{
			"config"
			{
				"oslist"		"windows"
				"osarch"		"64"
			}
			"manifests"
			{
				"public"
				{
					"gid"		"1001"
					"size"		"5000"
				}
				"beta"
				{
					"gid"		"1002"
					"size"		"6000"
				}
			}
		}
		"102"
		{
			"config"
			{
				"oslist"		"windows"
				"osarch"		"32"
			}
			"manifests"
			{
				"public"		"2001"
			}
			"maxsize"		"4000"
		}
		"103"
		{
			"manifests"
			{
				"public"
				{
					"gid"		"3001"
					"size"		"300"
				}
			}
		}
		"104"
		{
			"config"
			{
				"oslist"		"windows"
				"language"		"german"
			}
			"manifests"
			{
				"public"
				{
					"gid"		"4001"
					"size"		"400"
				}
			}
		}
		"105"
		{
			"config"
			{
				"oslist"		"linux"
			}
			"manifests"
			{
				"public"
				{
					"gid"		"5001"
					"size"		"500"
				}
			}
		}
		"106"
		{
			"depotfromapp"		"228980"
			"sharedinstall"		"1"
		}
		"107"
		{
			"dlcappid"		"999"
			"manifests"
			{
				"public"
				{
					"gid"		"7001"
					"size"		"700"
				}
			}
		}
		"branches"
		{
			"public"
			{
				"buildid"		"11"
			}
			"beta"
			{
				"buildid"		"12"
			}
		}
	}
}
"#;

const MAC_ONLY: &str = r#""appinfo"
{
	"common"
	{
		"name"		"Mac Game"
		"oslist"		"macos,linux"
	}
	"config"
	{
		"installdir"		"Mac Game"
	}
	"depots"
	{
		"201"
		{
			"manifests"
			{
				"public"
				{
					"gid"		"1"
					"size"		"1"
				}
			}
		}
	}
}
"#;

const THIRTY_TWO_ONLY: &str = r#""appinfo"
{
	"common"
	{
		"name"		"Old Game"
	}
	"config"
	{
		"installdir"		"Old Game"
	}
	"depots"
	{
		"301"
		{
			"config"
			{
				"oslist"		"windows"
				"osarch"		"32"
			}
			"manifests"
			{
				"public"
				{
					"gid"		"9"
					"size"		"9"
				}
			}
		}
	}
}
"#;

fn parse(appid: u32, text: &str) -> OwnedApp {
    OwnedApp::parse(&info(appid, text), &DepotFilter::default()).unwrap()
}

fn ids(app: &OwnedApp, branch: &str, filter: &DepotFilter) -> Vec<u32> {
    app.windows_depots(branch, filter)
        .unwrap()
        .iter()
        .map(|depot| depot.depot_id)
        .collect()
}

#[test]
fn parse_reads_the_fields_the_library_lists() {
    let app = parse(100, GAME);
    assert_eq!(app.appid, 100);
    assert_eq!(app.name, "Synthetic Game");
    assert_eq!(app.installdir, "Synthetic Game");
    assert_eq!(app.default_branch, "public");
    assert_eq!(app.drm, DRM_UNKNOWN);
    assert_eq!(app.size, 5300, "x64 plus the neutral depot");
    assert_eq!(app.platforms, ["linux", "windows"]);
    assert_eq!(app.builds["public"], 11);
    assert_eq!(app.change_number, 7);
    assert_eq!(app.depots.len(), 7);
}

#[test]
fn selection_takes_x86_64_and_leaves_32_bit_german_linux_and_redistributables() {
    let app = parse(100, GAME);
    assert_eq!(ids(&app, "public", &DepotFilter::default()), [101, 103]);
}

#[test]
fn selection_takes_32_bit_when_the_app_has_only_that() {
    let app = parse(300, THIRTY_TWO_ONLY);
    assert_eq!(ids(&app, "public", &DepotFilter::default()), [301]);
}

#[test]
fn selection_honours_language_owned_dlc_and_branch() {
    let app = parse(100, GAME);
    let german = DepotFilter {
        language: "german".to_owned(),
        ..DepotFilter::default()
    };
    assert_eq!(ids(&app, "public", &german), [101, 103, 104]);

    let with_dlc = DepotFilter {
        owned_appids: [999].into(),
        ..DepotFilter::default()
    };
    assert_eq!(ids(&app, "public", &with_dlc), [101, 103, 107]);

    assert_eq!(ids(&app, "beta", &DepotFilter::default()), [101]);
    assert_eq!(
        app.windows_depots("beta", &DepotFilter::default()).unwrap()[0].manifest_id,
        1002
    );
    assert!(matches!(
        app.windows_depots("nightly", &DepotFilter::default()),
        Err(Error::NoManifestForBranch { .. })
    ));
}

#[test]
fn an_app_with_no_windows_depot_is_refused_naming_its_platforms() {
    let library = Library::from_app_info(&[200], &[info(200, MAC_ONLY)]);
    assert_eq!(library.windows_apps().count(), 0);
    let error = library.windows_app(200).unwrap_err();
    assert_eq!(
        error.to_string(),
        "app 200 has no Windows depot; it lists: linux, macos"
    );
    assert!(matches!(
        library.apps[0].windows_depots("public", &DepotFilter::default()),
        Err(Error::NoWindowsDepot { .. })
    ));
    assert!(matches!(
        library.windows_app(1),
        Err(Error::AppNotOwned { appid: 1 })
    ));
}

#[tokio::test]
async fn fetch_walks_licenses_packages_and_product_info() {
    let session = FakeSession {
        licenses: vec![License {
            package_id: 1,
            license_type: 0,
            flags: 0,
            owner_id: 0,
            access_token: 0,
        }],
        appids: vec![100, 200, 400, 999],
        ..FakeSession::default()
    };
    session.app_info.lock().unwrap().push_back(vec![
        info(100, GAME),
        info(200, MAC_ONLY),
        AppInfo {
            appid: 400,
            change_number: 0,
            text: None,
        },
    ]);
    let library = Library::fetch(&session).await.unwrap();
    assert_eq!(
        library
            .windows_apps()
            .map(|app| app.appid)
            .collect::<Vec<_>>(),
        [100]
    );
    assert_eq!(library.skipped, [400, 999]);
    assert!(library.filter().owned_appids.contains(&999));
    assert_eq!(library.windows_app(100).unwrap().size, 6000);
}
