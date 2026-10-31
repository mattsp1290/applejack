//! Install, resume, delta, and verify against a scripted session, through the
//! real `DepotJob`. Chunks are encrypted the way the CDN serves them, so the
//! hash checks that run here are the ones that run live.

#[allow(dead_code, reason = "each test binary uses part of the shared fake")]
mod support;

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use applejack_steam::Error;
use applejack_steam::acf::{AppManifest, app_state};
use applejack_steam::auth::AuthError;
use applejack_steam::depot::{
    DepotInstaller, DownloadRecord, InstallOptions, InstallPlan, InstallProgress, LibraryPaths,
    verify_install,
};
use applejack_steam::library::{DepotFilter, Library, OwnedApp};
use applejack_steam::lock::{LockDir, Mode, app_lock_name};
use applejack_steam::session::{AppInfo, PackageGrants};
use steamroom::depot::ChunkId;
use steamroom::depot::manifest::ManifestFile;
use steamroom_client::download::RetryConfig;
use support::{FakeSession, Fault, file, manifest};
use tempfile::TempDir;

const APPID: u32 = 100;
const DEPOT: u32 = 101;
const V1: u64 = 1001;
const V2: u64 = 1002;
/// A second depot, for titles with more than one.
const OTHER: u32 = 103;
/// The app that holds a shared depot.
const PARENT: u32 = 900;
const NOT_RUNNING: fn() -> bool = || false;

fn product_info(manifest_id: u64, buildid: u64) -> AppInfo {
    let text = format!(
        r#""appinfo"
{{
	"common"
	{{
		"name"		"Synthetic Game"
		"oslist"		"windows"
	}}
	"config"
	{{
		"installdir"		"Synthetic Game"
	}}
	"depots"
	{{
		"{DEPOT}"
		{{
			"config"
			{{
				"oslist"		"windows"
				"osarch"		"64"
			}}
			"manifests"
			{{
				"public"
				{{
					"gid"		"{manifest_id}"
					"size"		"100"
				}}
			}}
		}}
		"branches"
		{{
			"public"
			{{
				"buildid"		"{buildid}"
			}}
		}}
	}}
}}
"#
    );
    AppInfo {
        appid: APPID,
        change_number: 1,
        text: Some(text),
    }
}

/// The account in these tests licenses every depot they use.
fn licensed() -> DepotFilter {
    DepotFilter {
        owned_depots: [DEPOT, OTHER].into(),
        ..DepotFilter::default()
    }
}

fn plan(manifest_id: u64, buildid: u64) -> InstallPlan {
    let filter = licensed();
    let app = OwnedApp::parse(&product_info(manifest_id, buildid), &filter).unwrap();
    InstallPlan::new(&app, "public", &filter).unwrap()
}

struct Bench {
    dir: TempDir,
    session: FakeSession,
}

impl Bench {
    fn new() -> Self {
        Self {
            dir: TempDir::new().unwrap(),
            session: FakeSession::default(),
        }
    }

    fn paths(&self) -> LibraryPaths {
        LibraryPaths::new(self.dir.path().join("library"))
    }

    fn data(&self) -> &Path {
        self.dir.path()
    }

    fn game(&self) -> std::path::PathBuf {
        self.paths().install_dir("Synthetic Game")
    }

    fn installer<'a>(
        &'a self,
        client_running: &'a (dyn Fn() -> bool + Send + Sync),
    ) -> DepotInstaller<'a, FakeSession> {
        let mut retry = RetryConfig::default();
        retry.max_attempts = 2;
        retry.initial_delay = Duration::from_millis(1);
        DepotInstaller::new(&self.session, self.paths(), self.data(), client_running).with_options(
            InstallOptions {
                max_downloads: 1,
                retry,
            },
        )
    }

    async fn install(&self, plan: &InstallPlan) -> Result<(), Error> {
        self.installer(&NOT_RUNNING).install(plan).await.map(drop)
    }

    fn v1_files(&self) -> Vec<ManifestFile> {
        let chunks = &self.session.chunks;
        vec![
            file(
                chunks,
                "bin\\game.exe",
                &[b"game code one", b"game code two"],
            ),
            file(chunks, "data\\level.pak", &[b"level data"]),
            file(chunks, "old\\gone.txt", &[b"removed by the update"]),
        ]
    }

    fn add_v1(&self) {
        let files = self.v1_files();
        self.session.add_manifest(DEPOT, manifest(V1, files));
    }

    /// `level.pak` is unchanged, `game.exe` keeps one chunk, `gone.txt` goes,
    /// and `new.txt` arrives.
    fn add_v2(&self) {
        let chunks = &self.session.chunks;
        let files = vec![
            file(
                chunks,
                "bin\\game.exe",
                &[b"game code one", b"game code 2!!"],
            ),
            file(chunks, "data\\level.pak", &[b"level data"]),
            file(chunks, "new.txt", &[b"added by the update"]),
        ];
        self.session.add_manifest(DEPOT, manifest(V2, files));
    }

    fn record(&self) -> DownloadRecord {
        serde_json::from_slice(&fs::read(self.paths().record(APPID)).unwrap()).unwrap()
    }

    fn acf(&self) -> AppManifest {
        AppManifest::parse(&fs::read_to_string(self.paths().acf(APPID)).unwrap()).unwrap()
    }
}

fn chunk_of(entry: &ManifestFile, index: usize) -> ChunkId {
    entry.chunks[index].id.clone()
}

#[tokio::test]
async fn install_writes_the_game_and_an_acf_the_reader_accepts() {
    let bench = Bench::new();
    bench.add_v1();
    bench.install(&plan(V1, 11)).await.unwrap();

    assert_eq!(
        fs::read(bench.game().join("bin/game.exe")).unwrap(),
        b"game code onegame code two"
    );
    assert_eq!(
        fs::read(bench.game().join("data/level.pak")).unwrap(),
        b"level data"
    );
    assert!(!bench.game().join(".DepotDownloader").exists());
    assert!(!bench.paths().record(APPID).exists());
    assert!(bench.paths().manifest(DEPOT, V1).exists());

    let acf = bench.acf();
    assert_eq!(acf.appid, APPID);
    assert_eq!(acf.name, "Synthetic Game");
    assert_eq!(acf.installdir, "Synthetic Game");
    assert_eq!(acf.state_flags, app_state::FULLY_INSTALLED);
    assert_eq!(acf.state_flags, 4);
    assert_eq!(acf.buildid, 11);
    assert_ne!(acf.last_updated, 0);
    assert_eq!(acf.size_on_disk, 26 + 10 + 21);
    assert_eq!(acf.installed_depots.len(), 1);
    assert_eq!(acf.installed_depots[0].depot_id, DEPOT);
    assert_eq!(acf.installed_depots[0].manifest_id, V1);
    assert_eq!(acf.installed_depots[0].size, 26 + 10 + 21);
    assert!(acf.is_fully_installed());
}

#[tokio::test]
async fn installing_an_installed_title_is_refused() {
    let bench = Bench::new();
    bench.add_v1();
    bench.install(&plan(V1, 11)).await.unwrap();
    assert!(matches!(
        bench.install(&plan(V1, 11)).await,
        Err(Error::AlreadyInstalled { appid: APPID })
    ));
}

#[tokio::test]
async fn progress_reaches_the_sink() {
    let bench = Bench::new();
    bench.add_v1();
    let reports = std::sync::Mutex::new(Vec::new());
    let sink = |report: &InstallProgress| reports.lock().unwrap().push(*report);
    bench
        .installer(&NOT_RUNNING)
        .with_progress(&sink)
        .install(&plan(V1, 11))
        .await
        .unwrap();
    let reports = reports.into_inner().unwrap();
    let last = reports.last().unwrap();
    assert_eq!(last.total_files, 3);
    assert_eq!(last.completed_files, 3);
    assert_eq!(last.completed_bytes, last.total_bytes);
}

#[tokio::test]
async fn a_corrupted_chunk_is_rejected_and_never_reaches_the_install() {
    let bench = Bench::new();
    let files = bench.v1_files();
    bench
        .session
        .chunks
        .fault(&chunk_of(&files[0], 1), Fault::Corrupt);
    bench.session.add_manifest(DEPOT, manifest(V1, files));

    let error = bench.install(&plan(V1, 11)).await.unwrap_err();
    assert!(
        matches!(
            error,
            Error::CorruptChunk {
                depot_id: DEPOT,
                ..
            }
        ),
        "{error}"
    );
    assert!(!bench.game().join("bin/game.exe").exists());
    assert!(!bench.paths().acf(APPID).exists());
    assert!(bench.paths().record(APPID).exists());

    bench.session.chunks.clear_faults();
    bench.install(&plan(V1, 11)).await.unwrap();
    assert!(bench.acf().is_fully_installed());
}

#[tokio::test]
async fn a_failed_run_resumes_from_its_record_without_refetching_finished_files() {
    let bench = Bench::new();
    let files = bench.v1_files();
    let late = chunk_of(&files[2], 0);
    bench
        .session
        .chunks
        .fault(&late, Fault::Fail(io::ErrorKind::ConnectionReset));
    bench.session.add_manifest(DEPOT, manifest(V1, files));

    let error = bench.install(&plan(V1, 11)).await.unwrap_err();
    assert!(
        matches!(
            error,
            Error::Depot {
                depot_id: DEPOT,
                ..
            }
        ),
        "{error}"
    );
    let record = bench.record();
    assert_eq!(record.depots[0].manifest_id, V1);
    assert!(!record.depots[0].complete);
    assert!(record.depots[0].files.contains("bin\\game.exe"));
    assert!(record.depots[0].files.contains("data\\level.pak"));
    assert!(!record.depots[0].files.contains("old\\gone.txt"));

    bench.session.chunks.clear_faults();
    bench.session.chunks.reset_counts();
    let outcome = bench
        .installer(&NOT_RUNNING)
        .install(&plan(V1, 11))
        .await
        .unwrap();
    assert!(outcome.resumed);
    assert_eq!(bench.session.chunks.fetches(), 1);
    assert_eq!(*bench.session.chunks.fetched.lock().unwrap(), [late]);
    assert_eq!(bench.session.manifest_calls.load(Ordering::Relaxed), 1);
    assert!(!bench.paths().record(APPID).exists());
}

