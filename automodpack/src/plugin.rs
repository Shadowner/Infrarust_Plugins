use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use infrarust_plugin_sdk::context::Context;
use infrarust_plugin_sdk::error::{Error, ErrorKind, PluginError};
use infrarust_plugin_sdk::event::{
    DisconnectEvent, EventPriority, LoginClaimEvent, LoginPluginRequestEvent,
    LoginPluginResponseEvent, RawConnectionEvent, ServerConnectedEvent,
};
use infrarust_plugin_sdk::plugin::Plugin;
use infrarust_plugin_sdk::services::{Config as ProxyConfig, ConnectionClaims, Messaging};
use infrarust_plugin_sdk::types::ChannelId;
use infrarust_plugin_sdk::{debug, info, plugin, warn};

use crate::ammh::AMMH;
use crate::bridge::{self, Bridge, Learned, LoginRequest};
use crate::cache::ServerEntry;
use crate::decide::{DataDecision, RawDecision};

const DATA_DIR: &str = "/";
const PLUGIN_ID: &str = "automodpack";

type Shared = Rc<RefCell<Bridge>>;

#[derive(Default)]
struct AutoModpackBridge;

#[plugin(id = "automodpack", name = "AutoModpack Bridge")]
impl Plugin for AutoModpackBridge {
    fn on_enable(&self, ctx: &Context) -> Result<(), PluginError> {
        let loaded = Bridge::load(Path::new(DATA_DIR))
            .map_err(|error| PluginError::new(format!("config.json: {error}")))?;
        if let Some(error) = &loaded.cache_error {
            warn!("starting with an empty server cache: {error}");
        }
        let cfg = loaded.bridge.config().clone();
        let shared: Shared = Rc::new(RefCell::new(loaded.bridge));

        for channel in bridge::CHANNELS {
            Messaging::register(&ChannelId::modern(channel)).map_err(|error| {
                refused("registering a login channel", "plugin-messaging", error)
            })?;
        }
        subscribe_login_messages(ctx, &shared)?;
        subscribe_logins(ctx, &shared)?;
        if cfg.holepunch {
            subscribe_login_claims(ctx, &shared)?;
        }
        if cfg.magic {
            subscribe_raw_connections(ctx, &shared)?;
            ConnectionClaims::register_prefix(&AMMH).map_err(|error| {
                refused("claiming the AMMH prefix", "connection-handoff", error)
            })?;
        }

        info!(
            "bridge ready: holepunch {}, magic {}, rewrite {:?}, {} declared and {} cached servers{}",
            on_off(cfg.holepunch),
            on_off(cfg.magic),
            cfg.rewrite_to_magic,
            cfg.servers.len(),
            shared.borrow().cache().len(),
            if loaded.config_found {
                ""
            } else {
                ", no config.json (defaults)"
            }
        );
        Ok(())
    }
}

fn subscribe_login_messages(ctx: &Context, shared: &Shared) -> Result<(), PluginError> {
    let state = Rc::clone(shared);
    ctx.on::<LoginPluginRequestEvent>(EventPriority::NORMAL, move |event| {
        let outcome = state.borrow_mut().login_request(
            event.player.id.as_u64(),
            event.server.as_str(),
            &event.channel,
            &event.data,
            now_ms(),
        );
        if let LoginRequest::Data { decision, learned } = &outcome {
            if learned.cache_changed() {
                let bridge = state.borrow();
                report_learned(
                    event.server.as_str(),
                    *learned,
                    bridge.cache().get(event.server.as_str()),
                );
                persist(&bridge);
            }
            if matches!(decision, DataDecision::Replace(_)) && !event.client_reachable {
                debug!(
                    "{} left the modpack data of {} untouched: the client is past its login",
                    event.player.username, event.server
                );
            }
        }
        if let Some(result) = bridge::request_result(&outcome, event.client_reachable) {
            debug!(
                "pointing the modpack data of {} at the proxy for {}",
                event.server, event.player.username
            );
            event.set_result(result);
        }
    })
    .map_err(|error| {
        refused(
            "listening to login plugin requests",
            "plugin-messaging",
            error,
        )
    })?;

    ctx.on::<LoginPluginResponseEvent>(EventPriority::NORMAL, |event| {
        if let Some(answer) = bridge::client_answer(&event.channel, event.successful, &event.data) {
            debug!(
                "{} answered the modpack data of {}: {answer:?}",
                event.player.username, event.server
            );
        }
    })
    .map_err(|error| {
        refused(
            "listening to login plugin responses",
            "plugin-messaging",
            error,
        )
    })?;
    Ok(())
}

