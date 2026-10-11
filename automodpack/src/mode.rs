use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionMode {
    Holepunch,
    Magic,
    Http,
}

impl ConnectionMode {
    pub const ALL: [ConnectionMode; 3] = [
        ConnectionMode::Holepunch,
        ConnectionMode::Magic,
        ConnectionMode::Http,
    ];

    pub fn wire_name(self) -> &'static str {
        match self {
            ConnectionMode::Holepunch => "HOLEPUNCH",
            ConnectionMode::Magic => "MAGIC",
            ConnectionMode::Http => "HTTP",
        }
    }

    pub fn from_wire_name(name: &str) -> Option<ConnectionMode> {
        ConnectionMode::ALL
            .into_iter()
            .find(|mode| mode.wire_name() == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_names_are_the_exact_java_enum_constants() {
        for mode in ConnectionMode::ALL {
            assert_eq!(ConnectionMode::from_wire_name(mode.wire_name()), Some(mode));
        }
        assert_eq!(ConnectionMode::from_wire_name("magic"), None);
        assert_eq!(ConnectionMode::from_wire_name("DIRECT"), None);
        assert_eq!(ConnectionMode::from_wire_name(""), None);
    }

    #[test]
    fn config_spelling_is_lowercase() {
        assert_eq!(
            serde_json::to_string(&ConnectionMode::Holepunch).unwrap(),
            "\"holepunch\""
        );
        assert_eq!(
            serde_json::from_str::<ConnectionMode>("\"http\"").unwrap(),
            ConnectionMode::Http
        );
        assert!(serde_json::from_str::<ConnectionMode>("\"MAGIC\"").is_err());
    }
}
