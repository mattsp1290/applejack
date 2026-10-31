//! A scripted session and an in-memory CDN. Every fixture here is synthetic.

use std::collections::{HashMap, VecDeque};
use std::future::{Future, ready};
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use applejack_steam::auth::{AuthError, License};
use applejack_steam::session::{AppInfo, SteamSession};
use bytes::Bytes;
use sha1::{Digest, Sha1};
use steamroom::crypto::{symmetric_encrypt_cbc, symmetric_encrypt_ecb_nopad};
use steamroom::depot::manifest::{DepotManifest, ManifestChunk, ManifestFile};
use steamroom::depot::{ChunkId, DepotId, DepotKey, ManifestId};
use steamroom::error::Error as SteamError;
use steamroom::util::checksum::SteamAdler32;
use steamroom_client::download::{BoxError, ChunkFetcher};

pub const KEY: DepotKey = DepotKey([0xAA; 32]);

/// What a chunk does when fetched.
pub enum Fault {
    Corrupt,
    Fail(io::ErrorKind),
    RateLimited,
    Slow(Duration),
}

#[derive(Default)]
pub struct FakeChunks {
    pub stored: Mutex<HashMap<ChunkId, Bytes>>,
    pub faults: Mutex<HashMap<ChunkId, Fault>>,
    pub fetched: Mutex<Vec<ChunkId>>,
}

impl FakeChunks {
    pub fn fetches(&self) -> usize {
        self.fetched.lock().unwrap().len()
    }

    pub fn fault(&self, id: &ChunkId, fault: Fault) {
        self.faults.lock().unwrap().insert(id.clone(), fault);
    }

    pub fn clear_faults(&self) {
        self.faults.lock().unwrap().clear();
    }

    pub fn reset_counts(&self) {
        self.fetched.lock().unwrap().clear();
    }
}

impl ChunkFetcher for FakeChunks {
    fn fetch_chunk(
        &self,
        _depot_id: DepotId,
        chunk_id: &ChunkId,
    ) -> impl Future<Output = Result<Bytes, BoxError>> + Send {
        self.fetched.lock().unwrap().push(chunk_id.clone());
        let data = self.stored.lock().unwrap().get(chunk_id).cloned().unwrap();
        let mut delay = None;
        let result = match self.faults.lock().unwrap().get(chunk_id) {
            None => Ok(data),
            Some(Fault::Corrupt) => {
                let mut bytes = data.to_vec();
                let middle = bytes.len() / 2;
                bytes[middle] ^= 0xFF;
                Ok(Bytes::from(bytes))
            }
            Some(Fault::Fail(kind)) => Err(Box::new(io::Error::from(*kind)) as BoxError),
            Some(Fault::RateLimited) => Err(Box::new(SteamError::CdnStatus {
                status: reqwest::StatusCode::TOO_MANY_REQUESTS,
                retry_after: Some(0),
            }) as BoxError),
            Some(Fault::Slow(duration)) => {
                delay = Some(*duration);
                Ok(data)
            }
        };
        async move {
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            result
        }
    }
}

/// Builds a manifest file and stores its encrypted chunks.
pub fn file(chunks: &FakeChunks, name: &str, parts: &[&[u8]]) -> ManifestFile {
    let mut entry = ManifestFile::new(
        name.to_owned(),
        parts.iter().map(|part| part.len() as u64).sum(),
    );
    let mut whole = Sha1::new();
    let mut offset = 0;
    for part in parts {
        whole.update(part);
        let id = ChunkId(Sha1::digest(part).into());
        let mut chunk = ManifestChunk::new(
            id.clone(),
            SteamAdler32::compute(part).0,
            u32::try_from(part.len()).unwrap(),
        );
        chunk.offset = Some(offset);
        offset += part.len() as u64;
        entry.chunks.push(chunk);

        let iv = [0x42u8; 16];
        let mut sealed = symmetric_encrypt_ecb_nopad(&iv, &KEY.0).unwrap();
        sealed.extend(symmetric_encrypt_cbc(part, &KEY.0, &iv).unwrap());
        chunks
            .stored
            .lock()
            .unwrap()
            .insert(id, Bytes::from(sealed));
    }
    entry.sha_content = Some(whole.finalize().into());
    entry
}

pub fn manifest(id: u64, files: Vec<ManifestFile>) -> DepotManifest {
    let mut manifest = DepotManifest::new(files);
    manifest.manifest_id = Some(ManifestId(id));
    manifest
}

/// `logon` output for the auth tests, and the session the depot tests script.
#[derive(Default)]
pub struct FakeSession {
    pub licenses: Vec<License>,
    pub appids: Vec<u32>,
    /// Answers to `app_info`, oldest first; the last one repeats.
    pub app_info: Mutex<VecDeque<Vec<AppInfo>>>,
    pub manifests: Mutex<HashMap<(u32, u64), DepotManifest>>,
    pub chunks: Arc<FakeChunks>,
    pub manifest_error: Mutex<Option<AuthError>>,
    pub app_info_calls: AtomicUsize,
    pub manifest_calls: AtomicUsize,
}

impl FakeSession {
    pub fn add_manifest(&self, depot_id: u32, manifest: DepotManifest) {
        let id = manifest.manifest_id.unwrap().0;
        self.manifests
            .lock()
            .unwrap()
            .insert((depot_id, id), manifest);
    }
}

impl SteamSession for FakeSession {
    type Chunks = FakeChunks;

    fn heartbeat_interval(&self) -> Option<Duration> {
        None
    }

    fn license_list(&self) -> impl Future<Output = Result<Vec<License>, AuthError>> + Send {
        ready(Ok(self.licenses.clone()))
    }

    fn package_apps(
        &self,
        _licenses: &[License],
    ) -> impl Future<Output = Result<Vec<u32>, AuthError>> + Send {
        ready(Ok(self.appids.clone()))
    }

    fn app_info(
        &self,
        _appids: &[u32],
    ) -> impl Future<Output = Result<Vec<AppInfo>, AuthError>> + Send {
        self.app_info_calls.fetch_add(1, Ordering::Relaxed);
        let mut answers = self.app_info.lock().unwrap();
        let answer = if answers.len() > 1 {
            answers.pop_front().unwrap()
        } else {
            answers.front().cloned().unwrap_or_default()
        };
        ready(Ok(answer))
    }

    fn depot_key(
        &self,
        _appid: u32,
        _depot_id: u32,
    ) -> impl Future<Output = Result<DepotKey, AuthError>> + Send {
        ready(Ok(KEY))
    }

    fn manifest(
        &self,
        _appid: u32,
        depot_id: u32,
        manifest_id: u64,
        _branch: &str,
    ) -> impl Future<Output = Result<Option<DepotManifest>, AuthError>> + Send {
        self.manifest_calls.fetch_add(1, Ordering::Relaxed);
        let error = self.manifest_error.lock().unwrap().clone();
        let found = self
            .manifests
            .lock()
            .unwrap()
            .get(&(depot_id, manifest_id))
            .cloned();
        ready(error.map_or(Ok(found), Err))
    }

    fn chunks(
        &self,
        _appid: u32,
        _depot_id: u32,
    ) -> impl Future<Output = Result<Arc<FakeChunks>, AuthError>> + Send {
        ready(Ok(Arc::clone(&self.chunks)))
    }
}