#[tokio::test]
async fn resume_picks_up_a_partial_record_from_disk_and_refetches_a_vanished_file() {
    let bench = Bench::new();
    bench.add_v1();
    fs::create_dir_all(bench.game().join("data")).unwrap();
    fs::create_dir_all(bench.game().join("bin")).unwrap();
    fs::write(bench.game().join("data/level.pak"), b"level data").unwrap();
    let path = bench.paths().record(APPID);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        format!(
            r#"{{"version":1,"appid":{APPID},"branch":"public","depots":[{{"depot_id":{DEPOT},
            "manifest_id":{V1},"previous_manifest_id":null,"complete":false,
            "files":["data\\level.pak","bin\\game.exe"]}}]}}"#
        ),
    )
    .unwrap();

    let outcome = bench
        .installer(&NOT_RUNNING)
        .install(&plan(V1, 11))
        .await
        .unwrap();
    assert!(outcome.resumed);
    // Three chunks: both of game.exe, which the record claimed but is gone,
    // and gone.txt. level.pak was not fetched.
    assert_eq!(bench.session.chunks.fetches(), 3);
    assert!(bench.game().join("bin/game.exe").exists());
}

#[tokio::test]
async fn a_record_for_another_manifest_is_not_resumed() {
    let bench = Bench::new();
    bench.add_v1();
    let path = bench.paths().record(APPID);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        format!(
            r#"{{"version":1,"appid":{APPID},"branch":"public","depots":[{{"depot_id":{DEPOT},
            "manifest_id":{V2},"previous_manifest_id":null,"complete":true,"files":[]}}]}}"#
        ),
    )
    .unwrap();
    let outcome = bench
        .installer(&NOT_RUNNING)
        .install(&plan(V1, 11))
        .await
        .unwrap();
    assert!(!outcome.resumed);
    assert_eq!(outcome.files_written, 3);
}

#[tokio::test]
async fn disk_full_keeps_the_partial_state_and_the_next_run_finishes() {
    let bench = Bench::new();
    let files = bench.v1_files();
    bench.session.chunks.fault(
        &chunk_of(&files[1], 0),
        Fault::Fail(io::ErrorKind::StorageFull),
    );
    bench.session.add_manifest(DEPOT, manifest(V1, files));

    let error = bench.install(&plan(V1, 11)).await.unwrap_err();
    assert!(
        matches!(error, Error::DiskFull { appid: APPID, .. }),
        "{error}"
    );
    assert!(bench.record().depots[0].files.contains("bin\\game.exe"));
    assert!(bench.game().join("bin/game.exe").exists());
    assert!(!bench.paths().acf(APPID).exists());

    bench.session.chunks.clear_faults();
    bench.install(&plan(V1, 11)).await.unwrap();
    assert!(bench.acf().is_fully_installed());
}

#[tokio::test]
async fn a_cdn_rate_limit_is_typed_and_attempts_are_bounded() {
    let bench = Bench::new();
    let files = bench.v1_files();
    bench
        .session
        .chunks
        .fault(&chunk_of(&files[0], 0), Fault::RateLimited);
    bench.session.add_manifest(DEPOT, manifest(V1, files));

    let error = bench.install(&plan(V1, 11)).await.unwrap_err();
    assert!(
        matches!(error, Error::Auth(AuthError::RateLimited { .. })),
        "{error}"
    );
    assert!(bench.session.chunks.fetches() <= 3, "retries are bounded");
}

#[tokio::test]
async fn a_rate_limit_from_the_session_is_typed_and_not_retried() {
    let bench = Bench::new();
    bench.add_v1();
    *bench.session.manifest_error.lock().unwrap() = Some(AuthError::RateLimited {
        retry_after: Duration::from_secs(60),
    });
    let error = bench.install(&plan(V1, 11)).await.unwrap_err();
    assert!(matches!(error, Error::Auth(AuthError::RateLimited { .. })));
    assert_eq!(bench.session.manifest_calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn update_touches_only_changed_files() {
    let bench = Bench::new();
    bench.add_v1();
    bench.install(&plan(V1, 11)).await.unwrap();
    let level = bench.game().join("data/level.pak");
    let before = fs::metadata(&level).unwrap();
    // A key the client wrote must survive the rewrite.
    let acf_path = bench.paths().acf(APPID);
    let text = fs::read_to_string(&acf_path).unwrap();
    fs::write(
        &acf_path,
        text.replace(
            "\t\"Universe\"\t\t\"1\"\n",
            "\t\"Universe\"\t\t\"1\"\n\t\"LauncherPath\"\t\t\"redacted\"\n",
        ),
    )
    .unwrap();

    bench.add_v2();
    bench.session.chunks.reset_counts();
    let outcome = bench
        .installer(&NOT_RUNNING)
        .update(&plan(V2, 12))
        .await
        .unwrap();

    assert_eq!(outcome.changed_depots, [DEPOT]);
    assert_eq!(outcome.files_written, 2);
    assert_eq!(outcome.files_removed, 1);
    // The changed chunk of game.exe and new.txt; the unchanged chunk came
    // off disk.
    assert_eq!(bench.session.chunks.fetches(), 2);
    assert_eq!(
        fs::read(bench.game().join("bin/game.exe")).unwrap(),
        b"game code onegame code 2!!"
    );
    assert_eq!(
        fs::read(bench.game().join("new.txt")).unwrap(),
        b"added by the update"
    );
    assert!(!bench.game().join("old/gone.txt").exists());
    assert!(!bench.game().join("old").exists());

    let after = fs::metadata(&level).unwrap();
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(before.ino(), after.ino());
    }

    let acf = bench.acf();
    assert_eq!(acf.buildid, 12);
    assert_eq!(acf.installed_depots[0].manifest_id, V2);
    assert!(
        fs::read_to_string(&acf_path)
            .unwrap()
            .contains("LauncherPath")
    );
    assert!(!bench.paths().manifest(DEPOT, V1).exists());
    assert!(bench.paths().manifest(DEPOT, V2).exists());
}

#[tokio::test]
async fn update_keeps_a_file_that_moved_to_a_depot_it_did_not_change() {
    let plan_with = |first: u64, build: u64| {
        let info = product_info(first, build);
        let text = info.text.as_ref().unwrap().replace(
            "\"branches\"",
            &format!(
                "\"{OTHER}\"\n{{\n\"manifests\"\n{{\n\"public\"\n{{\n\"gid\" \"7\"\n\"size\" \"1\"\n}}\n}}\n}}\n\"branches\""
            ),
        );
        let filter = licensed();
        let info = AppInfo {
            text: Some(text),
            ..info
        };
        let app = OwnedApp::parse(&info, &filter).unwrap();
        InstallPlan::new(&app, "public", &filter).unwrap()
    };
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    let moved = file(chunks, "moved.txt", &[b"same bytes"]);
    bench
        .session
        .add_manifest(DEPOT, manifest(V1, vec![moved.clone()]));
    bench.session.add_manifest(
        OTHER,
        manifest(7, vec![moved, file(chunks, "b.txt", &[b"b"])]),
    );
    bench.install(&plan_with(V1, 11)).await.unwrap();

    bench.session.add_manifest(
        DEPOT,
        manifest(V2, vec![file(chunks, "a.txt", &[b"new content"])]),
    );
    let outcome = bench
        .installer(&NOT_RUNNING)
        .update(&plan_with(V2, 12))
        .await
        .unwrap();
    assert_eq!(outcome.changed_depots, [DEPOT]);
    assert_eq!(outcome.files_removed, 0);
    assert!(bench.game().join("moved.txt").exists());
    assert_eq!(bench.acf().installed_depots.len(), 2);
}

#[tokio::test]
async fn an_update_keeps_a_file_renamed_only_in_case() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    bench.session.add_manifest(
        DEPOT,
        manifest(V1, vec![file(chunks, "Data\\Level.pak", &[b"same"])]),
    );
    bench.install(&plan(V1, 11)).await.unwrap();
    let folds_case = bench.game().join("DATA").exists();
    bench.session.add_manifest(
        DEPOT,
        manifest(V2, vec![file(chunks, "data\\level.pak", &[b"same"])]),
    );
    let outcome = bench
        .installer(&NOT_RUNNING)
        .update(&plan(V2, 12))
        .await
        .unwrap();
    assert_eq!(outcome.files_removed, u64::from(!folds_case));
    assert_eq!(
        verify_install(&bench.paths(), bench.data(), APPID)
            .unwrap()
            .missing,
        Vec::<String>::new()
    );
}

// From the install-safety review, probes p4 and p4b. `files_written: 0` was
// the tell: `DepotJob` hashed the old spelling as the new file and skipped
// it, and the stale pass then deleted it. On a volume that keeps the two
// spellings apart the old one goes and the new one is written; verify holds
// either way.
#[tokio::test]
async fn an_update_keeps_a_file_respelled_across_unicode_forms() {
    for (before, after) in [
        ("caf\u{e9}.pak", "cafe\u{301}.pak"),
        ("STRASSE.pak", "stra\u{df}e.pak"),
    ] {
        let bench = Bench::new();
        let chunks = &bench.session.chunks;
        bench
            .session
            .add_manifest(DEPOT, manifest(V1, vec![file(chunks, before, &[b"same"])]));
        bench.install(&plan(V1, 11)).await.unwrap();
        bench
            .session
            .add_manifest(DEPOT, manifest(V2, vec![file(chunks, after, &[b"same"])]));
        bench
            .installer(&NOT_RUNNING)
            .update(&plan(V2, 12))
            .await
            .unwrap();
        assert_eq!(fs::read(bench.game().join(after)).unwrap(), b"same");
        let report = verify_install(&bench.paths(), bench.data(), APPID).unwrap();
        assert!(report.is_clean(), "{after}: {report:?}");
    }
}

// From the install-safety review, probe p5: `mods` moved to another disk and
// linked back.
#[tokio::test]
async fn an_update_will_not_write_through_a_link_out_of_the_install() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    let outside = bench.dir.path().join("Documents");
    fs::create_dir_all(&outside).unwrap();
    bench.session.add_manifest(
        DEPOT,
        manifest(
            V1,
            vec![
                file(chunks, "mods\\b.txt", &[b"b1"]),
                file(chunks, "g.exe", &[b"1"]),
            ],
        ),
    );
    bench.install(&plan(V1, 11)).await.unwrap();
    fs::rename(bench.game().join("mods"), outside.join("mods")).unwrap();
    std::os::unix::fs::symlink(outside.join("mods"), bench.game().join("mods")).unwrap();

    bench.session.add_manifest(
        DEPOT,
        manifest(
            V2,
            vec![
                file(chunks, "mods\\b.txt", &[b"b2"]),
                file(chunks, "g.exe", &[b"2"]),
            ],
        ),
    );
    let error = bench
        .installer(&NOT_RUNNING)
        .update(&plan(V2, 12))
        .await
        .unwrap_err();

    let Error::LinkOutOfInstall { link, target } = &error else {
        panic!("{error}");
    };
    assert_eq!(*link, bench.game().join("mods"));
    assert_eq!(*target, outside.join("mods"));
    assert!(error.to_string().contains("Documents"));
    assert_eq!(fs::read(outside.join("mods/b.txt")).unwrap(), b"b1");
    assert_eq!(fs::read(bench.game().join("g.exe")).unwrap(), b"1");
    // Nothing was written, so the title is not held back from launching.
    assert!(!bench.paths().record(APPID).exists());
}

