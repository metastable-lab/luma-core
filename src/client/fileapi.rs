//! A blocking HTTP client for the Wi-Fi file API (§13). Feature `wifi-client`.
//!
//! Everything protocol-shaped comes from [`crate::fileapi`]: the host, the four URLs, the two
//! reply parsers and the KiB completeness rule. This file owns the socket and nothing else.
//!
//! ```no_run
//! use luma_core::client::fileapi::{ClientError, FileClient};
//!
//! fn main() -> Result<(), ClientError> {
//!     let c = FileClient::new();
//!     let list = c.list()?;
//!     for f in list.all_files() {
//!         let bytes = c.download(f)?; // verified against `size_kib` before it returns
//!         println!("{} {} bytes", f.name, bytes.len());
//!     }
//!     Ok(())
//! }
//! ```
//!
//! **The download check is the point.** [`FileClient::download`] refuses a body whose length is
//! outside [`crate::fileapi::expected_byte_range`] rather than handing back a truncated JPEG
//! that opens as a grey half-picture. A partial transfer is a real outcome on this AP — the
//! link is a 2.4 GHz SoftAP a metre from a phone that would rather be on the house network —
//! and the listing's own `size` is the only thing that can catch it.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::fileapi::{
    self, download_is_complete, expected_byte_range, DeleteReply, FileEntry, FileList, ParseError,
    SyncPlan,
};

/// Why a request did not produce what was asked for.
#[derive(Debug)]
pub enum ClientError {
    /// The socket, the DNS, the timeout — anything below HTTP. Usually "not joined to the
    /// glasses' AP".
    Transport { url: String, detail: String },
    /// A response that was not 2xx.
    Status { url: String, status: u16 },
    /// A 2xx body that did not parse.
    Parse { url: String, source: ParseError },
    /// A download whose length is outside the range the listing implies. See the module docs.
    ShortDownload {
        name: String,
        size_kib: u64,
        bytes: u64,
    },
    /// A local filesystem failure.
    Io { path: PathBuf, detail: String },
    /// The delete endpoint answered, and said no.
    DeleteRefused { name: String, reply: DeleteReply },
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Transport { url, detail } => write!(f, "{url}: {detail}"),
            ClientError::Status { url, status } => write!(f, "{url}: HTTP {status}"),
            ClientError::Parse { url, source } => write!(f, "{url}: {source}"),
            ClientError::ShortDownload {
                name,
                size_kib,
                bytes,
            } => {
                let r = expected_byte_range(*size_kib);
                write!(
                    f,
                    "{name}: got {bytes} bytes, a {size_kib} KiB listing means {}..={}",
                    r.start(),
                    r.end()
                )
            }
            ClientError::Io { path, detail } => write!(f, "{}: {detail}", path.display()),
            ClientError::DeleteRefused { name, reply } => {
                write!(
                    f,
                    "{name}: delete returned {} ({})",
                    reply.result, reply.info
                )
            }
        }
    }
}

impl std::error::Error for ClientError {}

/// What one [`FileClient::sync_all`] run did.
#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    /// Files written, in the order they landed.
    pub downloaded: Vec<PathBuf>,
    /// Names deleted from the glasses.
    pub deleted: Vec<String>,
    /// Bytes written.
    pub bytes: u64,
    /// Per-file failures. A sync does NOT stop on one bad file: the next one may be fine, and
    /// stopping leaves the AP up with the user staring at a spinner.
    pub failures: Vec<(String, String)>,
}

impl SyncReport {
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty()
    }
}

/// A blocking client for the glasses' file API.
///
/// One HTTP connection pool, a per-request timeout, and no state. Cheap to make and safe to
/// keep; the glasses' server closes connections aggressively (`Connection: close` on file
/// bodies) and `ureq` handles that.
#[derive(Debug, Clone)]
pub struct FileClient {
    host: String,
    agent: ureq::Agent,
    /// The ceiling on a single download body. 512 MiB — larger than any clip the glasses can
    /// hold, small enough that a wrong-network HTML page cannot exhaust memory.
    max_body_bytes: u64,
}

