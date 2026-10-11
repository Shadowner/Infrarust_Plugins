use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use infrarust_api::event::ResultedEvent;
use infrarust_api::events::claim::{
    LoginClaimEvent, LoginClaimResult, RawConnectionEvent, RawConnectionResult,
};
use infrarust_api::events::connection::ServerConnectedEvent;
use infrarust_api::events::lifecycle::{DisconnectCause, DisconnectEvent};
use infrarust_api::events::messaging::{
    LoginPluginRequestEvent, LoginPluginRequestResult, LoginPluginResponseEvent,
    LoginPluginResponseResult,
};
use infrarust_api::handoff::{Handoff, HandoffTarget};
use infrarust_api::player::Player;
use infrarust_api::test_util::{MockConfigService, MockPlayer};
use infrarust_api::types::{ProtocolVersion, ServerId};
use infrarust_core::event_bus::EventBusImpl;
use infrarust_core::handoff::ConnectionClaims;
use infrarust_core::plugin::manager::{PluginManager, PluginServices};
use infrarust_core::plugin::{PluginContextFactoryImpl, PluginPermissions};
use infrarust_loader_wasm::{WasmLoaderConfig, WasmPluginLoader, build_engine};
use infrarust_plugin_automodpack::ammh::{self, AMMH, AMOK};
use infrarust_plugin_automodpack::login_data::{DATA_CHANNEL, DataPacket, HANDSHAKE_CHANNEL};
use infrarust_plugin_automodpack::wire;

const PLUGIN: &str = "automodpack";
const COMPONENT: &str = env!("AUTOMODPACK_WASM");
const BOTH: &[&str] = &["connection-handoff", "plugin-messaging"];

const HANDSHAKE_V5: &str = include_str!("../../testdata/gson/handshake_v5.json");
const V5_MAGIC_DEDICATED: &str = include_str!("../../testdata/gson/v5_magic_dedicated.json");
const V5_HOLEPUNCH: &str = include_str!("../../testdata/gson/v5_holepunch.json");
const V5_HTTP: &str = include_str!("../../testdata/gson/v5_http.json");

const JAR_MARKER_LOGIN_START: &str = "0161a3ce9ccb6b0e48ed8d704ac9fc578b61";
const NOTCH_LOGIN_START: &str = "054e6f746368069a79f444e94726a5befca90e38aaf5";

struct Proxy {
    _tmp: tempfile::TempDir,
    data_dir: PathBuf,
    bus: Arc<EventBusImpl>,
    claims: Arc<ConnectionClaims>,
    errors: Vec<String>,
    _manager: PluginManager,
}

struct Setup<'a> {
    grants: &'a [&'a str],
    files: &'a [(&'a str, &'a str)],
}

impl Default for Setup<'_> {
    fn default() -> Self {
        Setup {
            grants: BOTH,
            files: &[],
        }
    }
}

fn server(id: &str, domain: &str) -> infrarust_api::services::config_service::ServerConfig {
    let mut config = MockConfigService::server(id);
    config.domains = vec![domain.to_owned()];
    config
}

fn loader() -> WasmPluginLoader {
    let config: infrarust_config::ProxyConfig = toml::from_str("").unwrap();
    let loader_config = WasmLoaderConfig::from_proxy_config(&config).with_cache_dir(None);
    WasmPluginLoader::new(build_engine(&config).unwrap(), loader_config).unwrap()
}