#[tokio::test]
async fn stale_removal_skips_a_path_through_a_link_out_of_the_install() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    let outside = bench.dir.path().join("Documents");
    fs::create_dir_all(&outside).unwrap();
    bench.session.add_manifest(
        DEPOT,
        manifest(
            V1,
            vec![
                file(chunks, "mods\\a.txt", &[b"a"]),
                file(chunks, "g.exe", &[b"1"]),
            ],
        ),
    );
    bench.install(&plan(V1, 11)).await.unwrap();
    fs::rename(bench.game().join("mods"), outside.join("mods")).unwrap();
    std::os::unix::fs::symlink(outside.join("mods"), bench.game().join("mods")).unwrap();

    bench
        .session
        .add_manifest(DEPOT, manifest(V2, vec![file(chunks, "g.exe", &[b"2"])]));
    let outcome = bench
        .installer(&NOT_RUNNING)
        .update(&plan(V2, 12))
        .await
        .unwrap();

    assert_eq!(outcome.files_removed, 0);
    assert_eq!(fs::read(outside.join("mods/a.txt")).unwrap(), b"a");
}

#[tokio::test]
async fn a_link_that_stays_inside_the_install_is_written_through() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    bench.session.add_manifest(
        DEPOT,
        manifest(
            V1,
            vec![
                file(chunks, "real\\b.txt", &[b"b1"]),
                file(chunks, "mods\\c.txt", &[b"c1"]),
            ],
        ),
    );
    bench.install(&plan(V1, 11)).await.unwrap();
    fs::remove_dir_all(bench.game().join("mods")).unwrap();
    std::os::unix::fs::symlink("real", bench.game().join("mods")).unwrap();

    bench.session.add_manifest(
        DEPOT,
        manifest(
            V2,
            vec![
                file(chunks, "real\\b.txt", &[b"b1"]),
                file(chunks, "mods\\c.txt", &[b"c2"]),
            ],
        ),
    );
    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V2, 12))
        .await
        .unwrap();
    assert_eq!(fs::read(bench.game().join("real/c.txt")).unwrap(), b"c2");
}

// From the install-safety review, probe p5b.
#[tokio::test]
async fn a_symlinked_install_dir_on_a_new_title_is_refused() {
    let bench = Bench::new();
    let outside = bench.dir.path().join("Important");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("data.txt"), b"mine").unwrap();
    fs::create_dir_all(bench.paths().root().join("steamapps/common")).unwrap();
    std::os::unix::fs::symlink(&outside, bench.game()).unwrap();
    let chunks = &bench.session.chunks;
    bench.session.add_manifest(
        DEPOT,
        manifest(V1, vec![file(chunks, "data.txt", &[b"game"])]),
    );

    let error = bench.install(&plan(V1, 11)).await.unwrap_err();

    assert!(matches!(error, Error::LinkOutOfInstall { .. }), "{error}");
    assert_eq!(fs::read(outside.join("data.txt")).unwrap(), b"mine");
}

#[tokio::test]
async fn an_installed_title_moved_to_another_disk_keeps_updating() {
    let bench = Bench::new();
    bench.add_v1();
    bench.install(&plan(V1, 11)).await.unwrap();
    let moved = bench.dir.path().join("Other Disk");
    fs::rename(bench.game(), &moved).unwrap();
    std::os::unix::fs::symlink(&moved, bench.game()).unwrap();

    bench.add_v2();
    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V2, 12))
        .await
        .unwrap();
    assert!(moved.join("new.txt").exists());
    assert!(!moved.join("old/gone.txt").exists());
}

// From the install-safety review, probe p8.
#[tokio::test]
async fn a_file_that_becomes_a_directory_lands() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    bench.session.add_manifest(
        DEPOT,
        manifest(
            V1,
            vec![
                file(chunks, "x", &[b"file"]),
                file(chunks, "g.exe", &[b"1"]),
            ],
        ),
    );
    bench.install(&plan(V1, 11)).await.unwrap();
    bench.session.add_manifest(
        DEPOT,
        manifest(
            V2,
            vec![
                file(chunks, "g.exe", &[b"2"]),
                file(chunks, "x\\y", &[b"now a dir"]),
            ],
        ),
    );
    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V2, 12))
        .await
        .unwrap();
    assert_eq!(fs::read(bench.game().join("x/y")).unwrap(), b"now a dir");
}

#[tokio::test]
async fn a_directory_that_becomes_a_file_lands() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    bench.session.add_manifest(
        DEPOT,
        manifest(V1, vec![file(chunks, "x\\y", &[b"in a dir"])]),
    );
    bench.install(&plan(V1, 11)).await.unwrap();
    bench.session.add_manifest(
        DEPOT,
        manifest(V2, vec![file(chunks, "x", &[b"now a file"])]),
    );
    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V2, 12))
        .await
        .unwrap();
    assert_eq!(fs::read(bench.game().join("x")).unwrap(), b"now a file");
}

// From the install-safety review, probe p12.
#[tokio::test]
async fn a_stored_manifest_gets_the_same_path_check() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    let stored = manifest(V1, vec![file(chunks, "..\\..\\..\\pwned.txt", &[b"x"])]);
    let path = bench.paths().manifest(DEPOT, V1);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();

    assert!(bench.install(&plan(V1, 11)).await.is_err());
    assert!(!bench.paths().root().join("pwned.txt").exists());
    assert_eq!(bench.session.chunks.fetches(), 0);
}

// From the install-safety review, probe p11.
#[tokio::test]
async fn verify_refuses_an_installdir_that_leaves_the_library() {
    let bench = Bench::new();
    bench.add_v1();
    bench.install(&plan(V1, 11)).await.unwrap();
    let path = bench.paths().acf(APPID);
    let text = fs::read_to_string(&path)
        .unwrap()
        .replace("Synthetic Game", "../../..");
    fs::write(&path, text).unwrap();
    assert!(matches!(
        verify_install(&bench.paths(), bench.data(), APPID),
        Err(Error::UnsafePath { .. })
    ));
}

// From the install-safety review, probe p10: `DepotJob` takes an empty file
// on its size, whatever hash the manifest carries.
#[tokio::test]
async fn verify_takes_an_empty_file_on_its_size() {
    let bench = Bench::new();
    let mut empty = ManifestFile::new("empty.txt".to_owned(), 0);
    empty.sha_content = Some([0; 20]);
    bench.session.add_manifest(DEPOT, manifest(V1, vec![empty]));
    bench.install(&plan(V1, 11)).await.unwrap();
    assert!(
        verify_install(&bench.paths(), bench.data(), APPID)
            .unwrap()
            .is_clean()
    );
    fs::write(bench.game().join("empty.txt"), b"x").unwrap();
    let report = verify_install(&bench.paths(), bench.data(), APPID).unwrap();
    assert_eq!(report.mismatched, ["empty.txt"]);
}

#[tokio::test]
async fn an_update_after_one_to_another_target_stopped_rewrites_what_it_left() {
    const V3: u64 = 1003;
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    let original = file(chunks, "a.txt", &[b"original"]);
    let other = file(chunks, "b.txt", &[b"other"]);
    bench
        .session
        .add_manifest(DEPOT, manifest(V1, vec![original.clone(), other.clone()]));
    bench.install(&plan(V1, 11)).await.unwrap();

    // The stopped update got as far as writing a.txt for V2.
    let changed = file(chunks, "a.txt", &[b"interim!"]);
    let failing = file(chunks, "b.txt", &[b"fails"]);
    chunks.fault(
        &chunk_of(&failing, 0),
        Fault::Fail(io::ErrorKind::ConnectionReset),
    );
    bench
        .session
        .add_manifest(DEPOT, manifest(V2, vec![changed, failing]));
    assert!(
        bench
            .installer(&NOT_RUNNING)
            .update(&plan(V2, 12))
            .await
            .is_err()
    );
    assert_eq!(fs::read(bench.game().join("a.txt")).unwrap(), b"interim!");

    // V3 equals V1 for a.txt, so a diff against the `.acf` would skip it.
    bench.session.add_manifest(
        DEPOT,
        manifest(V3, vec![original, file(chunks, "b.txt", &[b"third"])]),
    );
    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V3, 13))
        .await
        .unwrap();
    assert_eq!(fs::read(bench.game().join("a.txt")).unwrap(), b"original");
    assert!(
        verify_install(&bench.paths(), bench.data(), APPID)
            .unwrap()
            .is_clean()
    );
}

#[tokio::test]
async fn a_stopped_rescan_stays_a_rescan_when_resumed() {
    const V3: u64 = 1003;
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    let original = file(chunks, "a.txt", &[b"original"]);
    bench.session.add_manifest(
        DEPOT,
        manifest(
            V1,
            vec![original.clone(), file(chunks, "b.txt", &[b"other"])],
        ),
    );
    bench.install(&plan(V1, 11)).await.unwrap();

    let failing = file(chunks, "b.txt", &[b"fails"]);
    chunks.fault(
        &chunk_of(&failing, 0),
        Fault::Fail(io::ErrorKind::ConnectionReset),
    );
    bench.session.add_manifest(
        DEPOT,
        manifest(V2, vec![file(chunks, "a.txt", &[b"interim!"]), failing]),
    );
    assert!(
        bench
            .installer(&NOT_RUNNING)
            .update(&plan(V2, 12))
            .await
            .is_err()
    );

    // The rescan for V3 stops on b.txt, listed first, before reaching a.txt.
    let third = file(chunks, "b.txt", &[b"third"]);
    chunks.clear_faults();
    chunks.fault(
        &chunk_of(&third, 0),
        Fault::Fail(io::ErrorKind::ConnectionReset),
    );
    bench
        .session
        .add_manifest(DEPOT, manifest(V3, vec![third, original]));
    assert!(
        bench
            .installer(&NOT_RUNNING)
            .update(&plan(V3, 13))
            .await
            .is_err()
    );
    assert!(bench.record().rescan);
    assert_eq!(fs::read(bench.game().join("a.txt")).unwrap(), b"interim!");

    chunks.clear_faults();
    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V3, 13))
        .await
        .unwrap();
    assert_eq!(fs::read(bench.game().join("a.txt")).unwrap(), b"original");
}

