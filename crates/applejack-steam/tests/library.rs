//! The library against synthetic PICS buffers.

#[allow(dead_code, reason = "each test binary uses part of the shared fake")]
mod support;

use applejack_steam::Error;
use applejack_steam::auth::License;
use applejack_steam::library::{DRM_UNKNOWN, DepotFilter, Library, OwnedApp};
use applejack_steam::session::{AppInfo, PackageGrants};
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

/// Licenses every depot of the fixtures except DLC, and not the shared one.
fn licensed() -> DepotFilter {
    DepotFilter {
        owned_depots: [101, 102, 103, 104, 105, 201, 301].into(),
        ..DepotFilter::default()
    }
}

fn parse(appid: u32, text: &str) -> OwnedApp {
    OwnedApp::parse(&info(appid, text), &licensed()).unwrap()
}

fn grants(appids: &[u32], depot_ids: &[u32]) -> PackageGrants {
    PackageGrants {
        appids: appids.iter().copied().collect(),
        depot_ids: depot_ids.iter().copied().collect(),
    }
}

fn ids(app: &OwnedApp, branch: &str, filter: &DepotFilter) -> Vec<u32> {
    app.windows_depots(branch, filter)
        .unwrap()
        .depots
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
    assert_eq!(ids(&app, "public", &licensed()), [101, 103]);
}

#[test]
fn selection_takes_32_bit_when_the_app_has_only_that() {
    let app = parse(300, THIRTY_TWO_ONLY);
    assert_eq!(ids(&app, "public", &licensed()), [301]);
}

#[test]
fn selection_honours_language_owned_dlc_and_branch() {
    let app = parse(100, GAME);
    let german = DepotFilter {
        language: "german".to_owned(),
        ..licensed()
    };
    assert_eq!(ids(&app, "public", &german), [101, 103, 104]);

    let with_dlc = DepotFilter {
        owned_appids: [999].into(),
        ..licensed()
    };
    assert_eq!(ids(&app, "public", &with_dlc), [101, 103, 107]);

    assert_eq!(ids(&app, "beta", &licensed()), [101]);
    assert_eq!(
        app.windows_depots("beta", &licensed()).unwrap().depots[0].manifest_id,
        1002
    );
    assert!(matches!(
        app.windows_depots("nightly", &licensed()),
        Err(Error::NoManifestForBranch { .. })
    ));
}

