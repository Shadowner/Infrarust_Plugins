use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::mode::ConnectionMode;

pub const CONFIG_FILE_NAME: &str = "config.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RewriteToMagic {
    Never,
    #[default]
    MagicOnly,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerOverride {
    pub mode: ConnectionMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub holepunch: bool,
    pub magic: bool,
    pub rewrite_to_magic: RewriteToMagic,
    pub servers: BTreeMap<String, ServerOverride>,
    pub proxy_protocol_to_backend: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            holepunch: true,
            magic: true,
            rewrite_to_magic: RewriteToMagic::MagicOnly,
            servers: BTreeMap::new(),
            proxy_protocol_to_backend: false,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("reading the plugin config failed: {0}")]
    Read(#[source] io::Error),
    #[error("writing the plugin config failed: {0}")]
    Write(#[source] io::Error),
    #[error("the plugin config is not valid JSON for this plugin: {0}")]
    Syntax(#[source] serde_json::Error),
    #[error("the plugin config must be a JSON object")]
    NotAnObject,
    #[error("server `{server}`: {problem}")]
    Server {
        server: String,
        problem: OverrideProblem,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum OverrideProblem {
    #[error("an override must be a JSON object with `mode` and an optional `port`")]
    NotAnObject,
    #[error("port 0 is not a usable port")]
    PortZero,
    #[error("http mode needs the port of the backend's modpack listener")]
    HttpWithoutPort,
    #[error("holepunch always uses the server's Minecraft port, remove `port`")]
    HolepunchWithPort,
}

impl ServerOverride {
    pub fn problem(&self) -> Option<OverrideProblem> {
        match (self.mode, self.port) {
            (_, Some(0)) => Some(OverrideProblem::PortZero),
            (ConnectionMode::Http, None) => Some(OverrideProblem::HttpWithoutPort),
            (ConnectionMode::Holepunch, Some(_)) => Some(OverrideProblem::HolepunchWithPort),
            _ => None,
        }
    }
}

impl Config {
    pub fn from_json(json: &str) -> Result<Config, ConfigError> {
        let document: Value = serde_json::from_str(json).map_err(ConfigError::Syntax)?;
        let Some(top) = document.as_object() else {
            return Err(ConfigError::NotAnObject);
        };
        if let Some(Value::Object(servers)) = top.get("servers")
            && let Some((server, _)) = servers.iter().find(|(_, entry)| !entry.is_object())
        {
            return Err(ConfigError::Server {
                server: server.clone(),
                problem: OverrideProblem::NotAnObject,
            });
        }
        let config = Config::deserialize(document).map_err(ConfigError::Syntax)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        for (server, entry) in &self.servers {
            if let Some(problem) = entry.problem() {
                return Err(ConfigError::Server {
                    server: server.clone(),
                    problem,
                });
            }
        }
        Ok(())
    }

    pub fn to_json(&self) -> Result<String, ConfigError> {
        let mut json = serde_json::to_string_pretty(self).map_err(ConfigError::Syntax)?;
        json.push('\n');
        Ok(json)
    }

    pub fn load(path: &Path) -> Result<Option<Config>, ConfigError> {
        match fs::read_to_string(path) {
            Ok(json) => Config::from_json(&json).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(ConfigError::Read(error)),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        fs::write(path, self.to_json()?).map_err(ConfigError::Write)
    }

    pub fn server_override(&self, server: &str) -> Option<&ServerOverride> {
        self.servers.get(server)
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
                "infrarust-automodpack-config-{}-{name}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn file(&self) -> PathBuf {
            self.0.join(CONFIG_FILE_NAME)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn problem_of(json: &str) -> OverrideProblem {
        match Config::from_json(json) {
            Err(ConfigError::Server { problem, .. }) => problem,
            other => panic!("expected an override problem, got {other:?}"),
        }
    }

    #[test]
    fn defaults_hand_off_and_rewrite_only_magic() {
        let config = Config::default();
        assert!(config.holepunch);
        assert!(config.magic);
        assert_eq!(config.rewrite_to_magic, RewriteToMagic::MagicOnly);
        assert!(config.servers.is_empty());
        assert!(!config.proxy_protocol_to_backend);
    }

    #[test]
    fn default_file_has_a_stable_documented_shape() {
        let expected = r#"{
  "holepunch": true,
  "magic": true,
  "rewrite_to_magic": "magic-only",
  "servers": {},
  "proxy_protocol_to_backend": false
}
"#;
        assert_eq!(Config::default().to_json().unwrap(), expected);
        assert_eq!(Config::from_json(expected).unwrap(), Config::default());
    }

    #[test]
    fn an_empty_object_is_all_defaults() {
        assert_eq!(Config::from_json("{}").unwrap(), Config::default());
    }

    #[test]
    fn a_full_file_parses_every_field() {
        let config = Config::from_json(
            r#"{
              "holepunch": false,
              "magic": true,
              "rewrite_to_magic": "always",
              "servers": {
                "lobby": { "mode": "holepunch" },
                "survival": { "mode": "magic" },
                "creative": { "mode": "magic", "port": 24454 },
                "files": { "mode": "http", "port": 30037 }
              },
              "proxy_protocol_to_backend": true
            }"#,
        )
        .unwrap();
        assert!(!config.holepunch);
        assert_eq!(config.rewrite_to_magic, RewriteToMagic::Always);
        assert!(config.proxy_protocol_to_backend);
        assert_eq!(
            config.server_override("lobby"),
            Some(&ServerOverride {
                mode: ConnectionMode::Holepunch,
                port: None
            })
        );
        assert_eq!(
            config.server_override("creative"),
            Some(&ServerOverride {
                mode: ConnectionMode::Magic,
                port: Some(24454)
            })
        );
        assert_eq!(config.server_override("files").unwrap().port, Some(30037));
        assert_eq!(config.server_override("unknown"), None);
    }

    #[test]
    fn every_rewrite_policy_spelling_is_accepted() {
        for (spelling, policy) in [
            ("never", RewriteToMagic::Never),
            ("magic-only", RewriteToMagic::MagicOnly),
            ("always", RewriteToMagic::Always),
        ] {
            let config =
                Config::from_json(&format!(r#"{{"rewrite_to_magic":"{spelling}"}}"#)).unwrap();
            assert_eq!(config.rewrite_to_magic, policy);
        }
        assert!(matches!(
            Config::from_json(r#"{"rewrite_to_magic":"magic_only"}"#),
            Err(ConfigError::Syntax(_))
        ));
    }

    #[test]
    fn typos_and_wrong_types_are_errors_not_silent_defaults() {
        for broken in [
            r#"{"holepuch": false}"#,
            r#"{"holepunch": "false"}"#,
            r#"{"servers": {"a": {"mode": "MAGIC"}}}"#,
            r#"{"servers": {"a": {"mode": "magic", "prot": 1}}}"#,
            r#"{"servers": {"a": {"mode": "magic", "port": 65536}}}"#,
            r#"{"servers": {"a": {"mode": "magic", "port": -1}}}"#,
            r#"{"servers": {"a": {"port": 1}}}"#,
            "",
        ] {
            assert!(
                matches!(Config::from_json(broken), Err(ConfigError::Syntax(_))),
                "{broken}"
            );
        }
    }

    #[test]
    fn only_an_object_is_a_config() {
        for not_object in ["[]", "[true]", "null", "1", "\"magic\""] {
            assert!(
                matches!(Config::from_json(not_object), Err(ConfigError::NotAnObject)),
                "{not_object}"
            );
        }
    }

    #[test]
    fn overrides_must_name_a_reachable_endpoint() {
        assert_eq!(
            problem_of(r#"{"servers": {"files": {"mode": "http"}}}"#),
            OverrideProblem::HttpWithoutPort
        );
        assert_eq!(
            problem_of(r#"{"servers": {"a": ["magic"]}}"#),
            OverrideProblem::NotAnObject
        );
        assert_eq!(
            problem_of(r#"{"servers": {"a": {"mode": "magic", "port": 0}}}"#),
            OverrideProblem::PortZero
        );
        assert_eq!(
            problem_of(r#"{"servers": {"a": {"mode": "holepunch", "port": 25565}}}"#),
            OverrideProblem::HolepunchWithPort
        );
        match Config::from_json(r#"{"servers": {"files": {"mode": "http"}}}"#) {
            Err(ConfigError::Server { server, .. }) => assert_eq!(server, "files"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn load_reports_absence_separately_from_errors() {
        let dir = TempDir::new("load");
        assert!(matches!(Config::load(&dir.file()), Ok(None)));

        Config::default().save(&dir.file()).unwrap();
        assert_eq!(Config::load(&dir.file()).unwrap(), Some(Config::default()));

        fs::write(dir.file(), r#"{"magic": 1}"#).unwrap();
        assert!(matches!(
            Config::load(&dir.file()),
            Err(ConfigError::Syntax(_))
        ));

        fs::remove_file(dir.file()).unwrap();
        fs::create_dir(dir.file()).unwrap();
        assert!(matches!(
            Config::load(&dir.file()),
            Err(ConfigError::Read(_))
        ));
    }
}