fn fail_on(bench: &Bench, entry: &ManifestFile) {
    bench.session.chunks.fault(
        &chunk_of(entry, 0),
        Fault::Fail(io::ErrorKind::ConnectionReset),
    );
}

fn two_depot_plan(first: u64, second: u64, build: u64) -> InstallPlan {
    let info = product_info(first, build);
    let text = info.text.as_ref().unwrap().replace(
        "\"branches\"",
        &format!(
            "\"{OTHER}\"\n{{\n\"manifests\"\n{{\n\"public\"\n{{\n\"gid\" \"{second}\"\n\"size\" \"1\"\n}}\n}}\n}}\n\"branches\""
        ),
    );
    let filter = licensed();
    let info = AppInfo {
        text: Some(text),
        ..info
    };
    let app = OwnedApp::parse(&info, &filter).unwrap();
    InstallPlan::new(&app, "public", &filter).unwrap()
}

// From the install-safety review, probe p1: the stopped update never moved
// the `.acf`, so a target equal to it used to look like nothing to do.
#[tokio::test]
async fn a_rollback_after_a_stopped_update_repairs_the_files() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    bench.session.add_manifest(
        DEPOT,
        manifest(
            V1,
            vec![
                file(chunks, "a.txt", &[b"original"]),
                file(chunks, "b.txt", &[b"other"]),
            ],
        ),
    );
    bench.install(&plan(V1, 11)).await.unwrap();
    let failing = file(chunks, "b.txt", &[b"fails"]);
    fail_on(&bench, &failing);
    bench.session.add_manifest(
        DEPOT,
        manifest(V2, vec![file(chunks, "a.txt", &[b"interim!"]), failing]),
    );
    assert!(
        bench
            .installer(&NOT_RUNNING)
            .update(&plan(V2, 12))
            .await
            .is_err()
    );
    assert_eq!(fs::read(bench.game().join("a.txt")).unwrap(), b"interim!");

    let outcome = bench
        .installer(&NOT_RUNNING)
        .update(&plan(V1, 11))
        .await
        .unwrap();

    assert_eq!(outcome.changed_depots, [DEPOT]);
    assert_eq!(fs::read(bench.game().join("a.txt")).unwrap(), b"original");
    assert!(!bench.paths().record(APPID).exists());
    assert!(
        verify_install(&bench.paths(), bench.data(), APPID)
            .unwrap()
            .is_clean()
    );
    // The manifest the run just installed survives the end-of-run cleanup,
    // and the abandoned target's goes.
    assert!(bench.paths().manifest(DEPOT, V1).exists());
    assert!(!bench.paths().manifest(DEPOT, V2).exists());
}

// From the install-safety review, probe p2.
#[tokio::test]
async fn a_depot_back_at_its_acf_manifest_is_repaired_with_the_rest() {
    const V3: u64 = 1003;
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    bench.session.add_manifest(
        DEPOT,
        manifest(V1, vec![file(chunks, "a.txt", &[b"a one"])]),
    );
    bench.session.add_manifest(
        OTHER,
        manifest(
            70,
            vec![
                file(chunks, "b1.txt", &[b"b1 old"]),
                file(chunks, "b2.txt", &[b"b2 old"]),
            ],
        ),
    );
    bench.install(&two_depot_plan(V1, 70, 11)).await.unwrap();

    bench.session.add_manifest(
        DEPOT,
        manifest(V2, vec![file(chunks, "a.txt", &[b"a two"])]),
    );
    let failing = file(chunks, "b2.txt", &[b"b2 mid"]);
    fail_on(&bench, &failing);
    bench.session.add_manifest(
        OTHER,
        manifest(71, vec![file(chunks, "b1.txt", &[b"b1 mid"]), failing]),
    );
    assert!(
        bench
            .installer(&NOT_RUNNING)
            .update(&two_depot_plan(V2, 71, 12))
            .await
            .is_err()
    );
    assert_eq!(fs::read(bench.game().join("b1.txt")).unwrap(), b"b1 mid");

    bench.session.add_manifest(
        DEPOT,
        manifest(V3, vec![file(chunks, "a.txt", &[b"a three"])]),
    );
    let outcome = bench
        .installer(&NOT_RUNNING)
        .update(&two_depot_plan(V3, 70, 13))
        .await
        .unwrap();

    assert_eq!(outcome.changed_depots, [DEPOT, OTHER]);
    assert_eq!(fs::read(bench.game().join("b1.txt")).unwrap(), b"b1 old");
    assert!(!bench.paths().record(APPID).exists());
    assert!(
        verify_install(&bench.paths(), bench.data(), APPID)
            .unwrap()
            .is_clean()
    );
}

#[tokio::test]
async fn a_recorded_depot_the_plan_dropped_goes_back_to_its_acf_manifest() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    bench.session.add_manifest(
        DEPOT,
        manifest(V1, vec![file(chunks, "a.txt", &[b"a one"])]),
    );
    bench.session.add_manifest(
        OTHER,
        manifest(
            70,
            vec![
                file(chunks, "b.txt", &[b"b old"]),
                file(chunks, "c.txt", &[b"c old"]),
            ],
        ),
    );
    bench.install(&two_depot_plan(V1, 70, 11)).await.unwrap();
    let failing = file(chunks, "c.txt", &[b"c mid"]);
    fail_on(&bench, &failing);
    bench.session.add_manifest(
        OTHER,
        manifest(71, vec![file(chunks, "b.txt", &[b"b mid"]), failing]),
    );
    assert!(
        bench
            .installer(&NOT_RUNNING)
            .update(&two_depot_plan(V1, 71, 12))
            .await
            .is_err()
    );
    assert_eq!(fs::read(bench.game().join("b.txt")).unwrap(), b"b mid");

    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V1, 11))
        .await
        .unwrap();

    assert_eq!(fs::read(bench.game().join("b.txt")).unwrap(), b"b old");
    assert!(!bench.paths().record(APPID).exists());
    assert!(
        verify_install(&bench.paths(), bench.data(), APPID)
            .unwrap()
            .is_clean()
    );
}

#[tokio::test]
async fn a_recorded_depot_the_acf_does_not_list_is_refused_and_the_record_kept() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    bench.session.add_manifest(
        DEPOT,
        manifest(V1, vec![file(chunks, "a.txt", &[b"a one"])]),
    );
    bench.install(&plan(V1, 11)).await.unwrap();
    let failing = file(chunks, "b.txt", &[b"b new"]);
    fail_on(&bench, &failing);
    bench
        .session
        .add_manifest(OTHER, manifest(70, vec![failing]));
    assert!(
        bench
            .installer(&NOT_RUNNING)
            .update(&two_depot_plan(V1, 70, 12))
            .await
            .is_err()
    );

    let error = bench
        .installer(&NOT_RUNNING)
        .update(&plan(V1, 11))
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            Error::UnfinishedDepot {
                appid: APPID,
                depot_id: OTHER,
                ..
            }
        ),
        "{error}"
    );
    assert!(bench.paths().record(APPID).exists());
}

/// Runs an update to `V2` and expects it to stop. Files land in list order, so
/// where the caller puts the failing one decides what lands first.
async fn stop_update_to_v2(bench: &Bench, files: Vec<ManifestFile>) {
    bench.session.add_manifest(DEPOT, manifest(V2, files));
    assert!(
        bench
            .installer(&NOT_RUNNING)
            .update(&plan(V2, 12))
            .await
            .is_err()
    );
}

// From the install-safety review round two, probe r6d: V2 adds a path the
// user already has a file at, and stops before reaching it.
#[tokio::test]
async fn a_users_file_at_a_path_the_stopped_run_never_reached_survives_a_rollback() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    bench
        .session
        .add_manifest(DEPOT, manifest(V1, vec![file(chunks, "a.txt", &[b"one"])]));
    bench.install(&plan(V1, 11)).await.unwrap();
    let mine = bench.game().join("mods/mine.pak");
    fs::create_dir_all(mine.parent().unwrap()).unwrap();
    fs::write(&mine, b"user mod").unwrap();

    let failing = file(chunks, "a-first.txt", &[b"fails"]);
    fail_on(&bench, &failing);
    stop_update_to_v2(
        &bench,
        vec![
            failing,
            file(chunks, "a.txt", &[b"two"]),
            file(chunks, "mods/mine.pak", &[b"official"]),
        ],
    )
    .await;
    assert_eq!(fs::read(&mine).unwrap(), b"user mod");

    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V1, 11))
        .await
        .unwrap();
    assert_eq!(fs::read(&mine).unwrap(), b"user mod");
    assert_eq!(fs::read(bench.game().join("a.txt")).unwrap(), b"one");
    assert!(!bench.paths().record(APPID).exists());
}

// From the install-safety review round two, probe r6b: the depot's copy of
// the config never changed for this user, since V2 did not reach it.
#[tokio::test]
async fn a_versioned_edit_the_stopped_run_never_reached_survives_a_rollback() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    let config = flagged(file(chunks, "v.cfg", &[b"default"]), VERSIONED_USER_CONFIG);
    bench.session.add_manifest(
        DEPOT,
        manifest(V1, vec![config, file(chunks, "a.txt", &[b"one"])]),
    );
    bench.install(&plan(V1, 11)).await.unwrap();
    let edited = bench.game().join("v.cfg");
    fs::write(&edited, b"user edit").unwrap();

    let newer = flagged(
        file(chunks, "v.cfg", &[b"default two"]),
        VERSIONED_USER_CONFIG,
    );
    let failing = file(chunks, "a-first.txt", &[b"fails"]);
    fail_on(&bench, &failing);
    stop_update_to_v2(
        &bench,
        vec![failing, file(chunks, "a.txt", &[b"two"]), newer],
    )
    .await;
    assert_eq!(fs::read(&edited).unwrap(), b"user edit");

    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V1, 11))
        .await
        .unwrap();
    assert_eq!(fs::read(&edited).unwrap(), b"user edit");
    assert!(!bench.paths().record(APPID).exists());
}