async fn start(setup: Setup<'_>) -> Proxy {
    let tmp = tempfile::tempdir().unwrap();
    let plugins_dir = tmp.path().to_path_buf();
    fs::copy(COMPONENT, plugins_dir.join("automodpack.wasm")).unwrap_or_else(|error| {
        panic!("copying the component from {COMPONENT}: {error}");
    });
    let data_dir = plugins_dir.join(PLUGIN);
    fs::create_dir_all(&data_dir).unwrap();
    for (name, content) in setup.files {
        fs::write(data_dir.join(name), content).unwrap();
    }

    let bus = Arc::new(EventBusImpl::new());
    let config = MockConfigService::new()
        .with_server(server("lobby", "play.example.com"))
        .with_server(server("modded", "modded.example.com"))
        .with_server(server("files", "files.example.com"));
    let services = PluginServices {
        config_service: Arc::new(config),
        plugins_dir: plugins_dir.clone(),
        ..PluginServices::for_tests_with(Arc::clone(&bus))
    };
    let claims = Arc::clone(&services.connection_claims);
    let grants = HashMap::from([(
        PLUGIN.to_owned(),
        PluginPermissions {
            permissions: setup
                .grants
                .iter()
                .map(|grant| (*grant).to_owned())
                .collect(),
            ..PluginPermissions::default()
        },
    )]);
    let factory = Arc::new(PluginContextFactoryImpl::new(services, grants));

    let mut manager = PluginManager::new(vec![Box::new(loader())]);
    let discovered = manager.discover_all(&plugins_dir).await.unwrap();
    assert_eq!(
        discovered
            .iter()
            .map(|metadata| metadata.id.as_str())
            .collect::<Vec<_>>(),
        [PLUGIN]
    );
    let errors = manager
        .load_and_enable_all(factory)
        .await
        .into_iter()
        .map(|error| error.to_string())
        .collect();
    Proxy {
        _tmp: tmp,
        data_dir,
        bus,
        claims,
        errors,
        _manager: manager,
    }
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
        .collect()
}

fn payload(json: &str) -> Bytes {
    Bytes::from(wire::single_string_payload(json).unwrap())
}

fn player(id: u64, name: &str) -> Arc<dyn Player> {
    MockPlayer::new(id, name).into_arc()
}

impl Proxy {
    fn assert_enabled(&self) {
        assert!(self.errors.is_empty(), "{:?}", self.errors);
    }

    async fn request(
        &self,
        player_id: u64,
        server: &str,
        channel: &str,
        data: Bytes,
        client_reachable: bool,
    ) -> LoginPluginRequestResult {
        let message_id = if channel == HANDSHAKE_CHANNEL {
            -100
        } else {
            -101
        };
        let event = LoginPluginRequestEvent::new(
            player(player_id, "Steve"),
            ServerId::new(server),
            message_id,
            channel,
            data,
            client_reachable,
        );
        self.bus.fire(event).await.result().clone()
    }

    async fn automodpack_login(
        &self,
        player_id: u64,
        server: &str,
        data_json: &str,
    ) -> LoginPluginRequestResult {
        assert_eq!(
            self.request(
                player_id,
                server,
                HANDSHAKE_CHANNEL,
                payload(HANDSHAKE_V5),
                true
            )
            .await,
            LoginPluginRequestResult::Forward
        );
        self.request(player_id, server, DATA_CHANNEL, payload(data_json), true)
            .await
    }

    async fn connected(&self, player_id: u64, server: &str) {
        let event =
            ServerConnectedEvent::new(player(player_id, "Steve"), ServerId::new(server), None);
        self.bus.fire(event).await;
    }

    async fn disconnected(&self, player_id: u64) {
        let event = DisconnectEvent::new(
            player(player_id, "Steve"),
            None,
            DisconnectCause::ClientQuit,
        );
        self.bus.fire(event).await;
    }

    async fn claim(&self, server: &str, login_start: &str) -> LoginClaimResult {
        let event = LoginClaimEvent::new(
            "203.0.113.7:50000".parse().unwrap(),
            ProtocolVersion::new(767),
            "a",
            Bytes::from(hex(login_start)),
        )
        .with_host(
            format!("{server}.example.com\0FML3\0"),
            Some(format!("{server}.example.com")),
            25565,
        )
        .with_server(Some(ServerId::new(server)));
        self.bus.fire(event).await.result().clone()
    }

    async fn raw(&self, data: Vec<u8>) -> RawConnectionResult {
        let event = RawConnectionEvent::new(
            1,
            "203.0.113.7:50000".parse().unwrap(),
            "0.0.0.0:25565".parse().unwrap(),
            Bytes::from_static(&AMMH),
            data,
        );
        self.bus.fire(event).await.result().clone()
    }

