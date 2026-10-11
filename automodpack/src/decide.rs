use crate::ammh::{self, AMOK, Ammh, MalformedHost};
use crate::cache::{Cache, ServerEntry};
use crate::config::{Config, RewriteToMagic};
use crate::holepunch;
use crate::login_data::{CONNECTION_PORT, DataPacket, HandshakeInfo, Rewrite};
use crate::mode::ConnectionMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimDecision {
    Pass,
    HandoffRoute,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawDecision {
    Pass,
    NeedMore(u32),
    Reject(RejectReason),
    Handoff {
        server: String,
        port: Option<u16>,
        consumed: usize,
        replay: bool,
        reply: Vec<u8>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    MalformedPreamble(MalformedHost),
    UnknownHost,
    NoModpackEndpoint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataDecision {
    Forward,
    Replace(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    pub mode: ConnectionMode,
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoginWatch {
    handshake_seen: bool,
    data_seen: bool,
    amp_version: Option<String>,
}

impl LoginWatch {
    pub fn new() -> LoginWatch {
        LoginWatch::default()
    }

    pub fn handshake_seen(&self) -> bool {
        self.handshake_seen
    }

    pub fn saw_automodpack(&self) -> bool {
        self.handshake_seen || self.data_seen
    }

    pub fn amp_version(&self) -> Option<&str> {
        self.amp_version.as_deref()
    }
}

pub fn effective_endpoint(cfg: &Config, cache: &Cache, server_id: &str) -> Option<Endpoint> {
    if let Some(declared) = cfg.server_override(server_id) {
        return Some(Endpoint {
            mode: declared.mode,
            port: declared.port,
        });
    }
    let observed = cache.get(server_id)?;
    let port = match observed.advertised_port {
        CONNECTION_PORT => None,
        advertised => Some(u16::try_from(advertised).ok().filter(|port| *port != 0)?),
    };
    Some(Endpoint {
        mode: observed.mode,
        port,
    })
}

pub fn on_login_claim(
    cfg: &Config,
    cache: &Cache,
    server_id: &str,
    login_start_payload: &[u8],
) -> ClaimDecision {
    if !cfg.holepunch {
        return ClaimDecision::Pass;
    }
    let serves_holepunch = matches!(
        effective_endpoint(cfg, cache, server_id),
        Some(Endpoint {
            mode: ConnectionMode::Holepunch,
            ..
        })
    );
    if serves_holepunch && holepunch::login_start_marker(login_start_payload).is_some() {
        ClaimDecision::HandoffRoute
    } else {
        ClaimDecision::Pass
    }
}

pub fn on_raw_connection(
    cfg: &Config,
    cache: &Cache,
    data: &[u8],
    resolve: impl Fn(&str) -> Option<String>,
) -> RawDecision {
    if !cfg.magic {
        return RawDecision::Pass;
    }
    match ammh::parse(data) {
        Ammh::NoMatch => RawDecision::Pass,
        Ammh::NeedMore(total) => RawDecision::NeedMore(u32::try_from(total).unwrap_or(u32::MAX)),
        Ammh::Malformed(problem) => RawDecision::Reject(RejectReason::MalformedPreamble(problem)),
        Ammh::Complete { host, consumed } => {
            let Some(server) = resolve(&ammh::lookup_key(&host)) else {
                return RawDecision::Reject(RejectReason::UnknownHost);
            };
            let Some(lane) = modpack_lane(cfg, cache, &server) else {
                return RawDecision::Reject(RejectReason::NoModpackEndpoint);
            };
            RawDecision::Handoff {
                server,
                port: lane.port,
                consumed,
                replay: lane.backend_answers_magic,
                reply: if lane.backend_answers_magic {
                    Vec::new()
                } else {
                    AMOK.to_vec()
                },
            }
        }
    }
}

pub fn on_handshake_request(watch: &mut LoginWatch, payload: &[u8]) {
    watch.handshake_seen = true;
    if let Some(version) = HandshakeInfo::parse(payload)
        .ok()
        .and_then(|handshake| handshake.am_version)
    {
        watch.amp_version = Some(version);
    }
}

pub fn on_data_request(
    cfg: &Config,
    cache: &mut Cache,
    server_id: &str,
    payload: &[u8],
    watch: &mut LoginWatch,
    now_ms: u64,
) -> DataDecision {
    watch.data_seen = true;
    let Ok(packet) = DataPacket::parse(payload) else {
        return DataDecision::Forward;
    };
    let advertised = packet.endpoint();
    cache.record(
        server_id,
        ServerEntry {
            mode: advertised.mode,
            advertised_port: advertised.port,
            advertised_host: advertised.host.clone(),
            seen_at_ms: now_ms,
            amp_version: watch.amp_version.clone(),
        },
    );
    if !should_rewrite_to_magic(cfg, cache, server_id, &packet) {
        return DataDecision::Forward;
    }
    match packet.rewritten(Rewrite::ToProxyMagic).to_payload() {
        Ok(rewritten) => DataDecision::Replace(rewritten),
        Err(_) => DataDecision::Forward,
    }
}

pub fn on_login_finished(
    cache: &mut Cache,
    server_id: &str,
    watch: &LoginWatch,
) -> Option<ServerEntry> {
    if watch.saw_automodpack() {
        return None;
    }
    on_login_finished_without_handshake(cache, server_id)
}

pub fn on_login_finished_without_handshake(
    cache: &mut Cache,
    server_id: &str,
) -> Option<ServerEntry> {
    cache.forget_after_login_without_handshake(server_id)
}

fn should_rewrite_to_magic(
    cfg: &Config,
    cache: &Cache,
    server_id: &str,
    packet: &DataPacket,
) -> bool {
    let advertised = packet.endpoint();
    let policy_allows = match cfg.rewrite_to_magic {
        RewriteToMagic::Never => false,
        RewriteToMagic::MagicOnly => advertised.mode == ConnectionMode::Magic,
        RewriteToMagic::Always => {
            matches!(
                advertised.mode,
                ConnectionMode::Magic | ConnectionMode::Http
            )
        }
    };
    let proxy_can_route = modpack_lane(cfg, cache, server_id).is_some();
    cfg.magic && policy_allows && proxy_can_route && !advertised.is_proxy_magic()
}

struct ModpackLane {
    port: Option<u16>,
    backend_answers_magic: bool,
}

fn modpack_lane(cfg: &Config, cache: &Cache, server_id: &str) -> Option<ModpackLane> {
    match effective_endpoint(cfg, cache, server_id)? {
        Endpoint {
            mode: ConnectionMode::Magic,
            port,
        } => Some(ModpackLane {
            port,
            backend_answers_magic: true,
        }),
        Endpoint {
            mode: ConnectionMode::Http,
            port: Some(port),
        } => Some(ModpackLane {
            port: Some(port),
            backend_answers_magic: false,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde_json::{Value, json};

    use super::*;
    use crate::config::ServerOverride;
    use crate::holepunch::test_support::{SplitMix64, hex, uuid_from_hex};
    use crate::holepunch::{MARKER_PROTOCOL_VERSION, encode_marker, login_start_payload};
    use crate::wire;

    const VECTORS: &str = include_str!("../testdata/holepunch_vectors.json");
    const V5_HOLEPUNCH: &str = include_str!("../testdata/gson/v5_holepunch.json");
    const V5_MAGIC_DEDICATED: &str = include_str!("../testdata/gson/v5_magic_dedicated.json");
    const V5_HTTP: &str = include_str!("../testdata/gson/v5_http.json");
    const V4_DIRECT: &str = include_str!("../testdata/gson/v4_direct.json");
    const V4_DIRECT_VELOCITY: &str =
        include_str!("../testdata/gson/v4_direct_velocity_rewritten.json");
    const HANDSHAKE_V5: &str = include_str!("../testdata/gson/handshake_v5.json");

    fn payload(json: &str) -> Vec<u8> {
        wire::single_string_payload(json).unwrap()
    }

    fn reparsed(payload: &[u8]) -> Value {
        serde_json::from_str(&wire::read_single_string(payload).unwrap()).unwrap()
    }

    fn observed(mode: ConnectionMode, port: i32) -> ServerEntry {
        ServerEntry {
            mode,
            advertised_port: port,
            advertised_host: String::new(),
            seen_at_ms: 1,
            amp_version: None,
        }
    }

    fn cache_with(server: &str, mode: ConnectionMode, port: i32) -> Cache {
        let mut cache = Cache::new();
        cache.record(server, observed(mode, port));
        cache
    }

    fn config_declaring(server: &str, mode: ConnectionMode, port: Option<u16>) -> Config {
        let mut cfg = Config::default();
        cfg.servers
            .insert(server.to_owned(), ServerOverride { mode, port });
        cfg
    }

    fn marker_login(nonce: u32) -> Vec<u8> {
        login_start_payload(
            "q8f2ks0d",
            &encode_marker(MARKER_PROTOCOL_VERSION, 0, nonce),
        )
    }

    fn player_login() -> Vec<u8> {
        login_start_payload("Notch", &uuid_from_hex("069a79f444e94726a5befca90e38aaf5"))
    }

    fn ammh_for(host: &str) -> Vec<u8> {
        ammh::encode(host).unwrap()
    }

    fn resolve_play(host: &str) -> Option<String> {
        (host == "play.example.com").then(|| "lobby".to_owned())
    }

    #[test]
    fn marker_login_to_a_server_seen_in_holepunch_mode_is_handed_off() {
        let cache = cache_with("lobby", ConnectionMode::Holepunch, -1);
        assert_eq!(
            on_login_claim(&Config::default(), &cache, "lobby", &marker_login(5)),
            ClaimDecision::HandoffRoute
        );
    }

    #[test]
    fn marker_login_to_an_unknown_server_is_never_handed_off() {
        let cache = cache_with("lobby", ConnectionMode::Holepunch, -1);
        assert_eq!(
            on_login_claim(&Config::default(), &Cache::new(), "lobby", &marker_login(5)),
            ClaimDecision::Pass
        );
        assert_eq!(
            on_login_claim(&Config::default(), &cache, "survival", &marker_login(5)),
            ClaimDecision::Pass
        );
    }

    #[test]
    fn marker_login_to_a_magic_or_http_server_is_never_handed_off() {
        for (mode, port) in [(ConnectionMode::Magic, -1), (ConnectionMode::Http, 30037)] {
            let cache = cache_with("lobby", mode, port);
            assert_eq!(
                on_login_claim(&Config::default(), &cache, "lobby", &marker_login(5)),
                ClaimDecision::Pass,
                "{mode:?}"
            );
        }
    }

    #[test]
    fn a_config_declaration_enables_and_overrides_the_handoff() {
        let declared = config_declaring("lobby", ConnectionMode::Holepunch, None);
        assert_eq!(
            on_login_claim(&declared, &Cache::new(), "lobby", &marker_login(5)),
            ClaimDecision::HandoffRoute
        );

        let declared_magic = config_declaring("lobby", ConnectionMode::Magic, None);
        let seen_holepunch = cache_with("lobby", ConnectionMode::Holepunch, -1);
        assert_eq!(
            on_login_claim(&declared_magic, &seen_holepunch, "lobby", &marker_login(5)),
            ClaimDecision::Pass
        );
    }

    #[test]
    fn regular_player_logins_always_pass() {
        let cache = cache_with("lobby", ConnectionMode::Holepunch, -1);
        assert_eq!(
            on_login_claim(&Config::default(), &cache, "lobby", &player_login()),
            ClaimDecision::Pass
        );
        let mut rng = SplitMix64::new(0x5345_4355_5249_5459);
        for _ in 0..20_000 {
            let mut uuid = rng.next_uuid();
            uuid[6] = (uuid[6] & 0x0f) | 0x40;
            uuid[8] = (uuid[8] & 0x3f) | 0x80;
            assert_eq!(
                on_login_claim(
                    &Config::default(),
                    &cache,
                    "lobby",
                    &login_start_payload("Steve", &uuid)
                ),
                ClaimDecision::Pass
            );
        }
    }

    #[test]
    fn disabling_holepunch_passes_every_login() {
        let cfg = Config {
            holepunch: false,
            ..config_declaring("lobby", ConnectionMode::Holepunch, None)
        };
        let cache = cache_with("lobby", ConnectionMode::Holepunch, -1);
        assert_eq!(
            on_login_claim(&cfg, &cache, "lobby", &marker_login(5)),
            ClaimDecision::Pass
        );
    }

    #[test]
    fn a_plain_login_revokes_the_holepunch_handoff() {
        let mut cache = cache_with("lobby", ConnectionMode::Holepunch, -1);
        let forgotten = on_login_finished(&mut cache, "lobby", &LoginWatch::new());
        assert_eq!(
            forgotten.map(|entry| entry.mode),
            Some(ConnectionMode::Holepunch)
        );
        assert_eq!(
            on_login_claim(&Config::default(), &cache, "lobby", &marker_login(5)),
            ClaimDecision::Pass
        );
    }

    #[test]
    fn jar_login_start_frames_are_handed_off_exactly_when_the_jar_accepts_them() {
        #[derive(Deserialize)]
        struct Frames {
            login_start_frames: Vec<Frame>,
        }
        #[derive(Deserialize)]
        struct Frame {
            frame: String,
            jar_decodes: bool,
        }
        let cache = cache_with("lobby", ConnectionMode::Holepunch, -1);
        let frames: Frames = serde_json::from_str(VECTORS).unwrap();
        for vector in frames.login_start_frames {
            let frame = hex(&vector.frame);
            let (_, length_prefix) = wire::read_var_int(&frame).unwrap();
            let payload = &frame[length_prefix + 1..];
            let expected = if vector.jar_decodes {
                ClaimDecision::HandoffRoute
            } else {
                ClaimDecision::Pass
            };
            assert_eq!(
                on_login_claim(&Config::default(), &cache, "lobby", payload),
                expected
            );
        }
    }

    #[test]
    fn raw_traffic_without_the_magic_passes() {
        let cache = cache_with("lobby", ConnectionMode::Magic, -1);
        let handshake = [0x10, 0x00, 0xf2, 0x05, 0x09];
        assert_eq!(
            on_raw_connection(&Config::default(), &cache, &handshake, resolve_play),
            RawDecision::Pass
        );
        assert_eq!(
            on_raw_connection(&Config::default(), &cache, &[0xfe, 0x01], resolve_play),
            RawDecision::Pass
        );
    }

    #[test]
    fn raw_magic_is_ignored_when_magic_is_disabled() {
        let cfg = Config {
            magic: false,
            ..Config::default()
        };
        let cache = cache_with("lobby", ConnectionMode::Magic, -1);
        assert_eq!(
            on_raw_connection(&cfg, &cache, &ammh_for("play.example.com"), resolve_play),
            RawDecision::Pass
        );
    }

    #[test]
    fn partial_preambles_ask_for_the_total_length() {
        let cache = cache_with("lobby", ConnectionMode::Magic, -1);
        let preamble = ammh_for("play.example.com");
        assert_eq!(
            on_raw_connection(&Config::default(), &cache, &preamble[..2], resolve_play),
            RawDecision::NeedMore(6)
        );
        assert_eq!(
            on_raw_connection(&Config::default(), &cache, &preamble[..7], resolve_play),
            RawDecision::NeedMore(preamble.len() as u32)
        );
    }

    #[test]
    fn malformed_preambles_and_unknown_hosts_are_rejected() {
        let cache = cache_with("lobby", ConnectionMode::Magic, -1);
        assert_eq!(
            on_raw_connection(&Config::default(), &cache, b"AMMH\x00\x00", resolve_play),
            RawDecision::Reject(RejectReason::MalformedPreamble(MalformedHost::Empty))
        );
        assert_eq!(
            on_raw_connection(
                &Config::default(),
                &cache,
                &ammh_for("other.example.com"),
                resolve_play
            ),
            RawDecision::Reject(RejectReason::UnknownHost)
        );
    }

    #[test]
    fn the_resolver_sees_the_normalised_host() {
        let cache = cache_with("lobby", ConnectionMode::Magic, -1);
        let seen = std::cell::RefCell::new(Vec::new());
        let decision = on_raw_connection(
            &Config::default(),
            &cache,
            &ammh_for("Play.Example.COM."),
            |host| {
                seen.borrow_mut().push(host.to_owned());
                resolve_play(host)
            },
        );
        assert_eq!(seen.into_inner(), vec!["play.example.com".to_owned()]);
        assert!(matches!(decision, RawDecision::Handoff { .. }));
    }

    #[test]
    fn magic_backends_get_the_preamble_replayed_and_answer_themselves() {
        let mut data = ammh_for("play.example.com");
        let consumed = data.len();
        data.extend_from_slice(&[0x16, 0x03, 0x01, 0x00, 0xc8]);
        let shared = cache_with("lobby", ConnectionMode::Magic, -1);
        assert_eq!(
            on_raw_connection(&Config::default(), &shared, &data, resolve_play),
            RawDecision::Handoff {
                server: "lobby".to_owned(),
                port: None,
                consumed,
                replay: true,
                reply: Vec::new()
            }
        );
        let dedicated = cache_with("lobby", ConnectionMode::Magic, 24454);
        assert_eq!(
            on_raw_connection(&Config::default(), &dedicated, &data, resolve_play),
            RawDecision::Handoff {
                server: "lobby".to_owned(),
                port: Some(24454),
                consumed,
                replay: true,
                reply: Vec::new()
            }
        );
    }

    #[test]
    fn http_backends_get_amok_from_the_proxy_and_no_preamble() {
        let cache = cache_with("lobby", ConnectionMode::Http, 30037);
        let data = ammh_for("play.example.com");
        assert_eq!(
            on_raw_connection(&Config::default(), &cache, &data, resolve_play),
            RawDecision::Handoff {
                server: "lobby".to_owned(),
                port: Some(30037),
                consumed: data.len(),
                replay: false,
                reply: b"AMOK".to_vec()
            }
        );
    }

    #[test]
    fn servers_without_a_reachable_modpack_endpoint_are_rejected() {
        let data = ammh_for("play.example.com");
        for cache in [
            Cache::new(),
            cache_with("lobby", ConnectionMode::Holepunch, -1),
            cache_with("lobby", ConnectionMode::Http, -1),
            cache_with("lobby", ConnectionMode::Magic, 0),
            cache_with("lobby", ConnectionMode::Magic, 70_000),
            cache_with("lobby", ConnectionMode::Magic, -2),
        ] {
            assert_eq!(
                on_raw_connection(&Config::default(), &cache, &data, resolve_play),
                RawDecision::Reject(RejectReason::NoModpackEndpoint),
                "{cache:?}"
            );
        }
    }

    #[test]
    fn a_declared_port_wins_over_the_advertised_one() {
        let cfg = config_declaring("lobby", ConnectionMode::Magic, Some(25000));
        let cache = cache_with("lobby", ConnectionMode::Magic, 24454);
        assert!(matches!(
            on_raw_connection(&cfg, &cache, &ammh_for("play.example.com"), resolve_play),
            RawDecision::Handoff {
                port: Some(25000),
                ..
            }
        ));
        let http = config_declaring("lobby", ConnectionMode::Http, Some(8443));
        assert!(matches!(
            on_raw_connection(
                &http,
                &Cache::new(),
                &ammh_for("play.example.com"),
                resolve_play
            ),
            RawDecision::Handoff {
                port: Some(8443),
                replay: false,
                ..
            }
        ));
    }

    #[test]
    fn holepunch_data_is_forwarded_and_remembered() {
        let mut cache = Cache::new();
        let mut watch = LoginWatch::new();
        on_handshake_request(&mut watch, &payload(HANDSHAKE_V5));
        let decision = on_data_request(
            &Config::default(),
            &mut cache,
            "lobby",
            &payload(V5_HOLEPUNCH),
            &mut watch,
            42,
        );
        assert_eq!(decision, DataDecision::Forward);
        assert_eq!(
            cache.get("lobby"),
            Some(&ServerEntry {
                mode: ConnectionMode::Holepunch,
                advertised_port: -1,
                advertised_host: String::new(),
                seen_at_ms: 42,
                amp_version: Some("5.0.0-rc.3".to_owned()),
            })
        );
        assert_eq!(
            on_login_claim(&Config::default(), &cache, "lobby", &marker_login(77)),
            ClaimDecision::HandoffRoute
        );
    }

    #[test]
    fn magic_data_pointing_elsewhere_is_rewritten_to_come_back_through_the_proxy() {
        let mut cache = Cache::new();
        let mut watch = LoginWatch::new();
        let decision = on_data_request(
            &Config::default(),
            &mut cache,
            "lobby",
            &payload(V5_MAGIC_DEDICATED),
            &mut watch,
            7,
        );
        let DataDecision::Replace(rewritten) = decision else {
            panic!("expected a rewrite, got {decision:?}");
        };
        let on_wire = reparsed(&rewritten);
        assert_eq!(on_wire["endpointHost"], "");
        assert_eq!(on_wire["endpointPort"], -1);
        assert_eq!(on_wire["connectionMode"], "MAGIC");
        assert_eq!(on_wire["requireModpack"], false);

        let entry = cache.get("lobby").unwrap();
        assert_eq!(entry.advertised_port, 24454);
        assert_eq!(entry.advertised_host, "mc.example.net");

        let mut lane = ammh_for("play.example.com");
        lane.extend_from_slice(&[0x16, 0x03, 0x01]);
        assert!(matches!(
            on_raw_connection(&Config::default(), &cache, &lane, resolve_play),
            RawDecision::Handoff {
                port: Some(24454),
                replay: true,
                ..
            }
        ));
    }

    #[test]
    fn magic_data_already_pointing_at_the_proxy_is_forwarded_untouched() {
        let packet = json!({
            "endpointHost": "",
            "endpointPort": -1,
            "secret": {"secret": "s", "timestamp": 1},
            "connectionMode": "MAGIC",
            "requireModpack": true
        });
        let mut cache = Cache::new();
        assert_eq!(
            on_data_request(
                &Config::default(),
                &mut cache,
                "lobby",
                &payload(&packet.to_string()),
                &mut LoginWatch::new(),
                1
            ),
            DataDecision::Forward
        );
        assert_eq!(cache.get("lobby").unwrap().mode, ConnectionMode::Magic);
    }

    #[test]
    fn http_data_is_rewritten_only_under_the_always_policy() {
        let mut cache = Cache::new();
        assert_eq!(
            on_data_request(
                &Config::default(),
                &mut cache,
                "lobby",
                &payload(V5_HTTP),
                &mut LoginWatch::new(),
                1
            ),
            DataDecision::Forward
        );
        let always = Config {
            rewrite_to_magic: RewriteToMagic::Always,
            ..Config::default()
        };
        let decision = on_data_request(
            &always,
            &mut cache,
            "lobby",
            &payload(V5_HTTP),
            &mut LoginWatch::new(),
            2,
        );
        let DataDecision::Replace(rewritten) = decision else {
            panic!("expected a rewrite, got {decision:?}");
        };
        assert_eq!(reparsed(&rewritten)["connectionMode"], "MAGIC");
        assert_eq!(
            on_raw_connection(&always, &cache, &ammh_for("play.example.com"), resolve_play),
            RawDecision::Handoff {
                server: "lobby".to_owned(),
                port: Some(30037),
                consumed: ammh_for("play.example.com").len(),
                replay: false,
                reply: AMOK.to_vec()
            }
        );
    }

    #[test]
    fn http_data_without_a_known_port_is_never_rewritten() {
        let packet = json!({"endpointHost": "", "endpointPort": -1, "connectionMode": "HTTP"});
        let always = Config {
            rewrite_to_magic: RewriteToMagic::Always,
            ..Config::default()
        };
        assert_eq!(
            on_data_request(
                &always,
                &mut Cache::new(),
                "lobby",
                &payload(&packet.to_string()),
                &mut LoginWatch::new(),
                1
            ),
            DataDecision::Forward
        );
        let declared = Config {
            rewrite_to_magic: RewriteToMagic::Always,
            ..config_declaring("lobby", ConnectionMode::Http, Some(30037))
        };
        assert!(matches!(
            on_data_request(
                &declared,
                &mut Cache::new(),
                "lobby",
                &payload(&packet.to_string()),
                &mut LoginWatch::new(),
                1
            ),
            DataDecision::Replace(_)
        ));
    }

    #[test]
    fn holepunch_data_is_never_rewritten_even_under_always() {
        let always = Config {
            rewrite_to_magic: RewriteToMagic::Always,
            ..Config::default()
        };
        assert_eq!(
            on_data_request(
                &always,
                &mut Cache::new(),
                "lobby",
                &payload(V5_HOLEPUNCH),
                &mut LoginWatch::new(),
                1
            ),
            DataDecision::Forward
        );
    }

    #[test]
    fn never_policy_and_disabled_magic_forward_everything() {
        for cfg in [
            Config {
                rewrite_to_magic: RewriteToMagic::Never,
                ..Config::default()
            },
            Config {
                magic: false,
                rewrite_to_magic: RewriteToMagic::Always,
                ..Config::default()
            },
        ] {
            for json in [V5_MAGIC_DEDICATED, V5_HTTP, V4_DIRECT] {
                let mut cache = Cache::new();
                assert_eq!(
                    on_data_request(
                        &cfg,
                        &mut cache,
                        "lobby",
                        &payload(json),
                        &mut LoginWatch::new(),
                        1
                    ),
                    DataDecision::Forward
                );
                assert_eq!(cache.len(), 1);
            }
        }
    }

    #[test]
    fn a_declared_mode_without_a_magic_route_blocks_the_rewrite() {
        let cfg = config_declaring("lobby", ConnectionMode::Holepunch, None);
        let mut cache = Cache::new();
        assert_eq!(
            on_data_request(
                &cfg,
                &mut cache,
                "lobby",
                &payload(V5_MAGIC_DEDICATED),
                &mut LoginWatch::new(),
                1
            ),
            DataDecision::Forward
        );
        assert_eq!(cache.get("lobby").unwrap().mode, ConnectionMode::Magic);
    }

    #[test]
    fn v4_data_follows_the_same_policy_as_the_velocity_bridge() {
        let always = Config {
            rewrite_to_magic: RewriteToMagic::Always,
            ..Config::default()
        };
        let mut cache = Cache::new();
        let decision = on_data_request(
            &always,
            &mut cache,
            "legacy",
            &payload(V4_DIRECT),
            &mut LoginWatch::new(),
            1,
        );
        let DataDecision::Replace(rewritten) = decision else {
            panic!("expected a rewrite, got {decision:?}");
        };
        let velocity: Value = serde_json::from_str(V4_DIRECT_VELOCITY).unwrap();
        assert_eq!(reparsed(&rewritten), velocity);
        assert_eq!(
            cache
                .get("legacy")
                .map(|entry| (entry.mode, entry.advertised_port)),
            Some((ConnectionMode::Http, 25570))
        );

        assert_eq!(
            on_data_request(
                &Config::default(),
                &mut Cache::new(),
                "legacy",
                &payload(V4_DIRECT),
                &mut LoginWatch::new(),
                1
            ),
            DataDecision::Forward
        );
    }

    #[test]
    fn unreadable_data_is_forwarded_and_teaches_nothing() {
        let mut cache = cache_with("lobby", ConnectionMode::Holepunch, -1);
        let before = cache.clone();
        for garbage in [
            payload("not json"),
            payload("{}"),
            payload(r#"{"connectionMode":"DIRECT","endpointPort":-1}"#),
            vec![0xff],
            Vec::new(),
        ] {
            assert_eq!(
                on_data_request(
                    &Config::default(),
                    &mut cache,
                    "lobby",
                    &garbage,
                    &mut LoginWatch::new(),
                    9
                ),
                DataDecision::Forward
            );
        }
        assert_eq!(cache, before);
    }

    #[test]
    fn the_watch_remembers_what_crossed_the_login() {
        let mut watch = LoginWatch::new();
        assert!(!watch.saw_automodpack());
        on_handshake_request(&mut watch, &[0xff]);
        assert!(watch.handshake_seen());
        assert_eq!(watch.amp_version(), None);
        on_handshake_request(&mut watch, &payload(HANDSHAKE_V5));
        assert_eq!(watch.amp_version(), Some("5.0.0-rc.3"));

        let mut data_only = LoginWatch::new();
        on_data_request(
            &Config::default(),
            &mut Cache::new(),
            "lobby",
            &payload(V5_HOLEPUNCH),
            &mut data_only,
            1,
        );
        assert!(!data_only.handshake_seen());
        assert!(data_only.saw_automodpack());
    }

    #[test]
    fn a_login_that_saw_automodpack_keeps_the_server() {
        let mut cache = cache_with("lobby", ConnectionMode::Holepunch, -1);
        let mut watch = LoginWatch::new();
        on_handshake_request(&mut watch, &payload(HANDSHAKE_V5));
        assert_eq!(on_login_finished(&mut cache, "lobby", &watch), None);
        assert!(cache.get("lobby").is_some());
        assert!(on_login_finished_without_handshake(&mut cache, "lobby").is_some());
        assert!(cache.get("lobby").is_none());
    }
}