// A file the stopped run did land stops being the run's once it is edited.
#[tokio::test]
async fn a_file_edited_after_the_stopped_run_landed_it_stays() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    bench
        .session
        .add_manifest(DEPOT, manifest(V1, vec![file(chunks, "a.txt", &[b"one"])]));
    bench.install(&plan(V1, 11)).await.unwrap();

    let failing = file(chunks, "z.txt", &[b"fails"]);
    fail_on(&bench, &failing);
    stop_update_to_v2(
        &bench,
        vec![
            file(chunks, "a.txt", &[b"one"]),
            file(chunks, "edited.ini", &[b"as shipped"]),
            file(chunks, "untouched.ini", &[b"as shipped too"]),
            failing,
        ],
    )
    .await;
    let edited = bench.game().join("edited.ini");
    assert_eq!(fs::read(&edited).unwrap(), b"as shipped");
    assert!(bench.game().join("untouched.ini").exists());
    fs::write(&edited, b"user edit").unwrap();

    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V1, 11))
        .await
        .unwrap();
    assert_eq!(fs::read(&edited).unwrap(), b"user edit");
    assert!(
        !bench.game().join("untouched.ini").exists(),
        "what still matches is the run's, and goes"
    );
}

#[tokio::test]
async fn a_versioned_config_edited_after_the_stopped_run_landed_it_stays() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    let config = flagged(file(chunks, "v.cfg", &[b"default"]), VERSIONED_USER_CONFIG);
    bench
        .session
        .add_manifest(DEPOT, manifest(V1, vec![config]));
    bench.install(&plan(V1, 11)).await.unwrap();

    let newer = flagged(
        file(chunks, "v.cfg", &[b"default two"]),
        VERSIONED_USER_CONFIG,
    );
    let failing = file(chunks, "z.txt", &[b"fails"]);
    fail_on(&bench, &failing);
    stop_update_to_v2(&bench, vec![newer, failing]).await;
    let edited = bench.game().join("v.cfg");
    assert_eq!(fs::read(&edited).unwrap(), b"default two");
    fs::write(&edited, b"user edit").unwrap();

    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V1, 11))
        .await
        .unwrap();
    assert_eq!(fs::read(&edited).unwrap(), b"user edit");
}

// `DepotJob` skips a link entry with no target, so it cannot have landed, and
// a comparison of two missing targets must not read as a match.
#[tokio::test]
async fn a_link_entry_with_no_target_does_not_claim_a_users_file() {
    const SYMLINK: u32 = 512;
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    bench
        .session
        .add_manifest(DEPOT, manifest(V1, vec![file(chunks, "a.txt", &[b"one"])]));
    bench.install(&plan(V1, 11)).await.unwrap();
    let mine = bench.game().join("mine.pak");
    fs::write(&mine, b"user mod").unwrap();

    let failing = file(chunks, "a-first.txt", &[b"fails"]);
    fail_on(&bench, &failing);
    let link = flagged(file(chunks, "mine.pak", &[b"official"]), SYMLINK);
    assert!(link.link_target.is_none());
    stop_update_to_v2(&bench, vec![failing, link]).await;

    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V1, 11))
        .await
        .unwrap();
    assert_eq!(fs::read(&mine).unwrap(), b"user mod");
}

// From the install-safety review round three, probe s4: an error reading a
// file proves nothing about whose it is.
#[tokio::test]
async fn an_abandoned_file_that_cannot_be_read_stays() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    bench
        .session
        .add_manifest(DEPOT, manifest(V1, vec![file(chunks, "a.txt", &[b"one"])]));
    bench.install(&plan(V1, 11)).await.unwrap();

    let failing = file(chunks, "zz.txt", &[b"fails"]);
    fail_on(&bench, &failing);
    stop_update_to_v2(
        &bench,
        vec![
            file(chunks, "a.txt", &[b"one"]),
            file(chunks, "x.pak", &[b"only in v2"]),
            failing,
        ],
    )
    .await;
    let path = bench.game().join("x.pak");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(fs::File::open(&path).is_err(), "needs a non-root run");

    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V1, 11))
        .await
        .unwrap();
    assert!(path.exists());
}

// From the install-safety review round three, probe s19.
#[tokio::test]
async fn a_users_file_at_an_abandoned_directory_entry_stays() {
    const DIRECTORY: u32 = 64;
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    bench
        .session
        .add_manifest(DEPOT, manifest(V1, vec![file(chunks, "a.txt", &[b"one"])]));
    bench.install(&plan(V1, 11)).await.unwrap();
    let mine = bench.game().join("dirent");
    fs::write(&mine, b"mine").unwrap();

    let failing = file(chunks, "0fail.txt", &[b"fails"]);
    fail_on(&bench, &failing);
    stop_update_to_v2(
        &bench,
        vec![
            failing,
            file(chunks, "a.txt", &[b"one"]),
            flagged(ManifestFile::new("dirent".to_owned(), 0), DIRECTORY),
        ],
    )
    .await;

    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V1, 11))
        .await
        .unwrap();
    assert_eq!(fs::read(&mine).unwrap(), b"mine");
}

// From the install-safety review round two, probe r5e: the disk a moved game
// sits on is not mounted. A record would gate a title nothing was written to.
#[tokio::test]
async fn a_dangling_install_directory_link_leaves_no_record() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    bench
        .session
        .add_manifest(DEPOT, manifest(V1, vec![file(chunks, "a.txt", &[b"one"])]));
    bench.install(&plan(V1, 11)).await.unwrap();
    fs::remove_dir_all(bench.game()).unwrap();
    std::os::unix::fs::symlink("/nonexistent/applejack-volume", bench.game()).unwrap();
    bench
        .session
        .add_manifest(DEPOT, manifest(V2, vec![file(chunks, "a.txt", &[b"two"])]));

    let error = bench
        .installer(&NOT_RUNNING)
        .update(&plan(V2, 12))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Io { .. }), "{error}");
    assert!(!bench.paths().record(APPID).exists());
}

/// Installs `text.pak` from `DEPOT` and stops an update of `OTHER` that had
/// already laid an overlay of it. `OTHER` is at manifest 70 in the `.acf`.
async fn stop_after_an_abandoned_overlay(bench: &Bench) {
    let chunks = &bench.session.chunks;
    bench.session.add_manifest(
        DEPOT,
        manifest(V1, vec![file(chunks, "text.pak", &[b"base"])]),
    );
    bench.session.add_manifest(
        OTHER,
        manifest(70, vec![file(chunks, "other.txt", &[b"other"])]),
    );
    bench.install(&two_depot_plan(V1, 70, 11)).await.unwrap();
    let failing = file(chunks, "z.txt", &[b"fails"]);
    fail_on(bench, &failing);
    bench.session.add_manifest(
        OTHER,
        manifest(71, vec![file(chunks, "text.pak", &[b"overlay"]), failing]),
    );
    assert!(
        bench
            .installer(&NOT_RUNNING)
            .update(&two_depot_plan(V1, 71, 12))
            .await
            .is_err()
    );
    assert_eq!(fs::read(bench.game().join("text.pak")).unwrap(), b"overlay");
}

// From this unit's own review, probes p1_abandoned_overlay_kept_heir and
// p6_rollback_overlay_heir: the kept depot owns the path, so nothing looked
// at what the stopped run had left there.
#[tokio::test]
async fn an_abandoned_overlay_goes_when_the_target_moves_on() {
    let bench = Bench::new();
    stop_after_an_abandoned_overlay(&bench).await;
    let chunks = &bench.session.chunks;
    bench.session.add_manifest(
        OTHER,
        manifest(72, vec![file(chunks, "other.txt", &[b"other 2"])]),
    );
    bench
        .installer(&NOT_RUNNING)
        .update(&two_depot_plan(V1, 72, 13))
        .await
        .unwrap();
    assert_eq!(fs::read(bench.game().join("text.pak")).unwrap(), b"base");
    assert!(
        verify_install(&bench.paths(), bench.data(), APPID)
            .unwrap()
            .is_clean()
    );
}

#[tokio::test]
async fn an_abandoned_overlay_goes_on_a_rollback() {
    let bench = Bench::new();
    stop_after_an_abandoned_overlay(&bench).await;
    bench
        .installer(&NOT_RUNNING)
        .update(&two_depot_plan(V1, 70, 11))
        .await
        .unwrap();
    assert_eq!(fs::read(bench.game().join("text.pak")).unwrap(), b"base");
    assert!(!bench.paths().record(APPID).exists());
}

// From this unit's own review, probe p2_unreadable_record_unplanned_depot.
#[tokio::test]
async fn an_unreadable_record_also_rechecks_depots_the_plan_dropped() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    bench.session.add_manifest(
        DEPOT,
        manifest(V1, vec![file(chunks, "a.txt", &[b"a one"])]),
    );
    bench.session.add_manifest(
        OTHER,
        manifest(
            70,
            vec![
                file(chunks, "b1.txt", &[b"b1 old"]),
                file(chunks, "b2.txt", &[b"b2 old"]),
            ],
        ),
    );
    bench.install(&two_depot_plan(V1, 70, 11)).await.unwrap();
    let failing = file(chunks, "b2.txt", &[b"b2 mid"]);
    fail_on(&bench, &failing);
    bench.session.add_manifest(
        OTHER,
        manifest(71, vec![file(chunks, "b1.txt", &[b"b1 mid"]), failing]),
    );
    assert!(
        bench
            .installer(&NOT_RUNNING)
            .update(&two_depot_plan(V1, 71, 12))
            .await
            .is_err()
    );
    fs::write(bench.paths().record(APPID), b"not json").unwrap();

    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V1, 11))
        .await
        .unwrap();

    assert_eq!(fs::read(bench.game().join("b1.txt")).unwrap(), b"b1 old");
    assert!(
        verify_install(&bench.paths(), bench.data(), APPID)
            .unwrap()
            .is_clean()
    );
}

#[tokio::test]
async fn a_versioned_config_a_stopped_run_replaced_goes_back_on_a_rollback() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    let config = flagged(file(chunks, "c.ini", &[b"one"]), VERSIONED_USER_CONFIG);
    bench.session.add_manifest(
        DEPOT,
        manifest(V1, vec![config, file(chunks, "b.txt", &[b"b"])]),
    );
    bench.install(&plan(V1, 11)).await.unwrap();
    let failing = file(chunks, "b.txt", &[b"fails"]);
    fail_on(&bench, &failing);
    let newer = flagged(file(chunks, "c.ini", &[b"two"]), VERSIONED_USER_CONFIG);
    bench
        .session
        .add_manifest(DEPOT, manifest(V2, vec![newer, failing]));
    assert!(
        bench
            .installer(&NOT_RUNNING)
            .update(&plan(V2, 12))
            .await
            .is_err()
    );
    assert_eq!(fs::read(bench.game().join("c.ini")).unwrap(), b"two");

    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V1, 11))
        .await
        .unwrap();
    assert_eq!(fs::read(bench.game().join("c.ini")).unwrap(), b"one");
}