fn subscribe_logins(ctx: &Context, shared: &Shared) -> Result<(), PluginError> {
    let state = Rc::clone(shared);
    ctx.on::<ServerConnectedEvent>(EventPriority::NORMAL, move |event| {
        let forgotten = state
            .borrow_mut()
            .server_connected(event.player.id.as_u64(), event.server.as_str());
        if let Some(entry) = forgotten {
            info!(
                "forgetting that {} serves {}: a login finished without any AutoModpack query",
                event.server,
                entry.mode.wire_name()
            );
            persist(&state.borrow());
        }
    })
    .map_err(|error| refused("listening to server connections", "event-bus", error))?;

    let state = Rc::clone(shared);
    ctx.on::<DisconnectEvent>(EventPriority::NORMAL, move |event| {
        state.borrow_mut().disconnected(event.player.id.as_u64());
    })
    .map_err(|error| refused("listening to disconnections", "event-bus", error))?;
    Ok(())
}

fn subscribe_login_claims(ctx: &Context, shared: &Shared) -> Result<(), PluginError> {
    let state = Rc::clone(shared);
    ctx.on::<LoginClaimEvent>(EventPriority::NORMAL, move |event| {
        let server = event.server.as_ref().map(|server| server.as_str());
        let Some(handoff) = state.borrow().login_claim(server, &event.login_start) else {
            return;
        };
        debug!(
            "handing the holepunch login {} from {} to {}",
            event.username,
            event.remote_addr,
            server.unwrap_or_default()
        );
        event.handoff(handoff);
    })
    .map_err(|error| refused("listening to login claims", "connection-handoff", error))?;
    Ok(())
}

fn subscribe_raw_connections(ctx: &Context, shared: &Shared) -> Result<(), PluginError> {
    let state = Rc::clone(shared);
    ctx.on::<RawConnectionEvent>(EventPriority::NORMAL, move |event| {
        let bridge = state.borrow();
        let decision = bridge.raw_connection(&event.prefix, &event.data, resolve_domain);
        match &decision {
            RawDecision::Reject(reason) => {
                debug!(
                    "closing the AMMH connection from {}: {reason:?}",
                    event.remote_addr
                );
            }
            RawDecision::Handoff { server, port, .. } => {
                debug!(
                    "handing the AMMH connection from {} to {server} on {}",
                    event.remote_addr,
                    port.map_or_else(
                        || "its Minecraft port".to_owned(),
                        |port| format!("port {port}")
                    )
                );
            }
            RawDecision::Pass | RawDecision::NeedMore(_) => {}
        }
        if let Some(result) = bridge.raw_result(decision, &event.data) {
            event.set_result(result);
        }
    })
    .map_err(|error| refused("listening to raw connections", "connection-handoff", error))?;
    Ok(())
}

fn resolve_domain(host: &str) -> Option<String> {
    match ProxyConfig::server_by_domain(host) {
        Ok(server) => server.map(|server| server.id.into_string()),
        Err(error) => {
            debug!("looking up the AMMH host {host} failed: {error}");
            None
        }
    }
}

fn report_learned(server: &str, learned: Learned, entry: Option<&ServerEntry>) {
    let Some(entry) = entry else {
        return;
    };
    match learned {
        Learned::New | Learned::Changed => info!(
            "{server} serves AutoModpack {} in {} mode, advertising {}:{}",
            entry.amp_version.as_deref().unwrap_or("(unknown version)"),
            entry.mode.wire_name(),
            if entry.advertised_host.is_empty() {
                "(connection host)"
            } else {
                &entry.advertised_host
            },
            entry.advertised_port
        ),
        Learned::Refreshed | Learned::Nothing => {}
    }
}

fn persist(bridge: &Bridge) {
    if let Err(error) = bridge.save_cache(Path::new(DATA_DIR)) {
        warn!("the server cache was not saved: {error}");
    }
}

fn refused(what: &str, capability: &str, error: Error) -> PluginError {
    if error.kind() == ErrorKind::PermissionDenied {
        PluginError::new(format!(
            "{what}: {error}; add `{capability}` to permissions in [plugins.{PLUGIN_ID}]"
        ))
    } else {
        PluginError::new(format!("{what}: {error}"))
    }
}

fn on_off(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}