impl Default for FileClient {
    fn default() -> Self {
        FileClient::new()
    }
}

impl FileClient {
    /// A client for the glasses at their fixed address.
    pub fn new() -> Self {
        FileClient::with_host(fileapi::HOST)
    }

    /// A client for another host — a replay fixture, a proxy.
    pub fn with_host(host: &str) -> Self {
        let config = ureq::Agent::config_builder()
            // Generous: a 400 KB photo over a SoftAP takes a few seconds and the read timeout
            // covers the whole body, not one packet.
            .timeout_global(Some(Duration::from_secs(60)))
            .build();
        FileClient {
            host: host.to_string(),
            agent: config.into(),
            max_body_bytes: 512 * 1024 * 1024,
        }
    }

    /// Change the per-request timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .build()
            .into();
        self
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn base_url(&self) -> String {
        fileapi::base_url(&self.host)
    }

    /// Whether the glasses answer at all. A cheap listing request with a short timeout — this is
    /// how an example tells "not on the AP" from "no files".
    pub fn is_reachable(&self) -> bool {
        let short = FileClient::with_host(&self.host).with_timeout(Duration::from_secs(3));
        short.get_bytes(&fileapi::list_url_on(&self.host)).is_ok()
    }

    /// `GET /app/getfilelist`.
    pub fn list(&self) -> Result<FileList, ClientError> {
        let url = fileapi::list_url_on(&self.host);
        let body = self.get_bytes(&url)?;
        let text = String::from_utf8_lossy(&body);
        FileList::parse(&text).map_err(|source| ClientError::Parse {
            url: url.clone(),
            source,
        })
    }

    /// `GET /app/getthumbnail?file=…`. A small JPEG, ~8–12 KB.
    ///
    /// Not length-checked: the listing carries no size for a thumbnail, so there is nothing to
    /// check it against.
    pub fn thumbnail(&self, name: &str) -> Result<Vec<u8>, ClientError> {
        self.get_bytes(&fileapi::thumbnail_url_on(&self.host, name))
    }

    /// `GET /<name>` with no completeness check. For a caller that has no listing.
    pub fn download_unchecked(&self, name: &str) -> Result<Vec<u8>, ClientError> {
        self.get_bytes(&fileapi::download_url_on(&self.host, name))
    }

    /// `GET /<name>`, verified against `size_kib` from the listing.
    pub fn download_checked(&self, name: &str, size_kib: u64) -> Result<Vec<u8>, ClientError> {
        let bytes = self.download_unchecked(name)?;
        if !download_is_complete(size_kib, bytes.len() as u64) {
            return Err(ClientError::ShortDownload {
                name: name.to_string(),
                size_kib,
                bytes: bytes.len() as u64,
            });
        }
        Ok(bytes)
    }

    /// `GET /<name>`, verified against the listing entry.
    pub fn download(&self, entry: &FileEntry) -> Result<Vec<u8>, ClientError> {
        self.download_checked(&entry.name, entry.size_kib)
    }

    /// Download into `dir`, named by the file's basename. Returns the path written.
    ///
    /// The bytes are checked BEFORE the file is created, so a short transfer leaves no partial
    /// file on disk to be mistaken for a good one later.
    pub fn download_to_dir(&self, entry: &FileEntry, dir: &Path) -> Result<PathBuf, ClientError> {
        let bytes = self.download(entry)?;
        fs::create_dir_all(dir).map_err(|e| ClientError::Io {
            path: dir.to_path_buf(),
            detail: e.to_string(),
        })?;
        let path = dir.join(entry.basename());
        fs::write(&path, &bytes).map_err(|e| ClientError::Io {
            path: path.clone(),
            detail: e.to_string(),
        })?;
        Ok(path)
    }