// From the install-safety review, probe p30.
#[tokio::test]
async fn an_update_keeps_a_later_depots_overlay() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    bench.session.add_manifest(
        DEPOT,
        manifest(
            V1,
            vec![
                file(chunks, "text.pak", &[b"base one"]),
                file(chunks, "g.exe", &[b"1"]),
            ],
        ),
    );
    bench.session.add_manifest(
        OTHER,
        manifest(70, vec![file(chunks, "text.pak", &[b"overlay"])]),
    );
    bench.install(&two_depot_plan(V1, 70, 11)).await.unwrap();
    assert_eq!(fs::read(bench.game().join("text.pak")).unwrap(), b"overlay");

    bench.session.add_manifest(
        DEPOT,
        manifest(
            V2,
            vec![
                file(chunks, "text.pak", &[b"base two"]),
                file(chunks, "g.exe", &[b"2"]),
            ],
        ),
    );
    let outcome = bench
        .installer(&NOT_RUNNING)
        .update(&two_depot_plan(V2, 70, 12))
        .await
        .unwrap();

    assert_eq!(outcome.files_written, 1);
    assert_eq!(fs::read(bench.game().join("text.pak")).unwrap(), b"overlay");
    assert!(
        verify_install(&bench.paths(), bench.data(), APPID)
            .unwrap()
            .is_clean()
    );
}

#[tokio::test]
async fn a_dropped_overlay_restores_the_lower_depots_file() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    bench.session.add_manifest(
        DEPOT,
        manifest(V1, vec![file(chunks, "text.pak", &[b"base"])]),
    );
    bench.session.add_manifest(
        OTHER,
        manifest(70, vec![file(chunks, "text.pak", &[b"overlay"])]),
    );
    bench.install(&two_depot_plan(V1, 70, 11)).await.unwrap();

    bench.session.add_manifest(
        OTHER,
        manifest(71, vec![file(chunks, "other.txt", &[b"other"])]),
    );
    let outcome = bench
        .installer(&NOT_RUNNING)
        .update(&two_depot_plan(V1, 71, 12))
        .await
        .unwrap();

    assert_eq!(outcome.changed_depots, [DEPOT, OTHER]);
    assert_eq!(outcome.files_removed, 0);
    assert_eq!(fs::read(bench.game().join("text.pak")).unwrap(), b"base");
    assert!(bench.paths().manifest(DEPOT, V1).exists());
    assert!(
        verify_install(&bench.paths(), bench.data(), APPID)
            .unwrap()
            .is_clean()
    );
}

const USER_CONFIG: u32 = 1;
const VERSIONED_USER_CONFIG: u32 = 2;

fn flagged(mut entry: ManifestFile, flags: u32) -> ManifestFile {
    entry.flags = flags;
    entry
}

// From the install-safety review, probe p3.
#[tokio::test]
async fn user_config_survives_updates_rescans_and_verify() {
    const V3: u64 = 1003;
    const V4: u64 = 1004;
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    let exe = |text: &'static [u8]| file(chunks, "game.exe", &[text]);
    bench.session.add_manifest(
        DEPOT,
        manifest(
            V1,
            vec![
                flagged(file(chunks, "settings.ini", &[b"defaults"]), USER_CONFIG),
                flagged(file(chunks, "profile.cfg", &[b"defaults"]), USER_CONFIG),
                exe(b"v1"),
            ],
        ),
    );
    bench.install(&plan(V1, 11)).await.unwrap();
    assert_eq!(
        fs::read(bench.game().join("settings.ini")).unwrap(),
        b"defaults"
    );
    fs::write(bench.game().join("settings.ini"), b"users keybinds").unwrap();
    fs::write(bench.game().join("profile.cfg"), b"users profile").unwrap();
    assert!(
        verify_install(&bench.paths(), bench.data(), APPID)
            .unwrap()
            .is_clean()
    );

    // The depot's copy changes and profile.cfg leaves the depot.
    let ini = flagged(
        file(chunks, "settings.ini", &[b"new defaults"]),
        USER_CONFIG,
    );
    bench
        .session
        .add_manifest(DEPOT, manifest(V2, vec![ini.clone(), exe(b"v2")]));
    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V2, 12))
        .await
        .unwrap();
    assert_eq!(
        fs::read(bench.game().join("settings.ini")).unwrap(),
        b"users keybinds"
    );
    assert_eq!(
        fs::read(bench.game().join("profile.cfg")).unwrap(),
        b"users profile"
    );

    // A rescan, after a stop, hashes every file.
    let failing = exe(b"v3");
    fail_on(&bench, &failing);
    bench
        .session
        .add_manifest(DEPOT, manifest(V3, vec![ini.clone(), failing]));
    assert!(
        bench
            .installer(&NOT_RUNNING)
            .update(&plan(V3, 13))
            .await
            .is_err()
    );
    bench.session.chunks.clear_faults();
    bench
        .session
        .add_manifest(DEPOT, manifest(V4, vec![ini, exe(b"v4")]));
    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V4, 14))
        .await
        .unwrap();
    assert_eq!(
        fs::read(bench.game().join("settings.ini")).unwrap(),
        b"users keybinds"
    );

    // Gone from disk, verify says so.
    fs::remove_file(bench.game().join("settings.ini")).unwrap();
    let report = verify_install(&bench.paths(), bench.data(), APPID).unwrap();
    assert_eq!(report.missing, ["settings.ini"]);
}

#[tokio::test]
async fn versioned_config_is_replaced_only_when_the_depots_copy_changes() {
    const V3: u64 = 1003;
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    let exe = |text: &'static [u8]| file(chunks, "game.exe", &[text]);
    let config = flagged(file(chunks, "c.ini", &[b"one"]), VERSIONED_USER_CONFIG);
    bench
        .session
        .add_manifest(DEPOT, manifest(V1, vec![config.clone(), exe(b"v1")]));
    bench.install(&plan(V1, 11)).await.unwrap();
    fs::write(bench.game().join("c.ini"), b"users").unwrap();

    bench
        .session
        .add_manifest(DEPOT, manifest(V2, vec![config, exe(b"v2")]));
    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V2, 12))
        .await
        .unwrap();
    assert_eq!(fs::read(bench.game().join("c.ini")).unwrap(), b"users");

    let config = flagged(file(chunks, "c.ini", &[b"two"]), VERSIONED_USER_CONFIG);
    bench
        .session
        .add_manifest(DEPOT, manifest(V3, vec![config, exe(b"v3")]));
    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V3, 13))
        .await
        .unwrap();
    assert_eq!(fs::read(bench.game().join("c.ini")).unwrap(), b"two");
}

#[tokio::test]
async fn an_unreadable_record_rechecks_every_depot() {
    let bench = Bench::new();
    bench.add_v1();
    bench.install(&plan(V1, 11)).await.unwrap();
    fs::write(bench.game().join("data/level.pak"), b"LEVEL DATA").unwrap();
    let path = bench.paths().record(APPID);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"not json").unwrap();

    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V1, 11))
        .await
        .unwrap();

    assert!(!path.exists());
    assert!(
        verify_install(&bench.paths(), bench.data(), APPID)
            .unwrap()
            .is_clean()
    );
}

// From the install-safety review, probe p6: a file only the abandoned target
// had, which no later manifest would ever name again.
#[tokio::test]
async fn files_of_abandoned_targets_are_removed() {
    const V3: u64 = 1003;
    const V4: u64 = 1004;
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    bench
        .session
        .add_manifest(DEPOT, manifest(V1, vec![file(chunks, "a.txt", &[b"one"])]));
    bench.install(&plan(V1, 11)).await.unwrap();

    let failing = file(chunks, "z.txt", &[b"fails"]);
    fail_on(&bench, &failing);
    bench.session.add_manifest(
        DEPOT,
        manifest(
            V2,
            vec![
                file(chunks, "a.txt", &[b"one"]),
                file(chunks, "hijack.dll", &[b"interim dll"]),
                failing,
            ],
        ),
    );
    assert!(
        bench
            .installer(&NOT_RUNNING)
            .update(&plan(V2, 12))
            .await
            .is_err()
    );
    assert!(bench.game().join("hijack.dll").exists());

    // A second stop must not forget the first target's files.
    let failing = file(chunks, "y.txt", &[b"fails too"]);
    fail_on(&bench, &failing);
    bench
        .session
        .add_manifest(DEPOT, manifest(V3, vec![failing]));
    assert!(
        bench
            .installer(&NOT_RUNNING)
            .update(&plan(V3, 13))
            .await
            .is_err()
    );

    bench.session.chunks.clear_faults();
    bench
        .session
        .add_manifest(DEPOT, manifest(V4, vec![file(chunks, "a.txt", &[b"four"])]));
    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V4, 14))
        .await
        .unwrap();

    assert!(!bench.game().join("hijack.dll").exists());
    assert!(!bench.paths().manifest(DEPOT, V2).exists());
    assert!(!bench.paths().manifest(DEPOT, V3).exists());
}

#[tokio::test]
async fn update_at_the_current_manifest_writes_nothing() {
    let bench = Bench::new();
    bench.add_v1();
    bench.install(&plan(V1, 11)).await.unwrap();
    let text = fs::read_to_string(bench.paths().acf(APPID)).unwrap();
    bench.session.chunks.reset_counts();
    let outcome = bench
        .installer(&NOT_RUNNING)
        .update(&plan(V1, 11))
        .await
        .unwrap();
    assert_eq!(outcome.changed_depots, Vec::<u32>::new());
    assert_eq!(bench.session.chunks.fetches(), 0);
    assert_eq!(fs::read_to_string(bench.paths().acf(APPID)).unwrap(), text);
}

#[tokio::test]
async fn update_after_the_client_moved_the_title_diffs_from_what_the_acf_records() {
    let bench = Bench::new();
    bench.add_v1();
    bench.install(&plan(V1, 11)).await.unwrap();
    // The stored manifest is gone, as for a title the client updated, so the
    // old one is fetched to compute the delta.
    fs::remove_file(bench.paths().manifest(DEPOT, V1)).unwrap();
    bench.add_v2();
    bench.session.chunks.reset_counts();
    bench
        .installer(&NOT_RUNNING)
        .update(&plan(V2, 12))
        .await
        .unwrap();
    assert_eq!(bench.session.chunks.fetches(), 2);
}

