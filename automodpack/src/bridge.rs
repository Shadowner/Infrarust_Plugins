use std::collections::HashMap;
use std::path::{Path, PathBuf};

use infrarust_plugin_sdk::event::{LoginPluginRequestResult, RawConnectionResult};
use infrarust_plugin_sdk::handoff::{Handoff, HandoffTarget};

use crate::ammh::AMMH;
use crate::cache::{CACHE_FILE_NAME, Cache, CacheError, ServerEntry};
use crate::config::{CONFIG_FILE_NAME, Config, ConfigError};
use crate::decide::{self, ClaimDecision, DataDecision, LoginWatch, RawDecision};
use crate::login_data::{ClientAnswer, DATA_CHANNEL, HANDSHAKE_CHANNEL};

pub const CHANNELS: [&str; 2] = [HANDSHAKE_CHANNEL, DATA_CHANNEL];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bridge {
    cfg: Config,
    cache: Cache,
    logins: HashMap<u64, PendingLogin>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingLogin {
    server: String,
    watch: LoginWatch,
}

#[derive(Debug)]
pub struct Loaded {
    pub bridge: Bridge,
    pub config_found: bool,
    pub cache_error: Option<CacheError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Learned {
    Nothing,
    New,
    Changed,
    Refreshed,
}

impl Learned {
    pub fn between(before: Option<&ServerEntry>, after: Option<&ServerEntry>) -> Learned {
        match (before, after) {
            (_, None) => Learned::Nothing,
            (None, Some(_)) => Learned::New,
            (Some(before), Some(after)) if before == after => Learned::Nothing,
            (Some(before), Some(after)) if same_observation(before, after) => Learned::Refreshed,
            (Some(_), Some(_)) => Learned::Changed,
        }
    }

    pub fn cache_changed(self) -> bool {
        self != Learned::Nothing
    }
}

fn same_observation(left: &ServerEntry, right: &ServerEntry) -> bool {
    left.mode == right.mode
        && left.advertised_port == right.advertised_port
        && left.advertised_host == right.advertised_host
        && left.amp_version == right.amp_version
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginRequest {
    Ignored,
    Handshake,
    Data {
        decision: DataDecision,
        learned: Learned,
    },
}

pub fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join(CONFIG_FILE_NAME)
}

pub fn cache_path(data_dir: &Path) -> PathBuf {
    data_dir.join(CACHE_FILE_NAME)
}

impl Bridge {
    pub fn new(cfg: Config, cache: Cache) -> Bridge {
        Bridge {
            cfg,
            cache,
            logins: HashMap::new(),
        }
    }

    pub fn load(data_dir: &Path) -> Result<Loaded, ConfigError> {
        let cfg = Config::load(&config_path(data_dir))?;
        let (cache, cache_error) = Cache::load(&cache_path(data_dir));
        Ok(Loaded {
            config_found: cfg.is_some(),
            bridge: Bridge::new(cfg.unwrap_or_default(), cache),
            cache_error,
        })
    }

    pub fn save_cache(&self, data_dir: &Path) -> Result<(), CacheError> {
        self.cache.save(&cache_path(data_dir))
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn cache(&self) -> &Cache {
        &self.cache
    }

    pub fn pending_logins(&self) -> usize {
        self.logins.len()
    }

    pub fn login_claim(&self, server: Option<&str>, login_start: &[u8]) -> Option<Handoff> {
        match decide::on_login_claim(&self.cfg, &self.cache, server?, login_start) {
            ClaimDecision::HandoffRoute => Some(Handoff::route()),
            ClaimDecision::Pass => None,
        }
    }

    pub fn raw_connection(
        &self,
        prefix: &[u8],
        data: &[u8],
        resolve: impl Fn(&str) -> Option<String>,
    ) -> RawDecision {
        if prefix != AMMH {
            return RawDecision::Pass;
        }
        decide::on_raw_connection(&self.cfg, &self.cache, data, resolve)
    }

    pub fn raw_result(&self, decision: RawDecision, data: &[u8]) -> Option<RawConnectionResult> {
        raw_connection_result(decision, data, self.cfg.proxy_protocol_to_backend)
    }

    pub fn login_request(
        &mut self,
        player: u64,
        server: &str,
        channel: &str,
        payload: &[u8],
        now_ms: u64,
    ) -> LoginRequest {
        match channel {
            HANDSHAKE_CHANNEL => {
                decide::on_handshake_request(watch_for(&mut self.logins, player, server), payload);
                LoginRequest::Handshake
            }
            DATA_CHANNEL => {
                let before = self.cache.get(server).cloned();
                let decision = decide::on_data_request(
                    &self.cfg,
                    &mut self.cache,
                    server,
                    payload,
                    watch_for(&mut self.logins, player, server),
                    now_ms,
                );
                LoginRequest::Data {
                    decision,
                    learned: Learned::between(before.as_ref(), self.cache.get(server)),
                }
            }
            _ => LoginRequest::Ignored,
        }
    }

    pub fn server_connected(&mut self, player: u64, server: &str) -> Option<ServerEntry> {
        let watch = match self.logins.remove(&player) {
            Some(pending) if pending.server == server => pending.watch,
            _ => LoginWatch::new(),
        };
        decide::on_login_finished(&mut self.cache, server, &watch)
    }

    pub fn disconnected(&mut self, player: u64) {
        self.logins.remove(&player);
    }
}

fn watch_for<'a>(
    logins: &'a mut HashMap<u64, PendingLogin>,
    player: u64,
    server: &str,
) -> &'a mut LoginWatch {
    let pending = logins.entry(player).or_insert_with(|| PendingLogin {
        server: server.to_owned(),
        watch: LoginWatch::new(),
    });
    if pending.server != server {
        *pending = PendingLogin {
            server: server.to_owned(),
            watch: LoginWatch::new(),
        };
    }
    &mut pending.watch
}

pub fn request_result(
    outcome: &LoginRequest,
    client_reachable: bool,
) -> Option<LoginPluginRequestResult> {
    match outcome {
        LoginRequest::Data {
            decision: DataDecision::Replace(payload),
            ..
        } if client_reachable => Some(LoginPluginRequestResult::Replace(payload.clone())),
        _ => None,
    }
}

pub fn client_answer(channel: &str, successful: bool, data: &[u8]) -> Option<ClientAnswer> {
    (channel == DATA_CHANNEL).then(|| ClientAnswer::read(successful.then_some(data)))
}

pub fn raw_connection_result(
    decision: RawDecision,
    data: &[u8],
    proxy_protocol_to_backend: bool,
) -> Option<RawConnectionResult> {
    match decision {
        RawDecision::Pass => None,
        RawDecision::NeedMore(total) => Some(RawConnectionResult::NeedMore(total)),
        RawDecision::Reject(_) => Some(RawConnectionResult::Reject),
        RawDecision::Handoff {
            server,
            port,
            consumed,
            replay,
            reply,
        } => {
            let target = match port {
                None => HandoffTarget::server(server),
                Some(port) => HandoffTarget::server_port(server, port),
            };
            let mut handoff = Handoff::to(target);
            if !replay {
                handoff = handoff
                    .without_replay()
                    .with_prefix(data.get(consumed..).unwrap_or_default());
            }
            if !reply.is_empty() {
                handoff = handoff.with_reply(reply);
            }
            if proxy_protocol_to_backend {
                handoff = handoff.with_proxy_protocol(true);
            } else if port.is_some() {
                handoff = handoff.with_proxy_protocol(false);
            }
            Some(RawConnectionResult::Handoff(handoff))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::ammh::{self, AMOK};
    use crate::config::ServerOverride;
    use crate::decide::RejectReason;
    use crate::holepunch::test_support::uuid_from_hex;
    use crate::holepunch::{MARKER_PROTOCOL_VERSION, encode_marker, login_start_payload};
    use crate::mode::ConnectionMode;
    use crate::wire;

    const V5_HOLEPUNCH: &str = include_str!("../testdata/gson/v5_holepunch.json");
    const V5_MAGIC_DEDICATED: &str = include_str!("../testdata/gson/v5_magic_dedicated.json");
    const V5_HTTP: &str = include_str!("../testdata/gson/v5_http.json");
    const HANDSHAKE_V5: &str = include_str!("../testdata/gson/handshake_v5.json");

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            let dir = std::env::temp_dir().join(format!(
                "infrarust-automodpack-bridge-{}-{name}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn payload(json: &str) -> Vec<u8> {
        wire::single_string_payload(json).unwrap()
    }

    fn entry(mode: ConnectionMode, port: i32) -> ServerEntry {
        ServerEntry {
            mode,
            advertised_port: port,
            advertised_host: String::new(),
            seen_at_ms: 1,
            amp_version: None,
        }
    }

    fn bridge_with(server: &str, mode: ConnectionMode, port: i32) -> Bridge {
        let mut cache = Cache::new();
        cache.record(server, entry(mode, port));
        Bridge::new(Config::default(), cache)
    }

    fn marker_login() -> Vec<u8> {
        login_start_payload("q8f2ks0d", &encode_marker(MARKER_PROTOCOL_VERSION, 0, 9))
    }

    fn player_login() -> Vec<u8> {
        login_start_payload("Notch", &uuid_from_hex("069a79f444e94726a5befca90e38aaf5"))
    }

    fn resolve_play(host: &str) -> Option<String> {
        (host == "play.example.com").then(|| "lobby".to_owned())
    }

    fn raw(bridge: &Bridge, data: &[u8]) -> Option<RawConnectionResult> {
        bridge.raw_result(bridge.raw_connection(&AMMH, data, resolve_play), data)
    }

    fn handoff_of(result: Option<RawConnectionResult>) -> Handoff {
        match result {
            Some(RawConnectionResult::Handoff(handoff)) => handoff,
            other => panic!("expected a handoff, got {other:?}"),
        }
    }

    #[test]
    fn a_marker_login_to_a_holepunch_server_is_routed_with_its_bytes_replayed() {
        let bridge = bridge_with("lobby", ConnectionMode::Holepunch, -1);
        let handoff = bridge.login_claim(Some("lobby"), &marker_login()).unwrap();
        assert_eq!(handoff, Handoff::route());
        assert_eq!(handoff.target, HandoffTarget::Route);
        assert!(handoff.replay);
        assert!(handoff.prefix.is_empty());
        assert!(handoff.reply.is_empty());
        assert_eq!(handoff.proxy_protocol, None);
    }

    #[test]
    fn logins_the_bridge_does_not_own_are_left_alone() {
        let bridge = bridge_with("lobby", ConnectionMode::Holepunch, -1);
        assert_eq!(bridge.login_claim(None, &marker_login()), None);
        assert_eq!(bridge.login_claim(Some("lobby"), &player_login()), None);
        assert_eq!(bridge.login_claim(Some("survival"), &marker_login()), None);
        assert_eq!(
            Bridge::new(Config::default(), Cache::new())
                .login_claim(Some("lobby"), &marker_login()),
            None
        );
        let magic = bridge_with("lobby", ConnectionMode::Magic, -1);
        assert_eq!(magic.login_claim(Some("lobby"), &marker_login()), None);
    }

    #[test]
    fn other_prefixes_and_plain_traffic_are_left_alone() {
        let bridge = bridge_with("lobby", ConnectionMode::Magic, -1);
        let data = ammh::encode("play.example.com").unwrap();
        assert_eq!(
            bridge.raw_connection(b"AMOK", &data, resolve_play),
            RawDecision::Pass
        );
        assert_eq!(bridge.raw_result(RawDecision::Pass, &data), None);
        assert_eq!(raw(&bridge, b"AMMX"), None);
    }

    #[test]
    fn partial_and_malformed_preambles_map_to_need_more_and_reject() {
        let bridge = bridge_with("lobby", ConnectionMode::Magic, -1);
        let data = ammh::encode("play.example.com").unwrap();
        assert_eq!(
            raw(&bridge, &data[..4]),
            Some(RawConnectionResult::NeedMore(6))
        );
        assert_eq!(
            raw(&bridge, &data[..8]),
            Some(RawConnectionResult::NeedMore(data.len() as u32))
        );
        assert_eq!(
            raw(&bridge, b"AMMH\x00\x00"),
            Some(RawConnectionResult::Reject)
        );
        assert_eq!(
            raw(&bridge, &ammh::encode("other.example.com").unwrap()),
            Some(RawConnectionResult::Reject)
        );
        assert_eq!(
            bridge.raw_connection(
                &AMMH,
                &ammh::encode("other.example.com").unwrap(),
                resolve_play
            ),
            RawDecision::Reject(RejectReason::UnknownHost)
        );
    }

    #[test]
    fn hosts_without_a_known_modpack_lane_are_rejected() {
        let data = ammh::encode("play.example.com").unwrap();
        for bridge in [
            Bridge::new(Config::default(), Cache::new()),
            bridge_with("lobby", ConnectionMode::Holepunch, -1),
            bridge_with("lobby", ConnectionMode::Http, -1),
        ] {
            assert_eq!(raw(&bridge, &data), Some(RawConnectionResult::Reject));
        }
    }

    #[test]
    fn a_magic_backend_on_its_minecraft_port_gets_the_preamble_and_the_server_proxy_setting() {
        let bridge = bridge_with("lobby", ConnectionMode::Magic, -1);
        let mut data = ammh::encode("play.example.com").unwrap();
        data.extend_from_slice(&[0x16, 0x03, 0x01]);
        let handoff = handoff_of(raw(&bridge, &data));
        assert_eq!(handoff, Handoff::to(HandoffTarget::server("lobby")));
        assert!(handoff.replay);
        assert!(handoff.prefix.is_empty());
        assert!(handoff.reply.is_empty());
        assert_eq!(handoff.proxy_protocol, None);
    }

    #[test]
    fn a_dedicated_magic_port_is_targeted_on_the_server_host() {
        let bridge = bridge_with("lobby", ConnectionMode::Magic, 24454);
        let data = ammh::encode("play.example.com").unwrap();
        assert_eq!(
            handoff_of(raw(&bridge, &data)),
            Handoff::to(HandoffTarget::server_port("lobby", 24454)).with_proxy_protocol(false)
        );
    }

    #[test]
    fn an_http_backend_gets_amok_from_the_proxy_and_only_the_bytes_after_the_preamble() {
        let bridge = bridge_with("lobby", ConnectionMode::Http, 30037);
        let mut data = ammh::encode("play.example.com").unwrap();
        data.extend_from_slice(&[0x16, 0x03, 0x01]);
        let handoff = handoff_of(raw(&bridge, &data));
        assert_eq!(
            handoff,
            Handoff::to(HandoffTarget::server_port("lobby", 30037))
                .without_replay()
                .with_prefix(vec![0x16, 0x03, 0x01])
                .with_reply(AMOK.to_vec())
                .with_proxy_protocol(false)
        );
        let bare = ammh::encode("play.example.com").unwrap();
        let handoff = handoff_of(raw(&bridge, &bare));
        assert!(!handoff.replay);
        assert!(handoff.prefix.is_empty());
        assert_eq!(handoff.reply, AMOK.to_vec());
    }

    #[test]
    fn proxy_protocol_to_backend_forces_the_header_on_every_lane() {
        for (mode, port) in [
            (ConnectionMode::Magic, -1),
            (ConnectionMode::Magic, 24454),
            (ConnectionMode::Http, 30037),
        ] {
            let mut cache = Cache::new();
            cache.record("lobby", entry(mode, port));
            let cfg = Config {
                proxy_protocol_to_backend: true,
                ..Config::default()
            };
            let bridge = Bridge::new(cfg, cache);
            let data = ammh::encode("play.example.com").unwrap();
            assert_eq!(handoff_of(raw(&bridge, &data)).proxy_protocol, Some(true));
        }
    }

    #[test]
    fn a_declared_server_routes_magic_without_any_observation() {
        let mut cfg = Config::default();
        cfg.servers.insert(
            "lobby".to_owned(),
            ServerOverride {
                mode: ConnectionMode::Magic,
                port: Some(25000),
            },
        );
        let bridge = Bridge::new(cfg, Cache::new());
        let data = ammh::encode("Play.Example.COM.").unwrap();
        assert_eq!(
            handoff_of(raw(&bridge, &data)).target,
            HandoffTarget::server_port("lobby", 25000)
        );
    }

    #[test]
    fn a_magic_data_packet_is_replaced_only_while_the_client_can_receive_it() {
        let mut bridge = Bridge::new(Config::default(), Cache::new());
        assert_eq!(
            bridge.login_request(7, "lobby", HANDSHAKE_CHANNEL, &payload(HANDSHAKE_V5), 1),
            LoginRequest::Handshake
        );
        let outcome =
            bridge.login_request(7, "lobby", DATA_CHANNEL, &payload(V5_MAGIC_DEDICATED), 2);
        let LoginRequest::Data {
            decision: DataDecision::Replace(rewritten),
            learned: Learned::New,
        } = &outcome
        else {
            panic!("expected a rewrite of a new server, got {outcome:?}");
        };
        assert_eq!(
            request_result(&outcome, true),
            Some(LoginPluginRequestResult::Replace(rewritten.clone()))
        );
        assert_eq!(request_result(&outcome, false), None);
        let learned = bridge.cache().get("lobby").unwrap();
        assert_eq!(learned.mode, ConnectionMode::Magic);
        assert_eq!(learned.advertised_port, 24454);
        assert_eq!(learned.amp_version.as_deref(), Some("5.0.0-rc.3"));
    }

    #[test]
    fn forwarded_packets_and_other_channels_leave_the_request_alone() {
        let mut bridge = Bridge::new(Config::default(), Cache::new());
        let holepunch = bridge.login_request(7, "lobby", DATA_CHANNEL, &payload(V5_HOLEPUNCH), 1);
        assert_eq!(
            holepunch,
            LoginRequest::Data {
                decision: DataDecision::Forward,
                learned: Learned::New
            }
        );
        assert_eq!(request_result(&holepunch, true), None);
        assert_eq!(
            bridge.login_request(7, "lobby", "fml:loginwrapper", b"\x01", 2),
            LoginRequest::Ignored
        );
        assert_eq!(request_result(&LoginRequest::Ignored, true), None);
        assert_eq!(request_result(&LoginRequest::Handshake, true), None);
    }

    #[test]
    fn the_cache_reports_what_each_data_packet_taught_it() {
        let mut bridge = Bridge::new(Config::default(), Cache::new());
        let learned = |outcome: LoginRequest| match outcome {
            LoginRequest::Data { learned, .. } => learned,
            other => panic!("expected data, got {other:?}"),
        };
        assert_eq!(
            learned(bridge.login_request(1, "lobby", DATA_CHANNEL, &payload(V5_HTTP), 10)),
            Learned::New
        );
        assert_eq!(
            learned(bridge.login_request(2, "lobby", DATA_CHANNEL, &payload(V5_HTTP), 20)),
            Learned::Refreshed
        );
        assert_eq!(
            learned(bridge.login_request(2, "lobby", DATA_CHANNEL, &payload(V5_HTTP), 20)),
            Learned::Nothing
        );
        assert_eq!(
            learned(bridge.login_request(3, "lobby", DATA_CHANNEL, &payload(V5_HOLEPUNCH), 30)),
            Learned::Changed
        );
        assert_eq!(
            learned(bridge.login_request(4, "lobby", DATA_CHANNEL, &payload("garbage"), 40)),
            Learned::Nothing
        );
        assert!(Learned::Refreshed.cache_changed());
        assert!(!Learned::Nothing.cache_changed());
    }

    #[test]
    fn a_login_that_crossed_automodpack_keeps_the_server() {
        let mut bridge = bridge_with("lobby", ConnectionMode::Holepunch, -1);
        bridge.login_request(7, "lobby", HANDSHAKE_CHANNEL, &payload(HANDSHAKE_V5), 1);
        assert_eq!(bridge.pending_logins(), 1);
        assert_eq!(bridge.server_connected(7, "lobby"), None);
        assert_eq!(bridge.pending_logins(), 0);
        assert!(bridge.login_claim(Some("lobby"), &marker_login()).is_some());
    }

    #[test]
    fn a_login_without_automodpack_forgets_the_server() {
        let mut bridge = bridge_with("lobby", ConnectionMode::Holepunch, -1);
        assert_eq!(
            bridge.server_connected(7, "lobby").map(|entry| entry.mode),
            Some(ConnectionMode::Holepunch)
        );
        assert!(bridge.cache().is_empty());
        assert_eq!(bridge.login_claim(Some("lobby"), &marker_login()), None);
    }

    #[test]
    fn queries_seen_for_another_server_do_not_count_for_this_login() {
        let mut bridge = bridge_with("lobby", ConnectionMode::Holepunch, -1);
        bridge.login_request(7, "survival", HANDSHAKE_CHANNEL, &payload(HANDSHAKE_V5), 1);
        assert!(bridge.server_connected(7, "lobby").is_some());
        assert_eq!(bridge.pending_logins(), 0);

        let mut bridge = bridge_with("lobby", ConnectionMode::Holepunch, -1);
        bridge.login_request(7, "survival", HANDSHAKE_CHANNEL, &payload(HANDSHAKE_V5), 1);
        bridge.login_request(7, "lobby", DATA_CHANNEL, &payload(V5_HOLEPUNCH), 2);
        bridge.login_request(8, "lobby", HANDSHAKE_CHANNEL, &payload(HANDSHAKE_V5), 3);
        assert_eq!(bridge.server_connected(7, "lobby"), None);
        assert_eq!(bridge.pending_logins(), 1);
    }

    #[test]
    fn a_disconnect_drops_the_pending_login() {
        let mut bridge = Bridge::new(Config::default(), Cache::new());
        bridge.login_request(7, "lobby", HANDSHAKE_CHANNEL, &payload(HANDSHAKE_V5), 1);
        bridge.login_request(8, "lobby", HANDSHAKE_CHANNEL, &payload(HANDSHAKE_V5), 1);
        bridge.disconnected(7);
        bridge.disconnected(9);
        assert_eq!(bridge.pending_logins(), 1);
    }

    #[test]
    fn only_data_answers_are_read() {
        let word = |text: &str| wire::single_string_payload(text).unwrap();
        assert_eq!(
            client_answer(DATA_CHANNEL, true, &word("false")),
            Some(ClientAnswer::Continue)
        );
        assert_eq!(
            client_answer(DATA_CHANNEL, true, &word("declined")),
            Some(ClientAnswer::Declined)
        );
        assert_eq!(
            client_answer(DATA_CHANNEL, false, &word("false")),
            Some(ClientAnswer::NoAnswer)
        );
        assert_eq!(client_answer(HANDSHAKE_CHANNEL, true, &word("{}")), None);
    }

    #[test]
    fn a_fresh_data_directory_starts_with_defaults_and_an_empty_cache() {
        let dir = TempDir::new("fresh");
        let loaded = Bridge::load(&dir.0).unwrap();
        assert!(!loaded.config_found);
        assert!(loaded.cache_error.is_none());
        assert_eq!(loaded.bridge.config(), &Config::default());
        assert!(loaded.bridge.cache().is_empty());
    }

    #[test]
    fn a_broken_config_refuses_to_start_and_a_broken_cache_starts_empty() {
        let dir = TempDir::new("broken");
        fs::write(config_path(&dir.0), r#"{"holepuch": false}"#).unwrap();
        assert!(matches!(Bridge::load(&dir.0), Err(ConfigError::Syntax(_))));

        fs::write(config_path(&dir.0), r#"{"magic": false}"#).unwrap();
        fs::write(cache_path(&dir.0), "{").unwrap();
        let loaded = Bridge::load(&dir.0).unwrap();
        assert!(loaded.config_found);
        assert!(!loaded.bridge.config().magic);
        assert!(matches!(loaded.cache_error, Some(CacheError::Corrupt(_))));
        assert!(loaded.bridge.cache().is_empty());
    }

    #[test]
    fn what_the_bridge_learned_survives_a_restart() {
        let dir = TempDir::new("restart");
        let mut bridge = Bridge::load(&dir.0).unwrap().bridge;
        bridge.login_request(7, "lobby", DATA_CHANNEL, &payload(V5_HOLEPUNCH), 5);
        bridge.save_cache(&dir.0).unwrap();

        let restarted = Bridge::load(&dir.0).unwrap();
        assert!(restarted.cache_error.is_none());
        assert_eq!(restarted.bridge.cache(), bridge.cache());
        assert!(
            restarted
                .bridge
                .login_claim(Some("lobby"), &marker_login())
                .is_some()
        );
    }
}
