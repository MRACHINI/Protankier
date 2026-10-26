use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use fost_client_utils::{HostProxyProvider, Proxy, ProxyProvider, Session, SocksProxyProvider, Task, DummyResourceLoader, LowLevelPing, SessionPing};
use fost_protocol::{packets::{self, Packet, PacketDowncast}, codec::{BattleTeam, LayoutState}, PacketDebugFilter, SimplePacketDebugFilter};
use tokio::time::timeout;
use tracing::info;
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

fn build_proxy(proxy: &str) -> anyhow::Result<Box<dyn ProxyProvider>> {
    if proxy.trim().is_empty() {
        Ok(Box::new(HostProxyProvider::new()))
    } else {
        Ok(Box::new(SocksProxyProvider::from_link(proxy)?))
    }
}

async fn connect(args: &Args) -> anyhow::Result<Session> {
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
    Ok(session)
}

async fn run_click(args: &Args) -> anyhow::Result<()> {
    let mut session = connect(args).await?;
    info!("{} - Connected to {}", args.username, args.target);

    session.await_server_resources_loaded().await?;
    info!("{} - Viewing login screen", args.username);

    match login(&mut session, &args.username, &args.password).await? {
        LoginOutcome::Success => info!("{} - Logged in", args.username),
        LoginOutcome::Failure => anyhow::bail!("login failed: wrong credentials"),
        LoginOutcome::Banned => anyhow::bail!("login failed: account banned"),
    }

    let account_rank = session
        .await_match(|_, packet| {
            packet.downcast_ref::<packets::s2c::AccountInfoProperties>().map(|p| p.user_property_cc.rank as u32)
        })
        .await?;
    info!("Account Rank: {}", account_rank);

    /* The live client asks the server to prepare the lobby before any battle list use.
       Packet 1452181070 is juhe1's `LoadLobbyOutPacket` / our `LayoutSwitchServerSelect`. */
    session.connection.send_packet(&packets::c2s::LayoutSwitchServerSelect {})?;
    info!("{} - Sent ServerSelect / LoadLobby", args.username);

    let session_timeout = Duration::from_secs(args.session_timeout_sec);

    /* Battle list selection is the live server's actual join request.
       Alpha selects Red, Bravo selects Blue, J/any selects None. */
    let team = match args.side.as_str() {
        "Alpha" => BattleTeam::Red,
        "Bravo" => BattleTeam::Blue,
        _ => BattleTeam::None,
    };

    info!("{} - Joining map {}", args.username, args.map);
    enter_battle(&mut session, &args.map, team, session_timeout, &args.username).await?;

    if args.join_on_leave {
        info!("{} - Watching for free slots", args.username);
        loop {
            let left = timeout(session_timeout, session.await_match(|_, packet| {
                packet.downcast_ref::<packets::s2c::BattleInfoRemoveUser>().map(|_| ())
            })).await;
            match left {
                Ok(Ok(())) => {
                    info!("{} - A user left, re-entering battle", args.username);
                    enter_battle(&mut session, &args.map, team, session_timeout, &args.username).await?;
                }
                Ok(Err(error)) => return Err(error),
                Err(_) => {
                    info!("{} - No slot event for {}s, still watching", args.username, args.session_timeout_sec);
                }
            }
        }
    }

    session.await;
    info!("{} - Client disconnected", args.username);
    Ok(())
}

async fn enter_battle(
    session: &mut Session,
    battle_id: &str,
    team: BattleTeam,
    timeout_duration: Duration,
    username: &str,
) -> anyhow::Result<()> {
    let battle_id = battle_id.to_string();

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

    /* Try joining directly; the select step is what the live server was rejecting. */
    info!("{} - Sending direct join for {}", username, battle_id);

    /* Request the join and wait for the battle lobby state transition. */
    session.connection.send_packet(&packets::s2c::BattleInfoJoinBattle {
        var_625: team,
    })?;
    info!("{} - Sent BattleInfoJoinBattle", username);

    let result = timeout(timeout_duration, session.await_match(|_, packet| {
        packet
            .downcast_ref::<packets::s2c::LobbyLayoutSwitchStart>()
            .filter(|packet| packet.state == LayoutState::Battle)
            .map(|_| ())
    }))
    .await;

    match result {
        Ok(Ok(())) => info!("{} - Joined map", username),
        Ok(Err(error)) => return Err(error),
        Err(_) => anyhow::bail!("battle join timed out"),
    }

    Ok(())
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
