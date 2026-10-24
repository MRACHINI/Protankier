use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use fost_client_utils::{HostProxyProvider, Proxy, ProxyProvider, Session, SocksProxyProvider, Task, DummyResourceLoader, LowLevelPing, SessionPing};
use fost_protocol::{packets::{self, Packet, PacketDowncast}, PacketDebugFilter, SimplePacketDebugFilter};
use tokio::{sync::oneshot, time::timeout};
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
    tx: Option<oneshot::Sender<LoginOutcome>>,
    sent: bool,
}

impl Task for LoginTask {
    type Result = ();

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

        if let (Some(outcome), Some(tx)) = (outcome, self.tx.take()) {
            let _ = tx.send(outcome);
        }
        Ok(())
    }

    fn poll(&mut self, session: &mut Session, _cx: &mut std::task::Context) -> std::task::Poll<anyhow::Result<()>> {
        if !self.sent {
            self.sent = true;
            session.connection.send_packet(&packets::c2s::AccountLoginExecute {
                login: self.username.clone(),
                password: self.password.clone(),
                remember: false,
            })?;
        }
        std::task::Poll::Pending
    }
}

async fn login(session: &mut Session, username: &str, password: &str) -> anyhow::Result<LoginOutcome> {
    let (tx, rx) = oneshot::channel();
    let task = LoginTask { username: username.to_string(), password: password.to_string(), tx: Some(tx), sent: false };
    let _handle = session.execute_task(task);
    let outcome = rx.await.context("login channel closed")?;
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

    let retry = Duration::from_millis(args.retry_ms);
    let session_timeout = Duration::from_secs(args.session_timeout_sec);

    let alive = session
        .execute_task(TaskQueryBattle::new(args.map.clone()))
        .await?;
    if !alive {
        anyhow::bail!("dead link");
    }
    info!("{} - Battle {} is alive", args.username, args.map);

    enter_battle(&mut session, &args.map, retry, &args.username).await?;

    if args.join_on_leave {
        info!("{} - Watching for free slots", args.username);
        loop {
            let left = timeout(session_timeout, session.await_match(|_, packet| {
                packet.downcast_ref::<packets::s2c::BattleInfoRemoveUser>().map(|_| ())
            })).await;
            match left {
                Ok(Ok(())) => {
                    info!("{} - A user left, re-entering battle", args.username);
                    enter_battle(&mut session, &args.map, retry, &args.username).await?;
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

async fn enter_battle(session: &mut Session, link: &str, retry: Duration, username: &str) -> anyhow::Result<()> {
    loop {
        match timeout(retry, session.connection.send_packet(&packets::c2s::ActivateLink { link: link.to_string() })).await {
            Ok(Ok(())) => {
                info!("{} - Sent ActivateLink {}", username, link);
                return Ok(());
            }
            Ok(Err(error)) => return Err(error.into()),
            Err(_) => {}
        }
    }
}

struct TaskQueryBattle {
    battle_id: String,
    sent: bool,
    tx: Option<oneshot::Sender<bool>>,
    rx: Option<oneshot::Receiver<bool>>,
}

impl TaskQueryBattle {
    fn new(battle_id: String) -> Self {
        let (tx, rx) = oneshot::channel();
        Self { battle_id, sent: false, tx: Some(tx), rx: Some(rx) }
    }
}

impl Task for TaskQueryBattle {
    type Result = bool;

    fn handle_packet(&mut self, _session: &mut Session, packet: &dyn Packet) -> anyhow::Result<()> {
        let result = if let Some(p) = packet.downcast_ref::<packets::s2c::LinkResultAlive>() {
            if p.battle_id == self.battle_id { Some(true) } else { None }
        } else if let Some(p) = packet.downcast_ref::<packets::s2c::LinkResultDead>() {
            if p.battle_id == self.battle_id { Some(false) } else { None }
        } else {
            None
        };

        if let (Some(result), Some(tx)) = (result, self.tx.take()) {
            let _ = tx.send(result);
        }
        Ok(())
    }

    fn poll(&mut self, session: &mut Session, cx: &mut std::task::Context) -> std::task::Poll<anyhow::Result<Self::Result>> {
        if !self.sent {
            self.sent = true;
            if let Err(error) = session.connection.send_packet(&packets::c2s::Query { battle_id: self.battle_id.clone() }) {
                return std::task::Poll::Ready(Err(error.into()));
            }
        }
        match self.rx.as_mut() {
            Some(rx) => match std::pin::Pin::new(rx).poll(cx) {
                std::task::Poll::Ready(Ok(result)) => std::task::Poll::Ready(Ok(result)),
                std::task::Poll::Ready(Err(_)) => std::task::Poll::Ready(Err(anyhow::anyhow!("query channel closed"))),
                std::task::Poll::Pending => std::task::Poll::Pending,
            },
            None => std::task::Poll::Ready(Err(anyhow::anyhow!("query task already finished"))),
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let args = Args::parse();
    match args.action.as_str() {
        "click" => run_click(&args).await,
        other => anyhow::bail!("unsupported action: {}", other),
    }
}
