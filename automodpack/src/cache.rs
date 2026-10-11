use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::mode::ConnectionMode;

pub type ServerKey = String;

pub const CACHE_FILE_NAME: &str = "server-cache.json";
pub const CACHE_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerEntry {
    pub mode: ConnectionMode,
    pub advertised_port: i32,
    pub advertised_host: String,
    pub seen_at_ms: u64,
    #[serde(default)]
    pub amp_version: Option<String>,
}

impl ServerEntry {
    fn same_observation(&self, other: &ServerEntry) -> bool {
        self.mode == other.mode
            && self.advertised_port == other.advertised_port
            && self.advertised_host == other.advertised_host
            && self.amp_version == other.amp_version
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    New,
    Changed,
    Refreshed,
}

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("reading the server cache failed: {0}")]
    Read(#[source] io::Error),
    #[error("writing the server cache failed: {0}")]
    Write(#[source] io::Error),
    #[error("the server cache is unreadable: {0}")]
    Corrupt(#[source] serde_json::Error),
    #[error("the server cache could not be encoded: {0}")]
    Encode(#[source] serde_json::Error),
    #[error(
        "the server cache uses format version {0}, this plugin reads version {CACHE_FORMAT_VERSION}"
    )]
    UnsupportedVersion(u32),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cache {
    servers: BTreeMap<ServerKey, ServerEntry>,
}

#[derive(Deserialize)]
struct CacheDocument {
    version: u32,
    #[serde(default)]
    servers: BTreeMap<ServerKey, ServerEntry>,
}

#[derive(Serialize)]
struct CacheDocumentView<'a> {
    version: u32,
    servers: &'a BTreeMap<ServerKey, ServerEntry>,
}

impl Cache {
    pub fn new() -> Cache {
        Cache::default()
    }

    pub fn get(&self, server: &str) -> Option<&ServerEntry> {
        self.servers.get(server)
    }

    pub fn len(&self) -> usize {
        self.servers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &ServerEntry)> {
        self.servers
            .iter()
            .map(|(server, entry)| (server.as_str(), entry))
    }

    pub fn record(&mut self, server: &str, entry: ServerEntry) -> Recorded {
        let outcome = match self.servers.get(server) {
            None => Recorded::New,
            Some(previous) if previous.same_observation(&entry) => Recorded::Refreshed,
            Some(_) => Recorded::Changed,
        };
        self.servers.insert(server.to_owned(), entry);
        outcome
    }

    pub fn forget_after_login_without_handshake(&mut self, server: &str) -> Option<ServerEntry> {
        self.servers.remove(server)
    }

    pub fn to_json(&self) -> Result<String, CacheError> {
        let document = CacheDocumentView {
            version: CACHE_FORMAT_VERSION,
            servers: &self.servers,
        };
        let mut json = serde_json::to_string_pretty(&document).map_err(CacheError::Encode)?;
        json.push('\n');
        Ok(json)
    }

    pub fn from_json(json: &str) -> Result<Cache, CacheError> {
        let document: CacheDocument = serde_json::from_str(json).map_err(CacheError::Corrupt)?;
        if document.version != CACHE_FORMAT_VERSION {
            return Err(CacheError::UnsupportedVersion(document.version));
        }
        Ok(Cache {
            servers: document.servers,
        })
    }

    pub fn load(path: &Path) -> (Cache, Option<CacheError>) {
        let loaded = match fs::read_to_string(path) {
            Ok(json) => Cache::from_json(&json),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Cache::new()),
            Err(error) => Err(CacheError::Read(error)),
        };
        match loaded {
            Ok(cache) => (cache, None),
            Err(error) => (Cache::new(), Some(error)),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), CacheError> {
        let mut staging_name = path.file_name().map(OsString::from).unwrap_or_default();
        staging_name.push(".tmp");
        let staging = path.with_file_name(staging_name);
        fs::write(&staging, self.to_json()?).map_err(CacheError::Write)?;
        fs::rename(&staging, path).map_err(|error| {
            let _ = fs::remove_file(&staging);
            CacheError::Write(error)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            let dir = std::env::temp_dir().join(format!(
                "infrarust-automodpack-cache-{}-{name}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn file(&self) -> PathBuf {
            self.0.join(CACHE_FILE_NAME)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entry(mode: ConnectionMode, port: i32, seen_at_ms: u64) -> ServerEntry {
        ServerEntry {
            mode,
            advertised_port: port,
            advertised_host: String::new(),
            seen_at_ms,
            amp_version: Some("5.0.0-rc.3".to_owned()),
        }
    }

    #[test]
    fn record_tells_new_changed_and_refreshed_apart() {
        let mut cache = Cache::new();
        assert_eq!(
            cache.record("lobby", entry(ConnectionMode::Magic, -1, 1)),
            Recorded::New
        );
        assert_eq!(
            cache.record("lobby", entry(ConnectionMode::Magic, -1, 2)),
            Recorded::Refreshed
        );
        assert_eq!(cache.get("lobby").unwrap().seen_at_ms, 2);
        assert_eq!(
            cache.record("lobby", entry(ConnectionMode::Magic, 24454, 3)),
            Recorded::Changed
        );
        assert_eq!(
            cache.record("lobby", entry(ConnectionMode::Holepunch, 24454, 4)),
            Recorded::Changed
        );
        let mut other_version = entry(ConnectionMode::Holepunch, 24454, 5);
        other_version.amp_version = None;
        assert_eq!(cache.record("lobby", other_version), Recorded::Changed);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn a_login_without_handshake_forgets_only_that_server() {
        let mut cache = Cache::new();
        cache.record("lobby", entry(ConnectionMode::Holepunch, -1, 1));
        cache.record("survival", entry(ConnectionMode::Magic, -1, 1));
        assert_eq!(
            cache.forget_after_login_without_handshake("lobby"),
            Some(entry(ConnectionMode::Holepunch, -1, 1))
        );
        assert_eq!(cache.get("lobby"), None);
        assert!(cache.get("survival").is_some());
        assert_eq!(cache.forget_after_login_without_handshake("lobby"), None);
        assert_eq!(cache.forget_after_login_without_handshake("unknown"), None);
    }

    #[test]
    fn json_document_has_a_stable_documented_shape() {
        let mut cache = Cache::new();
        cache.record(
            "lobby",
            ServerEntry {
                mode: ConnectionMode::Magic,
                advertised_port: 24454,
                advertised_host: "mc.example.net".to_owned(),
                seen_at_ms: 1_760_183_415_000,
                amp_version: Some("5.0.0-rc.3".to_owned()),
            },
        );
        let expected = r#"{
  "version": 1,
  "servers": {
    "lobby": {
      "mode": "magic",
      "advertised_port": 24454,
      "advertised_host": "mc.example.net",
      "seen_at_ms": 1760183415000,
      "amp_version": "5.0.0-rc.3"
    }
  }
}
"#;
        assert_eq!(cache.to_json().unwrap(), expected);
        assert_eq!(Cache::from_json(expected).unwrap(), cache);
    }

    #[test]
    fn from_json_tolerates_fields_a_newer_plugin_might_add() {
        let json = r#"{"version":1,"servers":{"a":{"mode":"http","advertised_port":30037,"advertised_host":"","seen_at_ms":5,"later":true}},"extra":[]}"#;
        let cache = Cache::from_json(json).unwrap();
        assert_eq!(cache.get("a").unwrap().amp_version, None);
        assert_eq!(cache.get("a").unwrap().mode, ConnectionMode::Http);
    }

    #[test]
    fn from_json_refuses_other_versions_and_bad_shapes() {
        assert!(matches!(
            Cache::from_json(r#"{"version":2,"servers":{}}"#),
            Err(CacheError::UnsupportedVersion(2))
        ));
        assert!(matches!(
            Cache::from_json(r#"{"servers":{}}"#),
            Err(CacheError::Corrupt(_))
        ));
        assert!(matches!(
            Cache::from_json(r#"{"version":1,"servers":{"a":{"mode":"MAGIC"}}}"#),
            Err(CacheError::Corrupt(_))
        ));
        assert!(matches!(Cache::from_json(""), Err(CacheError::Corrupt(_))));
    }

    #[test]
    fn load_of_a_missing_file_is_empty_without_error() {
        let dir = TempDir::new("missing");
        let (cache, error) = Cache::load(&dir.file());
        assert!(cache.is_empty());
        assert!(error.is_none());
    }

    #[test]
    fn load_of_a_corrupt_file_is_empty_with_the_error() {
        let dir = TempDir::new("corrupt");
        fs::write(dir.file(), "{\"version\":1,\"servers\":{\"a\":").unwrap();
        let (cache, error) = Cache::load(&dir.file());
        assert!(cache.is_empty());
        assert!(matches!(error, Some(CacheError::Corrupt(_))));
    }

    #[test]
    fn load_of_an_unreadable_path_is_empty_with_the_error() {
        let dir = TempDir::new("unreadable");
        fs::create_dir(dir.file()).unwrap();
        let (cache, error) = Cache::load(&dir.file());
        assert!(cache.is_empty());
        assert!(matches!(error, Some(CacheError::Read(_))));
    }

    #[test]
    fn save_then_load_round_trips_and_leaves_no_staging_file() {
        let dir = TempDir::new("roundtrip");
        let mut cache = Cache::new();
        cache.record("lobby", entry(ConnectionMode::Holepunch, -1, 10));
        cache.record("files", entry(ConnectionMode::Http, 30037, 11));
        cache.save(&dir.file()).unwrap();
        cache.record("lobby", entry(ConnectionMode::Magic, -1, 12));
        cache.save(&dir.file()).unwrap();

        let (loaded, error) = Cache::load(&dir.file());
        assert!(error.is_none());
        assert_eq!(loaded, cache);
        let names: Vec<_> = fs::read_dir(&dir.0)
            .unwrap()
            .map(|item| item.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![OsString::from(CACHE_FILE_NAME)]);
    }

    #[test]
    fn save_into_a_missing_directory_reports_a_write_error() {
        let dir = TempDir::new("nodir");
        let target = dir.0.join("absent").join(CACHE_FILE_NAME);
        assert!(matches!(
            Cache::new().save(&target),
            Err(CacheError::Write(_))
        ));
    }

    #[test]
    fn iter_walks_servers_in_key_order() {
        let mut cache = Cache::new();
        cache.record("b", entry(ConnectionMode::Magic, -1, 1));
        cache.record("a", entry(ConnectionMode::Http, 1, 1));
        let keys: Vec<&str> = cache.iter().map(|(server, _)| server).collect();
        assert_eq!(keys, vec!["a", "b"]);
    }
}
