use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use fost_client_utils::{HostProxyProvider, Proxy, ProxyProvider, Session, SocksProxyProvider, Task, DummyResourceLoader, LowLevelPing, SessionPing};
use fost_protocol::{packets::{self, Packet, PacketDowncast}, codec::{BattleTeam, LayoutState}, PacketDebugFilter, SimplePacketDebugFilter};
use tokio::time::timeout;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug, Clone)]
#[command(name = "server-click")]
struct Args {
    /// Action: click | unglitch | watch
    #[arg(long, default_value = "click")]
    action: String,

    #[arg(long)]
    username: String,

    #[arg(long)]
    password: String,

    /// Target battle id (the map link)
    #[arg(long, default_value = "")]
    map: String,

    /// Team side: Alpha | Bravo | J (any / DM)
    #[arg(long, default_value = "J")]
    side: String,

    /// Game server address, host:port
    #[arg(long, default_value = "146.59.110.195:1337")]
    target: String,

    /// SOCKS5 proxy URL or empty for direct
    #[arg(long, default_value = "")]
    proxy: String,

    #[arg(long, default_value = "en")]
    language_code: String,

    /// Print every decoded packet
    #[arg(long)]
    log_protocol: bool,

    /// Keep the session and re-join when the battle has a free slot
    #[arg(long)]
    join_on_leave: bool,

    /// Milliseconds between join attempts
    #[arg(long, default_value_t = 500)]
    retry_ms: u64,

    /// Seconds to wait for a session event before giving up
    #[arg(long, default_value_t = 60)]
    session_timeout_sec: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginOutcome {
    Success,
    Failure,
    Banned,
}

/// Session failures where reconnecting would only loop forever.
#[derive(Debug)]
enum SessionError {
    WrongCredentials,
    Banned,
    BattleNotListed { battle_id: String, online: usize },
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::WrongCredentials => write!(f, "login failed: wrong credentials"),
            SessionError::Banned => write!(f, "login failed: account banned"),
            SessionError::BattleNotListed { battle_id, online } => write!(
                f,
                "battle {} is not in the server list ({} battles online)",
                battle_id, online
            ),
        }
    }
}

impl std::error::Error for SessionError {}

struct LoginTask {
    username: String,
    password: String,
    outcome: Option<LoginOutcome>,
    sent: bool,
}

impl Task for LoginTask {
    type Result = LoginOutcome;

    fn handle_packet(&mut self, _session: &mut Session, packet: &dyn Packet) -> anyhow::Result<()> {
        let outcome = if packet.is_type::<packets::s2c::AccountLoginFailure>() {
            Some(LoginOutcome::Failure)
        } else if packet.is_type::<packets::s2c::AccountLoginSuccess>() {
            Some(LoginOutcome::Success)
        } else if packet.downcast_ref::<packets::s2c::BanPermanent>().is_some()
            || packet.downcast_ref::<packets::s2c::BanTemporary>().is_some()
        {
            Some(LoginOutcome::Banned)
        } else {
            None
        };

        if let Some(outcome) = outcome {
            self.outcome = Some(outcome);
        }
        Ok(())
    }

    fn poll(&mut self, session: &mut Session, _cx: &mut std::task::Context) -> std::task::Poll<anyhow::Result<LoginOutcome>> {
        if !self.sent {
            self.sent = true;
            if let Err(error) = session.connection.send_packet(&packets::c2s::AccountLoginExecute {
                login: self.username.clone(),
                password: self.password.clone(),
                remember: false,
            }) {
                return std::task::Poll::Ready(Err(error.into()));
            }
        }

        if let Some(outcome) = self.outcome.take() {
            return std::task::Poll::Ready(Ok(outcome));
        }
        std::task::Poll::Pending
    }
}

async fn login(session: &mut Session, username: &str, password: &str) -> anyhow::Result<LoginOutcome> {
    let task = LoginTask { username: username.to_string(), password: password.to_string(), outcome: None, sent: false };
    let outcome = session.execute_task(task).await.context("login channel closed")?;
    Ok(outcome)
}