    fn cache_file(&self) -> String {
        fs::read_to_string(self.data_dir.join("server-cache.json")).unwrap_or_default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_component_enables_and_claims_the_ammh_prefix() {
    let proxy = start(Setup::default()).await;
    proxy.assert_enabled();
    assert_eq!(proxy.claims.prefixes(), [Bytes::from_static(&AMMH)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_magic_backend_is_learned_rewritten_and_routed() {
    let proxy = start(Setup::default()).await;
    proxy.assert_enabled();

    let LoginPluginRequestResult::Replace(rewritten) = proxy
        .automodpack_login(7, "lobby", V5_MAGIC_DEDICATED)
        .await
    else {
        panic!("the magic data packet was not rewritten");
    };
    let packet = DataPacket::parse(&rewritten).unwrap();
    assert!(packet.endpoint().is_proxy_magic());
    assert_eq!(packet.fields()["requireModpack"], false);
    assert!(proxy.cache_file().contains("\"advertised_port\": 24454"));
    assert!(proxy.cache_file().contains("\"mode\": \"magic\""));

    let response = LoginPluginResponseEvent::new(
        player(7, "Steve"),
        ServerId::new("lobby"),
        -101,
        DATA_CHANNEL,
        true,
        payload("false"),
    );
    assert_eq!(
        proxy.bus.fire(response).await.result().clone(),
        LoginPluginResponseResult::Forward
    );
    proxy.connected(7, "lobby").await;

    let mut lane = ammh::encode("Play.Example.com").unwrap();
    lane.extend_from_slice(&[0x16, 0x03, 0x01]);
    assert_eq!(
        proxy.raw(lane).await,
        RawConnectionResult::Handoff(
            Handoff::to(HandoffTarget::server_port("lobby", 24454)).with_proxy_protocol(false)
        )
    );
    let preamble = ammh::encode("play.example.com").unwrap();
    assert_eq!(
        proxy.raw(preamble[..5].to_vec()).await,
        RawConnectionResult::NeedMore(6)
    );
    assert_eq!(
        proxy.raw(preamble[..7].to_vec()).await,
        RawConnectionResult::NeedMore(u32::try_from(preamble.len()).unwrap())
    );
    assert_eq!(
        proxy
            .raw(ammh::encode("unknown.example.com").unwrap())
            .await,
        RawConnectionResult::Reject
    );
    assert_eq!(
        proxy.raw(ammh::encode("modded.example.com").unwrap()).await,
        RawConnectionResult::Reject
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_http_backend_declared_in_the_config_gets_amok_from_the_proxy() {
    let proxy = start(Setup {
        files: &[(
            "config.json",
            r#"{"rewrite_to_magic":"always","servers":{"files":{"mode":"http","port":30037}}}"#,
        )],
        ..Setup::default()
    })
    .await;
    proxy.assert_enabled();

    assert!(matches!(
        proxy.automodpack_login(7, "files", V5_HTTP).await,
        LoginPluginRequestResult::Replace(_)
    ));
    let mut lane = ammh::encode("files.example.com").unwrap();
    lane.push(0x16);
    assert_eq!(
        proxy.raw(lane).await,
        RawConnectionResult::Handoff(
            Handoff::to(HandoffTarget::server_port("files", 30037))
                .without_replay()
                .with_prefix(vec![0x16])
                .with_reply(AMOK.to_vec())
                .with_proxy_protocol(false)
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_holepunch_backend_gets_marker_logins_until_a_plain_login_revokes_it() {
    let proxy = start(Setup::default()).await;
    proxy.assert_enabled();

    assert_eq!(
        proxy.claim("modded", JAR_MARKER_LOGIN_START).await,
        LoginClaimResult::Pass
    );
    assert_eq!(
        proxy.automodpack_login(7, "modded", V5_HOLEPUNCH).await,
        LoginPluginRequestResult::Forward
    );
    assert!(proxy.cache_file().contains("\"mode\": \"holepunch\""));
    proxy.connected(7, "modded").await;

    assert_eq!(
        proxy.claim("modded", JAR_MARKER_LOGIN_START).await,
        LoginClaimResult::Handoff(Handoff::route())
    );
    assert_eq!(
        proxy.claim("modded", NOTCH_LOGIN_START).await,
        LoginClaimResult::Pass
    );
    assert_eq!(
        proxy.claim("lobby", JAR_MARKER_LOGIN_START).await,
        LoginClaimResult::Pass
    );

    proxy.connected(8, "modded").await;
    assert_eq!(
        proxy.claim("modded", JAR_MARKER_LOGIN_START).await,
        LoginClaimResult::Pass
    );
    assert!(!proxy.cache_file().contains("modded"));
    proxy.disconnected(7).await;
    proxy.disconnected(8).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_past_its_login_keeps_the_backend_packet() {
    let proxy = start(Setup::default()).await;
    proxy.assert_enabled();
    assert_eq!(
        proxy
            .request(7, "lobby", DATA_CHANNEL, payload(V5_MAGIC_DEDICATED), false)
            .await,
        LoginPluginRequestResult::Forward
    );
    assert!(proxy.cache_file().contains("lobby"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_remembers_what_the_backends_advertised() {
    let cache = r#"{"version":1,"servers":{"modded":{"mode":"holepunch","advertised_port":-1,"advertised_host":"","seen_at_ms":1}}}"#;
    let proxy = start(Setup {
        files: &[("server-cache.json", cache)],
        ..Setup::default()
    })
    .await;
    proxy.assert_enabled();
    assert_eq!(
        proxy.claim("modded", JAR_MARKER_LOGIN_START).await,
        LoginClaimResult::Handoff(Handoff::route())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn disabled_modes_leave_their_hooks_alone() {
    let cache = r#"{"version":1,"servers":{"modded":{"mode":"holepunch","advertised_port":-1,"advertised_host":"","seen_at_ms":1},"lobby":{"mode":"magic","advertised_port":-1,"advertised_host":"","seen_at_ms":1}}}"#;
    let proxy = start(Setup {
        files: &[
            ("config.json", r#"{"holepunch":false,"magic":false}"#),
            ("server-cache.json", cache),
        ],
        ..Setup::default()
    })
    .await;
    proxy.assert_enabled();
    assert!(proxy.claims.prefixes().is_empty());
    assert_eq!(
        proxy.claim("modded", JAR_MARKER_LOGIN_START).await,
        LoginClaimResult::Pass
    );
    assert_eq!(
        proxy.raw(ammh::encode("play.example.com").unwrap()).await,
        RawConnectionResult::Pass
    );
    assert_eq!(
        proxy
            .automodpack_login(7, "lobby", V5_MAGIC_DEDICATED)
            .await,
        LoginPluginRequestResult::Forward
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_broken_config_keeps_the_bridge_off() {
    let proxy = start(Setup {
        files: &[("config.json", r#"{"holepuch": false}"#)],
        ..Setup::default()
    })
    .await;
    assert!(
        proxy
            .errors
            .iter()
            .any(|error| error.contains("config.json")),
        "{:?}",
        proxy.errors
    );
    assert!(proxy.claims.prefixes().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_capabilities_keep_the_bridge_off_and_name_the_grant() {
    for (grants, missing) in [
        (&["connection-handoff"][..], "plugin-messaging"),
        (&["plugin-messaging"][..], "connection-handoff"),
    ] {
        let proxy = start(Setup {
            grants,
            ..Setup::default()
        })
        .await;
        assert!(
            proxy.errors.iter().any(|error| error.contains(missing)),
            "{grants:?}: {:?}",
            proxy.errors
        );
    }
}

#[test]
fn the_component_is_the_one_the_tests_load() {
    assert!(Path::new(COMPONENT).is_file(), "{COMPONENT}");
}
