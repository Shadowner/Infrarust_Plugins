use serde_json::{Map, Value};
use thiserror::Error;

use crate::mode::ConnectionMode;
use crate::wire::{self, WireError};

pub const HANDSHAKE_CHANNEL: &str = "automodpack:handshake";
pub const DATA_CHANNEL: &str = "automodpack:data";
pub const HANDSHAKE_MESSAGE_ID: i32 = -100;
pub const DATA_MESSAGE_ID: i32 = -101;
pub const CONNECTION_PORT: i32 = -1;

const V5_HOST: &str = "endpointHost";
const V5_PORT: &str = "endpointPort";
const V5_MODE: &str = "connectionMode";
const V4_HOST: &str = "address";
const V4_PORT: &str = "port";
const V4_MAGIC: &str = "requiresMagic";
const V4_ONLY_KEYS: [&str; 5] = [V4_HOST, V4_PORT, V4_MAGIC, "modpackName", "modRequired"];

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DataError {
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error("the payload is not JSON: {0}")]
    Json(String),
    #[error("the payload is not a JSON object")]
    NotAnObject,
    #[error("the payload matches neither the 5.x nor the 4.x data layout")]
    UnknownLayout,
    #[error("field `{0}` is missing")]
    MissingField(&'static str),
    #[error("field `{0}` has the wrong type or range")]
    BadField(&'static str),
    #[error("connection mode `{0}` is not one AutoModpack knows")]
    UnknownMode(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataFormat {
    V5,
    V4,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvertisedEndpoint {
    pub host: String,
    pub port: i32,
    pub mode: ConnectionMode,
}

impl AdvertisedEndpoint {
    pub fn uses_connection_host(&self) -> bool {
        self.host.trim().is_empty()
    }

    pub fn uses_connection_port(&self) -> bool {
        self.port == CONNECTION_PORT
    }

    pub fn is_proxy_magic(&self) -> bool {
        self.uses_connection_host()
            && self.uses_connection_port()
            && self.mode == ConnectionMode::Magic
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rewrite {
    ToProxyMagic,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DataPacket {
    format: DataFormat,
    endpoint: AdvertisedEndpoint,
    fields: Map<String, Value>,
}

impl DataPacket {
    pub fn parse(payload: &[u8]) -> Result<DataPacket, DataError> {
        DataPacket::from_json(&wire::read_single_string(payload)?)
    }

    pub fn from_json(json: &str) -> Result<DataPacket, DataError> {
        let value: Value =
            serde_json::from_str(json).map_err(|error| DataError::Json(error.to_string()))?;
        let Value::Object(fields) = value else {
            return Err(DataError::NotAnObject);
        };
        let format = detect_format(&fields)?;
        let endpoint = match format {
            DataFormat::V5 => v5_endpoint(&fields)?,
            DataFormat::V4 => v4_endpoint(&fields)?,
        };
        Ok(DataPacket {
            format,
            endpoint,
            fields,
        })
    }

    pub fn format(&self) -> DataFormat {
        self.format
    }

    pub fn endpoint(&self) -> &AdvertisedEndpoint {
        &self.endpoint
    }

    pub fn fields(&self) -> &Map<String, Value> {
        &self.fields
    }

    pub fn rewritten(&self, rewrite: Rewrite) -> DataPacket {
        let mut fields = self.fields.clone();
        let endpoint = match rewrite {
            Rewrite::ToProxyMagic => {
                let (host_key, port_key) = match self.format {
                    DataFormat::V5 => (V5_HOST, V5_PORT),
                    DataFormat::V4 => (V4_HOST, V4_PORT),
                };
                fields.insert(host_key.to_owned(), Value::from(""));
                fields.insert(port_key.to_owned(), Value::from(CONNECTION_PORT));
                match self.format {
                    DataFormat::V5 => fields.insert(
                        V5_MODE.to_owned(),
                        Value::from(ConnectionMode::Magic.wire_name()),
                    ),
                    DataFormat::V4 => fields.insert(V4_MAGIC.to_owned(), Value::from(true)),
                };
                AdvertisedEndpoint {
                    host: String::new(),
                    port: CONNECTION_PORT,
                    mode: ConnectionMode::Magic,
                }
            }
        };
        DataPacket {
            format: self.format,
            endpoint,
            fields,
        }
    }

    pub fn to_json(&self) -> String {
        Value::Object(self.fields.clone()).to_string()
    }

    pub fn to_payload(&self) -> Result<Vec<u8>, DataError> {
        Ok(wire::single_string_payload(&self.to_json())?)
    }
}

fn detect_format(fields: &Map<String, Value>) -> Result<DataFormat, DataError> {
    if fields.contains_key(V5_MODE) {
        Ok(DataFormat::V5)
    } else if V4_ONLY_KEYS.iter().any(|key| fields.contains_key(*key)) {
        Ok(DataFormat::V4)
    } else {
        Err(DataError::UnknownLayout)
    }
}

fn v5_endpoint(fields: &Map<String, Value>) -> Result<AdvertisedEndpoint, DataError> {
    let mode = match fields.get(V5_MODE) {
        Some(Value::String(name)) => ConnectionMode::from_wire_name(name)
            .ok_or_else(|| DataError::UnknownMode(name.clone()))?,
        Some(Value::Null) | None => return Err(DataError::MissingField(V5_MODE)),
        Some(_) => return Err(DataError::BadField(V5_MODE)),
    };
    Ok(AdvertisedEndpoint {
        host: optional_host(fields, V5_HOST)?,
        port: required_port(fields, V5_PORT)?,
        mode,
    })
}

fn v4_endpoint(fields: &Map<String, Value>) -> Result<AdvertisedEndpoint, DataError> {
    let requires_magic = match fields.get(V4_MAGIC) {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Null) | None => false,
        Some(_) => return Err(DataError::BadField(V4_MAGIC)),
    };
    Ok(AdvertisedEndpoint {
        host: optional_host(fields, V4_HOST)?,
        port: required_port(fields, V4_PORT)?,
        mode: if requires_magic {
            ConnectionMode::Magic
        } else {
            ConnectionMode::Http
        },
    })
}

fn optional_host(fields: &Map<String, Value>, key: &'static str) -> Result<String, DataError> {
    match fields.get(key) {
        Some(Value::String(host)) => Ok(host.clone()),
        Some(Value::Null) | None => Ok(String::new()),
        Some(_) => Err(DataError::BadField(key)),
    }
}

fn required_port(fields: &Map<String, Value>, key: &'static str) -> Result<i32, DataError> {
    match fields.get(key) {
        Some(Value::Number(number)) => number
            .as_i64()
            .and_then(|port| i32::try_from(port).ok())
            .ok_or(DataError::BadField(key)),
        Some(Value::Null) | None => Err(DataError::MissingField(key)),
        Some(_) => Err(DataError::BadField(key)),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientAnswer {
    Continue,
    UpdateRequired,
    HostError,
    Rejected,
    Declined,
    Cancelled,
    NoAnswer,
    Unrecognized(String),
    Malformed,
}

impl ClientAnswer {
    pub fn read(payload: Option<&[u8]>) -> ClientAnswer {
        let Some(payload) = payload.filter(|bytes| !bytes.is_empty()) else {
            return ClientAnswer::NoAnswer;
        };
        let Ok(word) = wire::read_single_string(payload) else {
            return ClientAnswer::Malformed;
        };
        match word.as_str() {
            "false" => ClientAnswer::Continue,
            "true" => ClientAnswer::UpdateRequired,
            "null" => ClientAnswer::HostError,
            "rejected" => ClientAnswer::Rejected,
            "declined" => ClientAnswer::Declined,
            "cancelled" => ClientAnswer::Cancelled,
            _ => ClientAnswer::Unrecognized(word),
        }
    }

    pub fn wire_value(&self) -> Option<&str> {
        match self {
            ClientAnswer::Continue => Some("false"),
            ClientAnswer::UpdateRequired => Some("true"),
            ClientAnswer::HostError => Some("null"),
            ClientAnswer::Rejected => Some("rejected"),
            ClientAnswer::Declined => Some("declined"),
            ClientAnswer::Cancelled => Some("cancelled"),
            ClientAnswer::Unrecognized(word) => Some(word),
            ClientAnswer::NoAnswer | ClientAnswer::Malformed => None,
        }
    }

    pub fn lets_player_in(&self) -> bool {
        *self == ClientAnswer::Continue
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HandshakeInfo {
    pub loaders: Vec<String>,
    pub am_version: Option<String>,
    pub mc_version: Option<String>,
}

impl HandshakeInfo {
    pub fn parse(payload: &[u8]) -> Result<HandshakeInfo, DataError> {
        HandshakeInfo::from_json(&wire::read_single_string(payload)?)
    }

    pub fn from_json(json: &str) -> Result<HandshakeInfo, DataError> {
        let value: Value =
            serde_json::from_str(json).map_err(|error| DataError::Json(error.to_string()))?;
        let Value::Object(fields) = value else {
            return Err(DataError::NotAnObject);
        };
        let text = |key: &str| fields.get(key).and_then(Value::as_str).map(str::to_owned);
        let loaders = fields
            .get("loaders")
            .and_then(Value::as_array)
            .map(|loaders| {
                loaders
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Ok(HandshakeInfo {
            loaders,
            am_version: text("amVersion"),
            mc_version: text("mcVersion"),
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const V5_HOLEPUNCH: &str = include_str!("../testdata/gson/v5_holepunch.json");
    const V5_MAGIC_DEDICATED: &str = include_str!("../testdata/gson/v5_magic_dedicated.json");
    const V5_HTTP: &str = include_str!("../testdata/gson/v5_http.json");
    const V5_MAGIC_NULL_HOST: &str = include_str!("../testdata/gson/v5_magic_null_host.json");
    const V4_MAGIC: &str = include_str!("../testdata/gson/v4_magic.json");
    const V4_DIRECT: &str = include_str!("../testdata/gson/v4_direct.json");
    const V4_DIRECT_VELOCITY: &str =
        include_str!("../testdata/gson/v4_direct_velocity_rewritten.json");
    const HANDSHAKE_V5: &str = include_str!("../testdata/gson/handshake_v5.json");
    const HANDSHAKE_V4: &str = include_str!("../testdata/gson/handshake_v4.json");

    fn payload(json: &str) -> Vec<u8> {
        wire::single_string_payload(json).unwrap()
    }

    fn reparsed(payload: &[u8]) -> Value {
        serde_json::from_str(&wire::read_single_string(payload).unwrap()).unwrap()
    }

    fn without(mut value: Value, keys: &[&str]) -> Value {
        let object = value.as_object_mut().unwrap();
        for key in keys {
            object.remove(*key);
        }
        value
    }

    #[test]
    fn channel_ids_match_login_networking_ids() {
        assert_eq!(HANDSHAKE_CHANNEL, "automodpack:handshake");
        assert_eq!(DATA_CHANNEL, "automodpack:data");
        assert_eq!(HANDSHAKE_MESSAGE_ID, -100);
        assert_eq!(DATA_MESSAGE_ID, -101);
    }

    #[test]
    fn gson_pretty_v5_packets_parse_with_their_endpoint() {
        let cases = [
            (V5_HOLEPUNCH, "", -1, ConnectionMode::Holepunch),
            (
                V5_MAGIC_DEDICATED,
                "mc.example.net",
                24454,
                ConnectionMode::Magic,
            ),
            (V5_HTTP, "", 30037, ConnectionMode::Http),
            (V5_MAGIC_NULL_HOST, "", -1, ConnectionMode::Magic),
        ];
        for (json, host, port, mode) in cases {
            let packet = DataPacket::parse(&payload(json)).unwrap();
            assert_eq!(packet.format(), DataFormat::V5);
            assert_eq!(
                packet.endpoint(),
                &AdvertisedEndpoint {
                    host: host.to_owned(),
                    port,
                    mode
                }
            );
        }
    }

    #[test]
    fn gson_v4_packets_parse_with_their_endpoint() {
        let magic = DataPacket::parse(&payload(V4_MAGIC)).unwrap();
        assert_eq!(magic.format(), DataFormat::V4);
        assert_eq!(magic.endpoint().mode, ConnectionMode::Magic);
        assert!(magic.endpoint().is_proxy_magic());
        assert_eq!(magic.fields()["modpackName"], "Tom's <Pack> & Co = 1");

        let direct = DataPacket::parse(&payload(V4_DIRECT)).unwrap();
        assert_eq!(
            direct.endpoint(),
            &AdvertisedEndpoint {
                host: "files.example.org".to_owned(),
                port: 25570,
                mode: ConnectionMode::Http
            }
        );
    }

    #[test]
    fn v5_rewrite_points_the_client_back_at_the_proxy_and_keeps_everything_else() {
        let original: Value = serde_json::from_str(V5_MAGIC_DEDICATED).unwrap();
        let packet = DataPacket::parse(&payload(V5_MAGIC_DEDICATED)).unwrap();
        let rewritten = packet.rewritten(Rewrite::ToProxyMagic);
        assert!(rewritten.endpoint().is_proxy_magic());

        let on_wire = reparsed(&rewritten.to_payload().unwrap());
        assert_eq!(on_wire["endpointHost"], "");
        assert_eq!(on_wire["endpointPort"], -1);
        assert_eq!(on_wire["connectionMode"], "MAGIC");
        assert_eq!(
            without(
                on_wire.clone(),
                &["endpointHost", "endpointPort", "connectionMode"]
            ),
            without(
                original,
                &["endpointHost", "endpointPort", "connectionMode"]
            )
        );
        assert_eq!(
            on_wire["secret"],
            json!({"secret": "Q2xhdWRlLWF1dG9tb2RwYWNrLWJyaWRnZS10ZXN0LXNlY3JldA", "timestamp": 1_760_183_415})
        );
        assert_eq!(on_wire["requireModpack"], false);
    }

    #[test]
    fn v5_rewrite_of_an_http_packet_switches_it_to_magic() {
        let packet = DataPacket::parse(&payload(V5_HTTP)).unwrap();
        let on_wire = reparsed(
            &packet
                .rewritten(Rewrite::ToProxyMagic)
                .to_payload()
                .unwrap(),
        );
        assert_eq!(on_wire["connectionMode"], "MAGIC");
        assert_eq!(on_wire["endpointPort"], -1);
    }

    #[test]
    fn v5_rewrite_fills_a_host_gson_left_out_for_null() {
        let packet = DataPacket::parse(&payload(V5_MAGIC_NULL_HOST)).unwrap();
        let on_wire = reparsed(
            &packet
                .rewritten(Rewrite::ToProxyMagic)
                .to_payload()
                .unwrap(),
        );
        assert_eq!(on_wire["endpointHost"], "");
    }

    #[test]
    fn v4_rewrite_matches_what_the_velocity_bridge_sends() {
        let packet = DataPacket::parse(&payload(V4_DIRECT)).unwrap();
        let ours = reparsed(
            &packet
                .rewritten(Rewrite::ToProxyMagic)
                .to_payload()
                .unwrap(),
        );
        let velocity: Value = serde_json::from_str(V4_DIRECT_VELOCITY).unwrap();
        assert_eq!(ours, velocity);
    }

    #[test]
    fn v4_rewrite_keeps_html_escaped_values_equal() {
        let packet = DataPacket::parse(&payload(V4_MAGIC)).unwrap();
        let original: Value = serde_json::from_str(V4_MAGIC).unwrap();
        let ours = reparsed(
            &packet
                .rewritten(Rewrite::ToProxyMagic)
                .to_payload()
                .unwrap(),
        );
        assert_eq!(ours, original);
    }

    #[test]
    fn unknown_fields_and_extreme_numbers_survive_a_rewrite() {
        let json = json!({
            "endpointHost": "203.0.113.7",
            "endpointPort": 25570,
            "secret": {"secret": "s", "timestamp": i64::MAX},
            "connectionMode": "HTTP",
            "requireModpack": true,
            "futureField": {"nested": [1, 2.5, -3, null, "x"]},
            "bigUnsigned": u64::MAX
        });
        let packet = DataPacket::parse(&payload(&json.to_string())).unwrap();
        let on_wire = reparsed(
            &packet
                .rewritten(Rewrite::ToProxyMagic)
                .to_payload()
                .unwrap(),
        );
        assert_eq!(on_wire["secret"]["timestamp"], i64::MAX);
        assert_eq!(on_wire["bigUnsigned"], u64::MAX);
        assert_eq!(on_wire["futureField"], json["futureField"]);
    }

    #[test]
    fn rewriting_twice_changes_nothing_more() {
        let packet = DataPacket::parse(&payload(V5_HOLEPUNCH)).unwrap();
        let once = packet.rewritten(Rewrite::ToProxyMagic);
        let twice = once.rewritten(Rewrite::ToProxyMagic);
        assert_eq!(once.to_payload().unwrap(), twice.to_payload().unwrap());
    }

    #[test]
    fn v5_needs_a_known_mode_and_an_integer_port() {
        let base = json!({"endpointHost": "", "endpointPort": -1, "connectionMode": "MAGIC"});
        let with = |key: &str, value: Value| {
            let mut json = base.clone();
            json[key] = value;
            DataPacket::from_json(&json.to_string())
        };
        assert!(with("endpointPort", json!(25565)).is_ok());
        assert_eq!(
            with("connectionMode", json!("DIRECT")),
            Err(DataError::UnknownMode("DIRECT".to_owned()))
        );
        assert_eq!(
            with("connectionMode", json!("magic")),
            Err(DataError::UnknownMode("magic".to_owned()))
        );
        assert_eq!(
            with("connectionMode", Value::Null),
            Err(DataError::MissingField("connectionMode"))
        );
        assert_eq!(
            with("connectionMode", json!(1)),
            Err(DataError::BadField("connectionMode"))
        );
        assert_eq!(
            with("endpointPort", json!(25565.5)),
            Err(DataError::BadField("endpointPort"))
        );
        assert_eq!(
            with("endpointPort", json!(i64::from(i32::MAX) + 1)),
            Err(DataError::BadField("endpointPort"))
        );
        assert_eq!(
            with("endpointPort", json!("25565")),
            Err(DataError::BadField("endpointPort"))
        );
        assert_eq!(
            with("endpointHost", json!(5)),
            Err(DataError::BadField("endpointHost"))
        );
        assert_eq!(
            DataPacket::from_json(r#"{"connectionMode":"MAGIC"}"#),
            Err(DataError::MissingField("endpointPort"))
        );
    }

    #[test]
    fn v4_needs_an_integer_port_and_a_boolean_magic_flag() {
        assert_eq!(
            DataPacket::from_json(r#"{"address":"","requiresMagic":true}"#),
            Err(DataError::MissingField("port"))
        );
        assert_eq!(
            DataPacket::from_json(r#"{"address":"","port":-1,"requiresMagic":"yes"}"#),
            Err(DataError::BadField("requiresMagic"))
        );
        let gson_default = DataPacket::from_json(r#"{"port":25570,"modpackName":"x"}"#).unwrap();
        assert_eq!(gson_default.format(), DataFormat::V4);
        assert_eq!(gson_default.endpoint().mode, ConnectionMode::Http);
        assert_eq!(gson_default.endpoint().host, "");
    }

    #[test]
    fn foreign_payloads_are_refused() {
        assert!(matches!(
            DataPacket::from_json("not json"),
            Err(DataError::Json(_))
        ));
        assert_eq!(DataPacket::from_json("[]"), Err(DataError::NotAnObject));
        assert_eq!(DataPacket::from_json("\"x\""), Err(DataError::NotAnObject));
        assert_eq!(DataPacket::from_json("{}"), Err(DataError::UnknownLayout));
        assert_eq!(
            DataPacket::from_json(r#"{"endpointHost":"","endpointPort":-1}"#),
            Err(DataError::UnknownLayout)
        );
        let mut trailing = payload(V5_HOLEPUNCH);
        trailing.push(0);
        assert_eq!(
            DataPacket::parse(&trailing),
            Err(DataError::Wire(WireError::TrailingBytes(1)))
        );
        assert_eq!(
            DataPacket::parse(&[]),
            Err(DataError::Wire(WireError::Truncated))
        );
    }

    #[test]
    fn blank_hosts_follow_java_is_blank() {
        let endpoint = |host: &str| AdvertisedEndpoint {
            host: host.to_owned(),
            port: -1,
            mode: ConnectionMode::Magic,
        };
        assert!(endpoint("").is_proxy_magic());
        assert!(endpoint("  \t").is_proxy_magic());
        assert!(!endpoint("a").is_proxy_magic());
        assert!(
            !AdvertisedEndpoint {
                port: 25565,
                ..endpoint("")
            }
            .is_proxy_magic()
        );
        assert!(
            !AdvertisedEndpoint {
                mode: ConnectionMode::Http,
                ..endpoint("")
            }
            .is_proxy_magic()
        );
    }

    #[test]
    fn client_answers_map_every_wire_value() {
        let cases = [
            ("false", ClientAnswer::Continue),
            ("true", ClientAnswer::UpdateRequired),
            ("null", ClientAnswer::HostError),
            ("rejected", ClientAnswer::Rejected),
            ("declined", ClientAnswer::Declined),
            ("cancelled", ClientAnswer::Cancelled),
            ("maybe", ClientAnswer::Unrecognized("maybe".to_owned())),
        ];
        for (word, answer) in cases {
            assert_eq!(ClientAnswer::read(Some(&payload(word))), answer, "{word}");
            assert_eq!(answer.wire_value(), Some(word));
        }
        assert!(ClientAnswer::Continue.lets_player_in());
        assert!(!ClientAnswer::UpdateRequired.lets_player_in());
    }

    #[test]
    fn missing_or_broken_client_answers_are_told_apart() {
        assert_eq!(ClientAnswer::read(None), ClientAnswer::NoAnswer);
        assert_eq!(ClientAnswer::read(Some(&[])), ClientAnswer::NoAnswer);
        assert_eq!(
            ClientAnswer::read(Some(&[0x05, b'f'])),
            ClientAnswer::Malformed
        );
        let mut trailing = payload("false");
        trailing.push(1);
        assert_eq!(ClientAnswer::read(Some(&trailing)), ClientAnswer::Malformed);
        assert_eq!(ClientAnswer::NoAnswer.wire_value(), None);
    }

    #[test]
    fn handshakes_of_both_generations_expose_the_versions() {
        assert_eq!(
            HandshakeInfo::parse(&payload(HANDSHAKE_V5)).unwrap(),
            HandshakeInfo {
                loaders: vec!["fabric".to_owned()],
                am_version: Some("5.0.0-rc.3".to_owned()),
                mc_version: Some("1.21.10".to_owned()),
            }
        );
        assert_eq!(
            HandshakeInfo::parse(&payload(HANDSHAKE_V4))
                .unwrap()
                .am_version
                .as_deref(),
            Some("4.0.6")
        );
        assert_eq!(
            HandshakeInfo::from_json("{}").unwrap(),
            HandshakeInfo::default()
        );
        assert_eq!(HandshakeInfo::from_json("[]"), Err(DataError::NotAnObject));
    }
}