#[tokio::test]
async fn update_refuses_unless_the_acf_reports_fully_installed() {
    let bench = Bench::new();
    bench.add_v1();
    assert!(matches!(
        bench.installer(&NOT_RUNNING).update(&plan(V1, 11)).await,
        Err(Error::NotInstalled { appid: APPID })
    ));

    bench.install(&plan(V1, 11)).await.unwrap();
    let path = bench.paths().acf(APPID);
    let text = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        text.replace("\"StateFlags\"\t\t\"4\"", "\"StateFlags\"\t\t\"6\""),
    )
    .unwrap();

    bench.add_v2();
    let error = bench
        .installer(&NOT_RUNNING)
        .update(&plan(V2, 12))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "app 100 has StateFlags 6; `applejack runtime steam` owns its update"
    );
    assert!(bench.game().join("old/gone.txt").exists());
}

#[tokio::test]
async fn verify_detects_a_changed_file_and_a_missing_one() {
    let bench = Bench::new();
    bench.add_v1();
    bench.install(&plan(V1, 11)).await.unwrap();

    let report = verify_install(&bench.paths(), bench.data(), APPID).unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.files_checked, 3);

    // Same length, so only the hash can tell.
    fs::write(bench.game().join("data/level.pak"), b"LEVEL DATA").unwrap();
    fs::remove_file(bench.game().join("old/gone.txt")).unwrap();
    let report = verify_install(&bench.paths(), bench.data(), APPID).unwrap();
    assert_eq!(report.mismatched, ["data/level.pak"]);
    assert_eq!(report.missing, ["old/gone.txt"]);
    assert!(!report.is_clean());
}

#[tokio::test]
async fn verify_needs_the_stored_manifest() {
    let bench = Bench::new();
    bench.add_v1();
    assert!(matches!(
        verify_install(&bench.paths(), bench.data(), APPID),
        Err(Error::NotInstalled { .. })
    ));
    bench.install(&plan(V1, 11)).await.unwrap();
    fs::remove_file(bench.paths().manifest(DEPOT, V1)).unwrap();
    assert!(matches!(
        verify_install(&bench.paths(), bench.data(), APPID),
        Err(Error::ManifestNotStored {
            depot_id: DEPOT,
            ..
        })
    ));
}

#[tokio::test]
async fn a_held_lock_fails_fast_naming_the_holder() {
    let bench = Bench::new();
    bench.add_v1();
    let mut holder = LockDir::new(bench.data(), "run");
    let held = holder.app(APPID, Mode::NonBlocking).unwrap();

    let error = bench.install(&plan(V1, 11)).await.unwrap_err();
    let Error::LockHeld { name, holder } = &error else {
        panic!("{error}");
    };
    assert_eq!(*name, app_lock_name(APPID));
    let holder = holder.as_ref().unwrap();
    assert_eq!(holder.pid, std::process::id());
    assert_eq!(holder.command, "run");
    assert!(error.to_string().contains("`run`"));
    assert!(!bench.game().exists());
    assert_eq!(bench.session.manifest_calls.load(Ordering::Relaxed), 0);
    drop(held);

    let mut other = LockDir::new(bench.data(), "run");
    let library = other.library_exclusive(Mode::NonBlocking).unwrap();
    assert!(matches!(
        bench.install(&plan(V1, 11)).await,
        Err(Error::LockHeld { .. })
    ));
    drop(library);
    bench.install(&plan(V1, 11)).await.unwrap();
}