/// Remembers the newest battle list the server pushed, even if it arrived
/// before we started waiting for it.
#[derive(Clone, Default)]
struct BattleListCache(Arc<Mutex<Option<String>>>);

impl BattleListCache {
    fn store(&self, json: String) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(json);
        }
    }

    fn take(&self) -> Option<String> {
        self.0.lock().ok().and_then(|mut slot| slot.take())
    }
}

impl fost_client_utils::PacketHandler for BattleListCache {
    fn handle_packet(&mut self, _client: &mut Session, packet: &dyn Packet) -> anyhow::Result<()> {
        if let Some(packet) = packet.downcast_ref::<packets::s2c::BattleListListCreate>() {
            self.store(packet.json.clone());
        }
        Ok(())
    }
}

/// Reports incoming packets that no handler recognised, so we can see
/// what the live server sends that our packet table does not cover.
struct UnknownPacketLog;

impl fost_client_utils::PacketHandler for UnknownPacketLog {
    fn handle_packet(&mut self, _client: &mut Session, packet: &dyn Packet) -> anyhow::Result<()> {
        if let Some(packet) = packet.downcast_ref::<packets::UnknownPacket>() {
            let payload = packet.payload();
            let head: String = payload
                .iter()
                .take(48)
                .map(|byte| format!("{:02x}", byte))
                .collect::<Vec<_>>()
                .join(" ");
            info!(
                "[UNKNOWN-IN] id={} len={} head=[{}]",
                packet.packet_id() as i32,
                payload.len(),
                head
            );
        }
        Ok(())
    }
}

fn build_proxy(proxy: &str) -> anyhow::Result<Box<dyn ProxyProvider>> {
    if proxy.trim().is_empty() {
        Ok(Box::new(HostProxyProvider::new()))
    } else {
        Ok(Box::new(SocksProxyProvider::from_link(proxy)?))
    }
}

async fn connect(args: &Args) -> anyhow::Result<(Session, BattleListCache)> {
    let provider = build_proxy(&args.proxy)?;
    let mut proxy: Box<dyn Proxy> = provider.next_proxy().context("no proxy available")?;
    let target: std::net::SocketAddr = args.target.parse().context("invalid target address")?;
    let stream = proxy.create_stream(target).await?;

    let filter: Box<dyn PacketDebugFilter> = if args.log_protocol {
        Box::new(SimplePacketDebugFilter::logging_enabled())
    } else {
        Box::new(SimplePacketDebugFilter::logging_disabled())
    };

    let mut session = Session::builder()
        .set_lang_code(args.language_code.clone())
        .set_log_filter(filter)
        .allow_unknown_packets()
        .connect_with_socket(target, stream)
        .await?;

    session.register_packet_handler(DummyResourceLoader {});
    session.register_packet_handler(LowLevelPing {});
    session.register_packet_handler(SessionPing {});
    let battle_list = BattleListCache::default();
    session.register_packet_handler(battle_list.clone());
    session.register_packet_handler(UnknownPacketLog);
    Ok((session, battle_list))
}