    /// `GET /app/deletefile?file=…`. Errors when the reply says the delete did not take.
    pub fn delete(&self, name: &str) -> Result<DeleteReply, ClientError> {
        let url = fileapi::delete_url_on(&self.host, name);
        let body = self.get_bytes(&url)?;
        let text = String::from_utf8_lossy(&body);
        let reply = DeleteReply::parse(&text).map_err(|source| ClientError::Parse {
            url: url.clone(),
            source,
        })?;
        if !reply.is_success() {
            return Err(ClientError::DeleteRefused {
                name: name.to_string(),
                reply,
            });
        }
        Ok(reply)
    }

    /// The whole gallery, in the order [`SyncPlan`] recommends.
    ///
    /// Thumbnails are skipped — this writes the real files. A file is deleted only after its
    /// own download has been verified, so an interrupted sync loses nothing that was not
    /// already saved.
    ///
    /// Does NOT tear the Wi-Fi down: that is a BLE write (`0x44 30 00`) and this client has no
    /// radio. [`SyncPlan`]'s last step is the reminder.
    pub fn sync_all(&self, dir: &Path, delete_after: bool) -> Result<SyncReport, ClientError> {
        let list = self.list()?;
        let mut report = SyncReport::default();
        for entry in list.all_files() {
            match self.download_to_dir(entry, dir) {
                Ok(path) => {
                    report.bytes += fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    report.downloaded.push(path);
                }
                Err(e) => {
                    report.failures.push((entry.name.clone(), e.to_string()));
                    continue; // never delete what did not land
                }
            }
            if delete_after {
                match self.delete(&entry.name) {
                    Ok(_) => report.deleted.push(entry.name.clone()),
                    Err(e) => report.failures.push((entry.name.clone(), e.to_string())),
                }
            }
        }
        Ok(report)
    }

    /// The plan this client would follow for the gallery as it stands now.
    pub fn plan(&self, delete_after: bool) -> Result<SyncPlan, ClientError> {
        Ok(SyncPlan::full(&self.list()?, delete_after))
    }

    fn get_bytes(&self, url: &str) -> Result<Vec<u8>, ClientError> {
        let mut resp = self
            .agent
            .get(url)
            .call()
            .map_err(|e| ClientError::Transport {
                url: url.to_string(),
                detail: e.to_string(),
            })?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(ClientError::Status {
                url: url.to_string(),
                status,
            });
        }
        let mut out = Vec::new();
        resp.body_mut()
            .as_reader()
            .take(self.max_body_bytes)
            .read_to_end(&mut out)
            .map_err(|e| ClientError::Transport {
                url: url.to_string(),
                detail: e.to_string(),
            })?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No hardware and no network in the test environment, so what is checkable here is that the
    /// client builds the same URLs the sans-IO module does — which is the whole of its protocol
    /// surface. The parsing and the KiB rule are pinned in `fileapi`'s own tests.
    #[test]
    fn a_client_addresses_the_glasses_at_the_documented_host() {
        let c = FileClient::new();
        assert_eq!(c.host(), fileapi::HOST);
        assert_eq!(c.base_url(), "http://192.168.169.1");
    }

    #[test]
    fn a_client_can_be_pointed_at_a_replay_host_without_touching_the_url_builders() {
        let c = FileClient::with_host("127.0.0.1:8080");
        assert_eq!(c.base_url(), "http://127.0.0.1:8080");
        assert_eq!(
            fileapi::list_url_on(c.host()),
            "http://127.0.0.1:8080/app/getfilelist"
        );
    }

    #[test]
    fn a_short_download_error_says_what_it_expected() {
        let e = ClientError::ShortDownload {
            name: "EVENT/x.jpg".into(),
            size_kib: 313,
            bytes: 12,
        };
        assert_eq!(
            e.to_string(),
            "EVENT/x.jpg: got 12 bytes, a 313 KiB listing means 320512..=321535"
        );
    }
}