#[test]
fn an_app_with_no_windows_depot_is_refused_naming_its_platforms() {
    let library = Library::from_app_info(&grants(&[200], &[201]), &[info(200, MAC_ONLY)]);
    assert_eq!(library.windows_apps().count(), 0);
    let error = library.windows_app(200).unwrap_err();
    assert_eq!(
        error.to_string(),
        "app 200 has no Windows depot; it lists: linux, macos"
    );
    assert!(matches!(
        library.apps[0].windows_depots("public", &library.filter()),
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
        grants: grants(&[100, 200, 400, 999], &[101, 103]),
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
    session.app_info.lock().unwrap().push_back(Vec::new());
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
    assert!(library.filter().owned_depots.contains(&103));
    assert_eq!(library.windows_app(100).unwrap().size, 6000);
}

// From the protocol review's `proof_shared_and_unlicensed_depots`, which
// showed depots no license grants being planned.
#[test]
fn a_depot_no_license_grants_is_skipped_and_reported() {
    let app = parse(100, GAME);
    let filter = DepotFilter {
        owned_depots: [101].into(),
        ..DepotFilter::default()
    };
    let selection = app.windows_depots("public", &filter).unwrap();
    assert_eq!(selection.depots.len(), 1);
    assert_eq!(selection.depots[0].depot_id, 101);
    assert_eq!(selection.not_licensed, [103, 106, 107]);
}

#[test]
fn a_package_appid_grants_a_depot_of_that_id_as_depotdownloader_reads_it() {
    let app = parse(300, THIRTY_TWO_ONLY);
    let filter = DepotFilter {
        owned_appids: [301].into(),
        ..DepotFilter::default()
    };
    assert_eq!(ids(&app, "public", &filter), [301]);
}

#[test]
fn a_title_with_no_licensed_depot_is_refused_with_a_clear_reason() {
    let app = parse(300, THIRTY_TWO_ONLY);
    let error = app
        .windows_depots("public", &DepotFilter::default())
        .unwrap_err();
    assert!(
        matches!(&error, Error::NoLicensedDepot { appid: 300, depots } if *depots == [301]),
        "{error}"
    );
    assert_eq!(
        error.to_string(),
        "app 300: this account licenses none of its Windows depots (301)"
    );
}

#[test]
fn a_free_app_needs_no_license_but_its_paid_dlc_does() {
    for key in ["freetodownload", "FreeToDownload"] {
        let text = GAME.replace("\"type\"", &format!("\"{key}\"\t\"1\"\n\"type\""));
        let text = text.replace("\"depotfromapp\"\t\t\"228980\"", "");
        let filter = DepotFilter::default();
        let app = OwnedApp::parse(&info(100, &text), &filter).unwrap();
        assert!(app.free_to_download, "{key}");
        let selection = app.windows_depots("public", &filter).unwrap();
        let chosen: Vec<u32> = selection.depots.iter().map(|d| d.depot_id).collect();
        assert_eq!(chosen, [101, 103], "{key}");
        assert_eq!(selection.not_licensed, [107], "{key}");
    }
}

#[test]
fn product_info_keys_are_read_in_any_case() {
    let app = parse(200, &MAC_ONLY.replace("\"oslist\"", "\"OSList\""));
    assert_eq!(app.platforms, ["linux", "macos"]);
    assert!(!app.has_windows_depot());
}

const SHARED: &str = r#""appinfo"
{
	"common"
	{
		"name"		"Shared Game"
	}
	"config"
	{
		"installdir"		"Shared Game"
	}
	"depots"
	{
		"501"
		{
			"manifests"
			{
				"public"
				{
					"gid"		"1"
					"size"		"10"
				}
				"beta"
				{
					"gid"		"2"
					"size"		"10"
				}
			}
		}
		"502"
		{
			"depotfromapp"		"900"
		}
		"branches"
		{
			"public"
			{
				"buildid"		"1"
			}
			"beta"
			{
				"buildid"		"2"
			}
		}
	}
}
"#;

const PARENT: &str = r#""appinfo"
{
	"depots"
	{
		"502"
		{
			"manifests"
			{
				"public"
				{
					"gid"		"77"
					"size"		"5"
				}
			}
		}
	}
}
"#;

#[tokio::test]
async fn fetch_resolves_a_shared_depot_through_an_unowned_parent() {
    let session = FakeSession {
        grants: grants(&[500], &[501, 502]),
        ..FakeSession::default()
    };
    session
        .app_info
        .lock()
        .unwrap()
        .push_back(vec![info(500, SHARED)]);
    session
        .app_info
        .lock()
        .unwrap()
        .push_back(vec![info(900, PARENT)]);

    let library = Library::fetch(&session).await.unwrap();

    assert_eq!(library.apps.len(), 1, "a parent is not an owned app");
    let app = library.windows_app(500).unwrap();
    assert_eq!(app.size, 15);
    let filter = library.filter();
    let selection = app.windows_depots("public", &filter).unwrap();
    assert_eq!(selection.depots[1].depot_id, 502);
    assert_eq!(selection.depots[1].manifest_id, 77);
    assert_eq!(selection.depots[1].source_app, Some(900));
    assert!(!selection.depots[1].public_manifest);

    let beta = app.windows_depots("beta", &filter).unwrap();
    assert_eq!(beta.depots[1].manifest_id, 77);
    assert!(beta.depots[1].public_manifest);
}

#[test]
fn a_free_app_names_itself_for_a_shared_depot() {
    let text = SHARED.replace("\"name\"", "\"freetodownload\" \"1\"\n\"name\"");
    let library = Library::from_app_info(
        &grants(&[500], &[501, 502]),
        &[info(500, &text), info(900, PARENT)],
    );
    let app = library.windows_app(500).unwrap();
    let selection = app.windows_depots("public", &library.filter()).unwrap();
    assert_eq!(selection.depots[1].source_app, None);
}

#[test]
fn a_shared_depot_that_stays_unresolved_refuses_the_selection() {
    for parent in [
        None,
        Some(info(900, "\"appinfo\"\n{\n}\n")),
        // One level only: the parent's own link is not followed.
        Some(info(
            900,
            "\"appinfo\"\n{\n\"depots\"\n{\n\"502\"\n{\n\"depotfromapp\" \"901\"\n}\n}\n}\n",
        )),
    ] {
        let mut infos = vec![info(500, SHARED)];
        infos.extend(parent);
        let library = Library::from_app_info(&grants(&[500], &[501, 502]), &infos);
        let error = library.apps[0]
            .windows_depots("public", &library.filter())
            .unwrap_err();
        assert!(
            matches!(
                error,
                Error::UnresolvedSharedDepot {
                    appid: 500,
                    depot_id: 502,
                    from_app: 900,
                }
            ),
            "{error}"
        );
    }
}

#[test]
fn a_depot_that_names_its_own_app_does_not_loop() {
    let text = SHARED.replace("\"900\"", "\"500\"");
    let library = Library::from_app_info(&grants(&[500], &[501, 502]), &[info(500, &text)]);
    assert!(matches!(
        library.apps[0].windows_depots("public", &library.filter()),
        Err(Error::UnresolvedSharedDepot { depot_id: 502, .. })
    ));
}

#[test]
fn an_unlicensed_shared_depot_is_skipped_rather_than_refused() {
    let library = Library::from_app_info(&grants(&[500], &[501]), &[info(500, SHARED)]);
    let selection = library.apps[0]
        .windows_depots("public", &library.filter())
        .unwrap();
    assert_eq!(selection.not_licensed, [502]);
}