#[tokio::test]
async fn nothing_is_written_while_a_client_is_running() {
    let bench = Bench::new();
    bench.add_v1();
    let running = || true;
    let result = bench.installer(&running).install(&plan(V1, 11)).await;
    assert!(matches!(result, Err(Error::ClientRunning { appid: APPID })));
    assert!(!bench.paths().root().exists());
    assert_eq!(bench.session.manifest_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn a_client_that_starts_mid_download_stops_the_run_with_its_record() {
    let bench = Bench::new();
    let files = bench.v1_files();
    // Long enough for the once-a-second check to come round mid-download.
    let slow = chunk_of(&files[2], 0);
    bench
        .session
        .chunks
        .fault(&slow, Fault::Slow(Duration::from_millis(2500)));
    bench.session.add_manifest(DEPOT, manifest(V1, files));
    let chunks = &bench.session.chunks;
    let running = || chunks.fetches() > 0;
    let result = bench.installer(&running).install(&plan(V1, 11)).await;
    assert!(matches!(result, Err(Error::ClientRunning { .. })));
    assert!(!bench.paths().acf(APPID).exists());
    assert!(bench.record().depots[0].files.contains("bin\\game.exe"));
    assert!(!bench.game().join("old/gone.txt").exists());

    bench.session.chunks.clear_faults();
    bench.install(&plan(V1, 11)).await.unwrap();
    assert!(bench.acf().is_fully_installed());
}

fn staged_bytes(bench: &Bench, name: &str) -> usize {
    let staged = bench.game().join(".DepotDownloader/staging").join(name);
    fs::read(staged).map_or(0, |bytes| bytes.iter().filter(|byte| **byte != 0).count())
}

// From the install-safety review, probe p15b.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_chunk_task_outlives_a_client_running_stop() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    let parts: Vec<Vec<u8>> = (0..6u8).map(|index| vec![b'a' + index; 32]).collect();
    let parts: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    let big = file(chunks, "big.bin", &parts);
    for chunk in &big.chunks {
        chunks.fault(&chunk.id, Fault::Slow(Duration::from_millis(700)));
    }
    bench.session.add_manifest(DEPOT, manifest(V1, vec![big]));
    let predicate = || bench.session.chunks.fetches() > 0;

    let result = bench.installer(&predicate).install(&plan(V1, 11)).await;
    assert!(matches!(result, Err(Error::ClientRunning { .. })));

    let bytes_at_return = staged_bytes(&bench, "big.bin");
    let fetches_at_return = bench.session.chunks.fetches();
    assert!(fetches_at_return > 0, "the stop came mid-download");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(staged_bytes(&bench, "big.bin"), bytes_at_return);
    assert_eq!(bench.session.chunks.fetches(), fetches_at_return);
}

// From the install-safety review, probe p15: the old task used to write V1
// bytes into the file the second run had finished and hash-checked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stopped_run_cannot_reach_the_file_of_a_later_run() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    let old = file(chunks, "big.bin", &[b"V1 V1 V1 V1 "]);
    chunks.fault(&chunk_of(&old, 0), Fault::Slow(Duration::from_millis(1800)));
    bench.session.add_manifest(DEPOT, manifest(V1, vec![old]));
    let predicate = || bench.session.chunks.fetches() > 0;
    let result = bench.installer(&predicate).install(&plan(V1, 11)).await;
    assert!(matches!(result, Err(Error::ClientRunning { .. })));
    assert_eq!(bench.session.chunks.fetches(), 1, "the stop came mid-fetch");

    let new = file(chunks, "big.bin", &[b"V2 V2 V2 V2 "]);
    bench.session.add_manifest(DEPOT, manifest(V2, vec![new]));
    bench.install(&plan(V2, 12)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;

    assert_eq!(
        fs::read(bench.game().join("big.bin")).unwrap(),
        b"V2 V2 V2 V2 "
    );
    assert!(
        verify_install(&bench.paths(), bench.data(), APPID)
            .unwrap()
            .is_clean()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_the_install_future_stops_its_chunk_tasks() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    let big = file(chunks, "big.bin", &[&[7; 32]]);
    chunks.fault(&chunk_of(&big, 0), Fault::Slow(Duration::from_millis(400)));
    bench.session.add_manifest(DEPOT, manifest(V1, vec![big]));

    let plan = plan(V1, 11);
    let installer = bench.installer(&NOT_RUNNING);
    let mut install = Box::pin(installer.install(&plan));
    let fetching = async {
        while bench.session.chunks.fetches() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::select! {
        result = &mut install => panic!("the slow chunk finished first: {result:?}"),
        () = fetching => {}
    }
    drop(install);

    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(staged_bytes(&bench, "big.bin"), 0);
    assert!(!bench.game().join("big.bin").exists());
    bench.session.chunks.clear_faults();
    bench.install(&plan).await.unwrap();
}

// From the install-safety review, probe p16.
#[tokio::test]
async fn a_client_that_starts_while_resolving_gets_no_writes() {
    let bench = Bench::new();
    bench.add_v1();
    let calls = AtomicUsize::new(0);
    let predicate = || calls.fetch_add(1, Ordering::Relaxed) >= 1;
    let result = bench.installer(&predicate).install(&plan(V1, 11)).await;
    assert!(matches!(result, Err(Error::ClientRunning { .. })));
    assert!(!bench.game().exists());
    assert_eq!(bench.session.chunks.fetches(), 0);
}

// From the install-safety review round three, S3: `running` turns true after
// the two checks ahead of the hashing in `download`, as a client started during
// it would.
#[tokio::test]
async fn a_client_that_starts_while_hashing_keeps_its_blockers() {
    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    bench.session.add_manifest(
        DEPOT,
        manifest(
            V1,
            vec![
                file(chunks, "x", &[b"file"]),
                file(chunks, "g.exe", &[b"1"]),
            ],
        ),
    );
    bench.install(&plan(V1, 11)).await.unwrap();
    bench.session.add_manifest(
        DEPOT,
        manifest(
            V2,
            vec![
                file(chunks, "g.exe", &[b"2"]),
                file(chunks, "x\\y", &[b"now a dir"]),
            ],
        ),
    );
    let calls = AtomicUsize::new(0);
    let running = || calls.fetch_add(1, Ordering::Relaxed) >= 2;
    let result = bench.installer(&running).update(&plan(V2, 12)).await;
    assert!(matches!(result, Err(Error::ClientRunning { .. })));
    assert_eq!(fs::read(bench.game().join("x")).unwrap(), b"file");
}

#[tokio::test]
async fn a_changed_manifest_is_refetched_once() {
    let bench = Bench::new();
    bench.add_v2();
    bench
        .session
        .app_info
        .lock()
        .unwrap()
        .push_back(vec![product_info(V2, 12)]);

    bench.install(&plan(V1, 11)).await.unwrap();
    assert_eq!(bench.session.app_info_calls.load(Ordering::Relaxed), 1);
    let acf = bench.acf();
    assert_eq!(acf.installed_depots[0].manifest_id, V2);
    assert_eq!(acf.buildid, 12);
}

#[tokio::test]
async fn a_refetch_moves_every_depot_to_the_new_build() {
    let two_depots = |first: u64, second: u64, build: u64| {
        let info = product_info(first, build);
        let text = info.text.as_ref().unwrap().replace(
            "\"branches\"",
            &format!(
                "\"{OTHER}\"\n{{\n\"manifests\"\n{{\n\"public\"\n{{\n\"gid\" \"{second}\"\n\"size\" \"1\"\n}}\n}}\n}}\n\"branches\""
            ),
        );
        AppInfo {
            text: Some(text),
            ..info
        }
    };
    let filter = licensed();
    let listed = OwnedApp::parse(&two_depots(V1, 70, 11), &filter).unwrap();
    let listed = InstallPlan::new(&listed, "public", &filter).unwrap();

    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    // Depot 101's old manifest is still served; depot 103's is not.
    bench.session.add_manifest(
        DEPOT,
        manifest(V1, vec![file(chunks, "old.txt", &[b"old"])]),
    );
    bench.session.add_manifest(
        DEPOT,
        manifest(V2, vec![file(chunks, "new.txt", &[b"new"])]),
    );
    bench
        .session
        .add_manifest(OTHER, manifest(71, vec![file(chunks, "o.txt", &[b"o"])]));
    bench
        .session
        .app_info
        .lock()
        .unwrap()
        .push_back(vec![two_depots(V2, 71, 12)]);

    bench.install(&listed).await.unwrap();
    let acf = bench.acf();
    assert_eq!(acf.buildid, 12);
    let installed: Vec<u64> = acf.installed_depots.iter().map(|d| d.manifest_id).collect();
    assert_eq!(installed, [V2, 71]);
    assert!(!bench.game().join("old.txt").exists());
}

#[tokio::test]
async fn a_manifest_that_is_gone_after_the_refetch_fails_with_both_ids() {
    let bench = Bench::new();
    bench
        .session
        .app_info
        .lock()
        .unwrap()
        .push_back(vec![product_info(V2, 12)]);

    let error = bench.install(&plan(V1, 11)).await.unwrap_err();
    assert!(matches!(
        error,
        Error::ManifestChanged {
            appid: APPID,
            depot_id: DEPOT,
            listed: V1,
            current: V2,
        }
    ));
    assert!(error.to_string().contains("1001"));
    assert!(error.to_string().contains("1002"));
    assert_eq!(bench.session.app_info_calls.load(Ordering::Relaxed), 1);
    assert_eq!(bench.session.manifest_calls.load(Ordering::Relaxed), 2);
    assert!(!bench.game().exists());
}

#[tokio::test]
async fn a_manifest_steam_lists_and_will_not_serve_is_not_a_changed_one() {
    let bench = Bench::new();
    bench
        .session
        .app_info
        .lock()
        .unwrap()
        .push_back(vec![product_info(V1, 11)]);
    let error = bench.install(&plan(V1, 11)).await.unwrap_err();
    assert!(
        matches!(
            error,
            Error::ManifestNotServed {
                appid: APPID,
                depot_id: DEPOT,
                manifest_id: V1,
            }
        ),
        "{error}"
    );
}

/// `DEPOT`, plus `OTHER` held by another app.
fn shared_depot_info() -> AppInfo {
    let info = product_info(V1, 11);
    let text = info.text.as_ref().unwrap().replace(
        "\"branches\"",
        &format!("\"{OTHER}\"\n{{\n\"depotfromapp\" \"{PARENT}\"\n}}\n\"branches\""),
    );
    AppInfo {
        text: Some(text),
        ..info
    }
}

fn parent_info() -> AppInfo {
    let text = format!(
        r#""appinfo"
{{
	"depots"
	{{
		"{OTHER}"
		{{
			"manifests"
			{{
				"public"
				{{
					"gid"		"70"
					"size"		"1"
				}}
			}}
		}}
	}}
}}
"#
    );
    AppInfo {
        appid: PARENT,
        change_number: 1,
        text: Some(text),
    }
}

fn grants() -> PackageGrants {
    PackageGrants {
        appids: [APPID].into(),
        depot_ids: [DEPOT, OTHER].into(),
    }
}

#[tokio::test]
async fn a_shared_depot_installs_through_the_app_that_holds_it() {
    let library = Library::from_app_info(&grants(), &[shared_depot_info(), parent_info()]);
    let app = library.windows_app(APPID).unwrap();
    let plan = InstallPlan::new(app, "public", &library.filter()).unwrap();
    assert_eq!(plan.depots[1].source_app, Some(PARENT));

    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    bench
        .session
        .add_manifest(DEPOT, manifest(V1, vec![file(chunks, "a.txt", &[b"a"])]));
    bench
        .session
        .add_manifest(OTHER, manifest(70, vec![file(chunks, "b.txt", &[b"b"])]));
    bench.install(&plan).await.unwrap();

    assert_eq!(
        *bench.session.manifest_apps.lock().unwrap(),
        [(DEPOT, APPID), (OTHER, PARENT)]
    );
    assert_eq!(*bench.session.chunk_apps.lock().unwrap(), [APPID, PARENT]);
    assert!(bench.game().join("b.txt").exists());
    assert_eq!(bench.acf().installed_depots.len(), 2);
    assert!(bench.acf().is_fully_installed());
}

#[tokio::test]
async fn a_refetch_resolves_a_shared_depot_again() {
    let library = Library::from_app_info(&grants(), &[shared_depot_info(), parent_info()]);
    let app = library.windows_app(APPID).unwrap();
    let filter = library.filter();
    let mut plan = InstallPlan::new(app, "public", &filter).unwrap();
    plan.depots[0].manifest_id = 999;

    let bench = Bench::new();
    let chunks = &bench.session.chunks;
    bench
        .session
        .add_manifest(DEPOT, manifest(V1, vec![file(chunks, "a.txt", &[b"a"])]));
    bench
        .session
        .add_manifest(OTHER, manifest(70, vec![file(chunks, "b.txt", &[b"b"])]));
    {
        let mut answers = bench.session.app_info.lock().unwrap();
        answers.push_back(vec![shared_depot_info()]);
        answers.push_back(vec![parent_info()]);
    }
    bench.install(&plan).await.unwrap();
    assert!(bench.game().join("b.txt").exists());
}

#[test]
fn a_shared_depot_with_no_manifest_refuses_the_install() {
    let library = Library::from_app_info(&grants(), &[shared_depot_info()]);
    let app = library.windows_app(APPID).unwrap();
    let error = InstallPlan::new(app, "public", &library.filter()).unwrap_err();
    assert!(
        matches!(
            error,
            Error::UnresolvedSharedDepot {
                appid: APPID,
                depot_id: OTHER,
                from_app: PARENT,
            }
        ),
        "{error}"
    );
    assert!(error.to_string().contains(&OTHER.to_string()));
    assert!(error.to_string().contains(&PARENT.to_string()));
}

#[test]
fn a_plan_reports_the_depots_it_left_out() {
    let filter = DepotFilter {
        owned_depots: [DEPOT].into(),
        ..DepotFilter::default()
    };
    let app = OwnedApp::parse(&shared_depot_info(), &filter).unwrap();
    let plan = InstallPlan::new(&app, "public", &filter).unwrap();
    assert_eq!(plan.depots.len(), 1);
    assert_eq!(plan.not_licensed, [OTHER]);
}

#[tokio::test]
async fn a_manifest_path_outside_the_install_is_refused() {
    for name in ["..\\..\\escape.txt", "/etc/escape.txt", "a\\..\\..\\b"] {
        let bench = Bench::new();
        let entry = file(&bench.session.chunks, name, &[b"payload"]);
        bench.session.add_manifest(DEPOT, manifest(V1, vec![entry]));
        let error = bench.install(&plan(V1, 11)).await.unwrap_err();
        assert!(matches!(error, Error::UnsafePath { .. }), "{name}: {error}");
        assert_eq!(bench.session.chunks.fetches(), 0);
    }
}

#[tokio::test]
async fn a_symlink_out_of_the_install_is_refused() {
    let bench = Bench::new();
    let mut entry = ManifestFile::new("link".to_owned(), 0);
    entry.link_target = Some("../../outside".to_owned());
    bench.session.add_manifest(DEPOT, manifest(V1, vec![entry]));
    assert!(matches!(
        bench.install(&plan(V1, 11)).await,
        Err(Error::UnsafePath { .. })
    ));
}

#[test]
fn a_branch_without_a_build_id_is_refused() {
    let filter = licensed();
    let mut app = OwnedApp::parse(&product_info(V1, 11), &filter).unwrap();
    app.builds.clear();
    assert!(matches!(
        InstallPlan::new(&app, "public", &filter),
        Err(Error::NoManifestForBranch { .. })
    ));
}

#[test]
fn an_installdir_that_is_not_one_name_is_refused() {
    for installdir in ["../escape", "/abs", "a/b", "a\\b", ".."] {
        let filter = licensed();
        let mut app = OwnedApp::parse(&product_info(V1, 11), &filter).unwrap();
        app.installdir = installdir.to_owned();
        assert!(
            matches!(
                InstallPlan::new(&app, "public", &filter),
                Err(Error::UnsafePath { .. })
            ),
            "{installdir}"
        );
    }
}

#[tokio::test]
async fn no_library_file_carries_a_secret() {
    let bench = Bench::new();
    bench.add_v1();
    bench.install(&plan(V1, 11)).await.unwrap();
    let key_hex = steamroom::util::hex::encode(&support::KEY.0);

    let mut pending = vec![bench.dir.path().to_owned()];
    let mut seen = 0;
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                seen += 1;
                let text = String::from_utf8_lossy(&fs::read(&path).unwrap()).into_owned();
                assert!(!text.contains(&key_hex), "{}", path.display());
                assert!(!text.contains("170, 170, 170, 170"), "{}", path.display());
                assert!(!text.contains("token"), "{}", path.display());
            }
        }
    }
    assert!(seen >= 5);
}

#[test]
fn the_install_future_can_run_on_a_spawned_task() {
    fn assert_send<T: Send>(_: T) {}
    let session = FakeSession::default();
    let installer = DepotInstaller::new(
        &session,
        LibraryPaths::new("/nonexistent"),
        "/nonexistent",
        &NOT_RUNNING,
    );
    let plan = plan(V1, 11);
    assert_send(installer.install(&plan));
    assert_send(installer.update(&plan));
}