async fn run_click(args: &Args) -> anyhow::Result<()> {
    let mut attempt = 0u32;
    loop {
        match run_session(args).await {
            Ok(()) => return Ok(()),
            Err(error) if is_permanent(&error) => return Err(error),
            Err(error) if !args.join_on_leave => return Err(error),
            Err(error) => {
                /* The server drops battle sessions periodically; reconnect instead of dying. */
                attempt += 1;
                let delay = Duration::from_secs(3).min(Duration::from_millis(500) * attempt);
                warn!(
                    "{} - Session ended ({}); reconnecting in {}s",
                    args.username,
                    error,
                    delay.as_secs()
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// Errors that reconnect attempts cannot fix.
fn is_permanent(error: &anyhow::Error) -> bool {
    match error.downcast_ref::<SessionError>() {
        Some(SessionError::WrongCredentials) | Some(SessionError::Banned) => true,
        Some(SessionError::BattleNotListed { .. }) => true,
        _ => false,
    }
}

/// Runs one full session: connect, log in, join, then keep watching.
/// A dropped connection just means we start over, which is what a clicker must do.
async fn run_session(args: &Args) -> anyhow::Result<()> {
    let (mut session, battle_list) = connect(args).await?;
    info!("{} - Connected to {}", args.username, args.target);

    session.await_server_resources_loaded().await?;
    info!("{} - Viewing login screen", args.username);

    match login(&mut session, &args.username, &args.password).await? {
        LoginOutcome::Success => info!("{} - Logged in", args.username),
        LoginOutcome::Failure => return Err(SessionError::WrongCredentials.into()),
        LoginOutcome::Banned => return Err(SessionError::Banned.into()),
    }

    let account_rank = session
        .await_match(|_, packet| {
            packet.downcast_ref::<packets::s2c::AccountInfoProperties>().map(|p| p.user_property_cc.rank as u32)
        })
        .await?;
    info!("Account Rank: {}", account_rank);

    let session_timeout = Duration::from_secs(args.session_timeout_sec);
    let team = team_for_side(&args.side);

    enter_battle(
        &mut session,
        &battle_list,
        &args.map,
        team,
        session_timeout,
        args.join_on_leave,
        &args.username,
    )
    .await?;

    if args.join_on_leave {
        info!("{} - Watching battle", args.username);
        loop {
            let left = timeout(session_timeout, session.await_match(|_, packet| {
                packet
                    .downcast_ref::<packets::s2c::LobbyLayoutSwitchStart>()
                    .filter(|packet| packet.state != LayoutState::Battle)
                    .map(|_| ())
            }))
            .await;

            match left {
                Ok(Ok(())) => {
                    info!("{} - Left the battle, sniping again", args.username);
                    enter_battle(
                        &mut session,
                        &battle_list,
                        &args.map,
                        team,
                        session_timeout,
                        true,
                        &args.username,
                    )
                    .await?;
                }
                Ok(Err(error)) => return Err(error),
                Err(_) => {
                    info!("{} - Still in battle after {}s", args.username, args.session_timeout_sec);
                }
            }
        }
    }

    session.await;
    info!("{} - Client disconnected", args.username);
    Ok(())
}

fn team_for_side(side: &str) -> BattleTeam {
    match side {
        "Alpha" => BattleTeam::Red,
        "Bravo" => BattleTeam::Blue,
        _ => BattleTeam::None,
    }
}

async fn enter_battle(
    session: &mut Session,
    battle_list: &BattleListCache,
    battle_id: &str,
    team: BattleTeam,
    timeout_duration: Duration,
    keep_trying: bool,
    username: &str,
) -> anyhow::Result<()> {
    let battle_id = battle_id.to_string();

    info!("{} - Joining map {}", username, battle_id);

    /* Wait for the login/lobby transition to settle before touching the battle list. */
    let result = timeout(timeout_duration, session.await_match(|_, packet| {
        packet
            .downcast_ref::<packets::s2c::LobbyLayoutSwitchEnd>()
            .map(|_| ())
    }))
    .await;

    match result {
        Ok(Ok(())) => info!("{} - Lobby ready", username),
        Ok(Err(error)) => return Err(error),
        Err(_) => anyhow::bail!("lobby ready timed out"),
    }

    /* This is what the real client does the moment the lobby finishes switching:
       PanelModel.changeState -> showBattleLobby() -> LoadLobbyOutPacket (1452181070).
       Sending it before LobbyLayoutSwitchEnd made the live server drop us; sending it
       afterwards is what makes the server enter the battle-select lobby and push the list. */
    session.connection.send_packet(&packets::c2s::LayoutSwitchServerSelect {})?;
    info!("{} - Sent LoadLobby (battle select)", username);

    /* The real client waits for the server's battle list before selecting.
       Selecting before this packet arrives makes the live server drop the connection.
       The list may already have arrived during login, so check the cache first. */
    let mut list_json = battle_list.take();
    if list_json.is_some() {
        info!("{} - Battle list already received during login", username);
    } else {
        let list_result = timeout(timeout_duration, session.await_match(|_, packet| {
            packet
                .downcast_ref::<packets::s2c::BattleListListCreate>()
                .map(|packet| packet.json.clone())
        }))
        .await;

        match list_result {
            Ok(Ok(json)) => list_json = Some(json),
            Ok(Err(error)) => return Err(error),
            Err(_) => info!("{} - Battle list not received; continuing", username),
        }
    }

    let battle_id = battle_id;
    if let Some(json) = list_json {
        let ids = battle_ids(&json);
        info!(
            "{} - Battle list received ({} battles)",
            username,
            ids.len()
        );

        /* Snipe only the requested battle. Quietly joining a different one would make the
           clicker look successful while sitting in the wrong match. */
        if !ids.iter().any(|id| *id == battle_id) {
            return Err(SessionError::BattleNotListed {
                battle_id: battle_id.clone(),
                online: ids.len(),
            }
            .into());
        }
    }

    /* The real client selects a battle once and joins; if the match is full the join simply
       does not produce the Battle layout switch. That is the normal state for a slot sniper:
       with join-on-leave enabled keep trying until a slot frees up, otherwise fail once.
       A disconnect is fatal here - retrying it would spin at full speed forever. */
    let attempt_timeout = Duration::from_secs(4);
    let retry_pause = Duration::from_millis(600);
    loop {
        match try_join(session, &battle_id, team, attempt_timeout, username).await {
            Ok(JoinAttempt::Joined) => return Ok(()),
            Ok(JoinAttempt::Full) if keep_trying => {
                warn!("{} - Match is full; retrying", username);
                tokio::time::sleep(retry_pause).await;
            }
            Ok(JoinAttempt::Full) => anyhow::bail!("match is full"),
            Err(error) => return Err(error),
        }
    }
}

fn battle_ids(json: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|value| value.get("battles").and_then(|b| b.as_array()).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|battle| {
            battle
                .get("battleId")
                .and_then(|id| id.as_str())
                .map(str::to_string)
        })
        .collect()
}

/// One select + join attempt. An error means "no slot this time", not a fatal failure.
/// Outcome of one select + join attempt.
enum JoinAttempt {
    /// We are in the battle.
    Joined,
    /// The match is full right now; worth trying again.
    Full,
}

async fn try_join(
    session: &mut Session,
    battle_id: &str,
    team: BattleTeam,
    timeout_duration: Duration,
    username: &str,
) -> anyhow::Result<JoinAttempt> {
    session.connection.send_packet(&packets::c2s::BattleListBattleSelect {
        item: battle_id.to_string(),
    })?;

    let select_id = battle_id.to_string();
    match timeout(timeout_duration, session.await_match(move |_, packet| {
        packet
            .downcast_ref::<packets::s2c::BattleListBattleSelect>()
            .filter(|packet| packet.item == select_id)
            .map(|_| ())
    }))
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error),
        Err(_) => return Ok(JoinAttempt::Full),
    }

    session.connection.send_packet(&packets::s2c::BattleInfoJoinBattle {
        var_625: team,
    })?;

    match timeout(timeout_duration, session.await_match(|_, packet| {
        packet
            .downcast_ref::<packets::s2c::LobbyLayoutSwitchStart>()
            .filter(|packet| packet.state == LayoutState::Battle)
            .map(|_| ())
    }))
    .await
    {
        Ok(Ok(())) => {
            info!("{} - Joined map {}", username, battle_id);
            Ok(JoinAttempt::Joined)
        }
        Ok(Err(error)) => Err(error),
        Err(_) => Ok(JoinAttempt::Full),
    }
}


#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let args = Args::parse();
    match args.action.as_str() {
        "click" => run_click(&args).await,
        other => anyhow::bail!("unsupported action: {}", other),
    }
}
