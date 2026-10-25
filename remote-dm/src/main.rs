//! remote-dm: a display manager for remote logins only.
//!
//! Towards gnome-remote-desktop's system daemon it looks like GDM's remote display support, so grd
//! works unmodified. Towards greeters it speaks greetd's IPC protocol, so any greetd greeter can be
//! the login screen. Every remote connection gets its own seatless, remote logind session.
//!
//! The life of a remote connection:
//! 1. grd calls `CreateRemoteDisplay`. We export a display object and start a greeter session for
//!    it, as a free greeter user, with its own greetd socket. Once the session is open, we set the
//!    display's `SessionId`, and the `grd --handover` in the greeter session takes the connection.
//! 2. The greeter logs a user in through the socket. We authenticate with PAM and start the user
//!    session.
//! 3. Once the user session is open, we export a second display object for it (`RemoteId` "/",
//!    `SessionId` of the user session), then change its `RemoteId` to the client's. grd then hands
//!    the connection over from the greeter session to the user session; the RDP client reconnects
//!    once. The greeter's grd calls `Logout`, which ends the greeter session.
//! 4. When the user session ends, its display object goes away.

mod display;
mod greeter_ipc;
mod logind;
mod pam;
mod scrambler;
mod session;
mod session_manager;
mod worker;

use std::collections::HashMap;

use anyhow::Context as _;
use tokio::sync::{mpsc, oneshot};
use tracing_subscriber::EnvFilter;
use zbus::zvariant::OwnedObjectPath;

use crate::display::{Displays, Factory, RemoteDisplayRequest, BUS_NAME, DISPLAYS_PATH, FACTORY_PATH};
use crate::greeter_ipc::{ErrorType, GreeterRequest, Request, Response};
use crate::session::{Session, SessionKey};
use crate::worker::{WorkerEvent, WorkerInput, WorkerRequest};

struct Config {
    /// --session-bus: serve on the session bus, for trying things out without root.
    session_bus: bool,
    /// Greeter sessions run as system users named with this prefix, one per remote display at a
    /// time (`rdm-greeter-1`, `rdm-greeter-2`, ...): concurrent sessions of the same user would
    /// share its session bus and systemd user instance.
    greeter_user_prefix: String,
    /// PAM service for greeter sessions.
    greeter_service: String,
    /// Greeter session command: a compositor running the greeter and `grd --handover`.
    greeter_command: String,
    /// PAM service for user sessions.
    user_service: String,
    /// User session command. The greeter's command is ignored: it's meant for local sessions,
    /// while remote ones have to be headless and run `grd --handover`.
    user_command: String,
}

impl Config {
    fn from_args() -> anyhow::Result<Self> {
        let mut config = Self {
            session_bus: false,
            greeter_user_prefix: "rdm-greeter-".to_owned(),
            greeter_service: "remote-dm-greeter".to_owned(),
            greeter_command: "niri --headless --session -c /etc/remote-dm/greeter.kdl".to_owned(),
            user_service: "remote-dm".to_owned(),
            user_command: "/usr/lib/gnome-remote-desktop-daemon --handover & \
                           exec niri --headless --session"
                .to_owned(),
        };

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            let mut value = || args.next().with_context(|| format!("{arg} needs a value"));
            match arg.as_str() {
                "--session-bus" => config.session_bus = true,
                "--greeter-user-prefix" => config.greeter_user_prefix = value()?,
                "--greeter-service" => config.greeter_service = value()?,
                "--greeter-command" => config.greeter_command = value()?,
                "--user-service" => config.user_service = value()?,
                "--user-command" => config.user_command = value()?,
                _ => anyhow::bail!("unknown argument: {arg}"),
            }
        }
        Ok(config)
    }
}

/// System users available for greeter sessions, by name prefix.
fn greeter_users(prefix: &str) -> Vec<(String, u32)> {
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    let mut users: Vec<_> = passwd
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?;
            let uid = fields.nth(1)?.parse().ok()?;
            name.starts_with(prefix).then(|| (name.to_owned(), uid))
        })
        .collect();
    users.sort();
    users
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Greeter,
    User,
}

#[derive(Debug, PartialEq, Eq)]
enum LoginState {
    Authenticating,
    Authenticated,
    Started,
}

/// A user logging in from a remote greeter.
struct Login {
    username: String,
    session: Session,
    state: LoginState,
    /// The greeter request waiting for what PAM does next.
    reply: Option<oneshot::Sender<Response>>,
}

/// One remote connection.
struct Remote {
    request: RemoteDisplayRequest,
    greeter_user: String,
    greeter_uid: u32,
    greeter: Option<Session>,
    greeter_display: Option<OwnedObjectPath>,
    greeter_socket: Option<greeter_ipc::Listener>,
    login: Option<Login>,
    user_display: Option<OwnedObjectPath>,
}

struct State {
    config: Config,
    conn: zbus::Connection,
    displays: Displays,
    remotes: HashMap<u64, Remote>,
    sessions: HashMap<SessionKey, (u64, Role)>,
    next_key: u64,
    session_events: mpsc::UnboundedSender<session::Event>,
    greeter_requests: mpsc::UnboundedSender<GreeterRequest>,
}

fn main() -> anyhow::Result<()> {
    // The worker must not start a tokio runtime: it forks.
    if std::env::args().nth(1).as_deref() == Some("session-worker") {
        worker::main();
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("remote_dm=info")),
        )
        .init();

    if std::env::args().nth(1).as_deref() == Some("session-manager") {
        return session_manager::main();
    }

    let config = Config::from_args()?;

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run(config))
}

async fn run(config: Config) -> anyhow::Result<()> {
    let (requests_tx, mut requests) = mpsc::unbounded_channel();
    let (session_events_tx, mut session_events) = mpsc::unbounded_channel();
    let (greeter_requests_tx, mut greeter_requests) = mpsc::unbounded_channel();

    let builder = if config.session_bus {
        zbus::connection::Builder::session()?
    } else {
        zbus::connection::Builder::system()?
    };
    let extra_allowed_uid = config.session_bus.then(nix::unistd::getuid).map(|uid| uid.as_raw());
    let conn = builder
        .serve_at(FACTORY_PATH, Factory::new(requests_tx, extra_allowed_uid))?
        .serve_at(DISPLAYS_PATH, zbus::fdo::ObjectManager)?
        .name(BUS_NAME)?
        .build()
        .await
        .with_context(|| format!("error connecting to the bus as {BUS_NAME}"))?;
    tracing::info!("serving {BUS_NAME}");

    let mut state = State {
        config,
        displays: Displays::new(conn.clone()),
        conn,
        remotes: HashMap::new(),
        sessions: HashMap::new(),
        next_key: 1,
        session_events: session_events_tx,
        greeter_requests: greeter_requests_tx,
    };

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            Some(request) = requests.recv() => state.on_remote_display_request(request).await,
            Some((key, event)) = session_events.recv() => state.on_session_event(key, event).await,
            Some(request) = greeter_requests.recv() => state.on_greeter_request(request).await,
            _ = sigterm.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }

    tracing::info!("exiting, ending {} remote sessions", state.remotes.len());
    for remote in state.remotes.values_mut() {
        if let Some(greeter) = &mut remote.greeter {
            greeter.terminate();
        }
        if let Some(login) = &mut remote.login {
            login.session.terminate();
        }
    }
    Ok(())
}

impl State {
    fn next_key(&mut self) -> u64 {
        let key = self.next_key;
        self.next_key += 1;
        key
    }

    async fn on_remote_display_request(&mut self, request: RemoteDisplayRequest) {
        let in_use: Vec<&str> = self
            .remotes
            .values()
            .map(|r| r.greeter_user.as_str())
            .collect();
        let free = greeter_users(&self.config.greeter_user_prefix)
            .into_iter()
            .find(|(name, _)| !in_use.contains(&name.as_str()));
        let Some((greeter_user, greeter_uid)) = free else {
            tracing::warn!(
                "no free greeter user (all {}* in use), refusing connection",
                self.config.greeter_user_prefix
            );
            return;
        };

        let path = match self.displays.add(request.remote_id.clone(), "").await {
            Ok(path) => path,
            Err(err) => {
                tracing::warn!("error adding remote display: {err:?}");
                return;
            }
        };

        let remote_key = self.next_key();
        let socket = if self.config.session_bus {
            None
        } else {
            match greeter_ipc::listen(remote_key, greeter_uid, self.greeter_requests.clone()) {
                Ok(socket) => Some(socket),
                Err(err) => {
                    tracing::warn!("error creating greeter socket: {err:?}");
                    let _ = self.displays.remove(&path).await;
                    return;
                }
            }
        };

        let mut env = vec!["XDG_SESSION_DESKTOP=niri".to_owned()];
        if let Some(socket) = &socket {
            env.push(format!("GREETD_SOCK={}", socket.path().display()));
        }
        let greeter_request = WorkerRequest {
            service: self.config.greeter_service.clone(),
            user: greeter_user.clone(),
            class: "greeter".to_owned(),
            rhost: request.hostname.clone(),
            command: Some(self.config.greeter_command.clone()),
            env,
        };

        let session_key = self.next_key();
        let greeter =
            match Session::spawn(&greeter_request, session_key, self.session_events.clone()).await
            {
                Ok(session) => session,
                Err(err) => {
                    tracing::warn!("error starting greeter session: {err:?}");
                    let _ = self.displays.remove(&path).await;
                    return;
                }
            };
        self.sessions.insert(session_key, (remote_key, Role::Greeter));

        tracing::info!(
            remote = remote_key,
            %path,
            remote_id = %request.remote_id,
            greeter_user,
            "new remote display, starting greeter"
        );
        self.remotes.insert(
            remote_key,
            Remote {
                request,
                greeter_user,
                greeter_uid,
                greeter: Some(greeter),
                greeter_display: Some(path),
                greeter_socket: socket,
                login: None,
                user_display: None,
            },
        );
    }

    async fn on_session_event(&mut self, key: SessionKey, event: WorkerEvent) {
        let Some(&(remote_key, role)) = self.sessions.get(&key) else {
            return;
        };
        if matches!(event, WorkerEvent::Exited { .. } | WorkerEvent::Failed { .. }) {
            self.sessions.remove(&key);
        }
        match role {
            Role::Greeter => self.on_greeter_session_event(remote_key, event).await,
            Role::User => self.on_user_session_event(remote_key, event).await,
        }
    }

    async fn on_greeter_session_event(&mut self, remote_key: u64, event: WorkerEvent) {
        let Some(remote) = self.remotes.get_mut(&remote_key) else {
            return;
        };

        match event {
            WorkerEvent::Opened { session_id } => {
                tracing::info!(remote = remote_key, session_id, "greeter session opened");
                if let Some(path) = &remote.greeter_display {
                    if let Err(err) = self.displays.set_session_id(path, &session_id).await {
                        tracing::warn!("error setting session id: {err:?}");
                    }
                }
            }
            WorkerEvent::Exited { code } => {
                tracing::info!(remote = remote_key, code, "greeter session ended");
                self.end_greeter(remote_key).await;
            }
            WorkerEvent::Failed { error, .. } => {
                tracing::warn!(remote = remote_key, "greeter session failed: {error}");
                self.end_greeter(remote_key).await;
            }
            // Greeter sessions don't authenticate.
            WorkerEvent::Prompt { .. } | WorkerEvent::Authenticated => (),
        }
    }

    async fn on_user_session_event(&mut self, remote_key: u64, event: WorkerEvent) {
        let Some(remote) = self.remotes.get_mut(&remote_key) else {
            return;
        };
        let Some(login) = &mut remote.login else {
            return;
        };

        match event {
            WorkerEvent::Prompt { kind, message } => {
                let response = Response::AuthMessage {
                    auth_message_type: kind,
                    auth_message: message,
                };
                match login.reply.take() {
                    Some(reply) => {
                        let _ = reply.send(response);
                    }
                    None => tracing::warn!(remote = remote_key, "PAM prompt without a greeter request"),
                }
            }
            WorkerEvent::Authenticated => {
                tracing::info!(remote = remote_key, "user authenticated");
                login.state = LoginState::Authenticated;
                if let Some(reply) = login.reply.take() {
                    let _ = reply.send(Response::Success);
                }
            }
            WorkerEvent::Failed { error, auth } => {
                tracing::info!(remote = remote_key, auth, "login failed: {error}");
                if let Some(reply) = login.reply.take() {
                    let _ = reply.send(Response::Error {
                        error_type: if auth { ErrorType::AuthError } else { ErrorType::Error },
                        description: error,
                    });
                }
                let started = login.state == LoginState::Started;
                remote.login = None;
                if started {
                    // The user session never came up; nothing will take the connection.
                    self.end_remote(remote_key).await;
                }
            }
            WorkerEvent::Opened { session_id } => {
                tracing::info!(remote = remote_key, session_id, "user session opened, handing over");
                self.hand_over(remote_key, &session_id).await;
            }
            WorkerEvent::Exited { code } => {
                tracing::info!(remote = remote_key, code, "user session ended");
                remote.login = None;
                self.end_remote(remote_key).await;
            }
        }
    }

    /// Moves the connection from the greeter session to the user session (see the module docs).
    async fn hand_over(&mut self, remote_key: u64, session_id: &str) {
        let Some(remote) = self.remotes.get(&remote_key) else {
            return;
        };
        let remote_id = remote.request.remote_id.clone();

        // grd registers a handover for the new session first (under a temporary client, since
        // the remote ID is "/"); the remote ID change then moves the client over to it.
        let placeholder = OwnedObjectPath::try_from("/").unwrap();
        let path = match self.displays.add(placeholder, session_id).await {
            Ok(path) => path,
            Err(err) => {
                tracing::warn!("error adding user display: {err:?}");
                return;
            }
        };
        if let Err(err) = self.displays.set_remote_id(&path, remote_id).await {
            tracing::warn!("error setting user display remote id: {err:?}");
        }

        if let Some(remote) = self.remotes.get_mut(&remote_key) {
            remote.user_display = Some(path);
        }
    }

    async fn on_greeter_request(&mut self, request: GreeterRequest) {
        let GreeterRequest {
            remote: remote_key,
            request,
            reply,
        } = request;
        let Some(remote) = self.remotes.get_mut(&remote_key) else {
            let _ = reply.send(Response::error("no such remote display"));
            return;
        };

        match request {
            Request::CreateSession { username } => {
                if remote.login.is_some() {
                    let _ = reply.send(Response::error("a session is already being set up"));
                    return;
                }

                let worker_request = WorkerRequest {
                    service: self.config.user_service.clone(),
                    user: username.clone(),
                    class: "user".to_owned(),
                    rhost: remote.request.hostname.clone(),
                    command: None,
                    env: vec!["XDG_SESSION_DESKTOP=niri".to_owned()],
                };
                let key = self.next_key();
                match Session::spawn(&worker_request, key, self.session_events.clone()).await {
                    Ok(session) => {
                        tracing::info!(remote = remote_key, username, "login started");
                        let username = username.clone();
                        self.sessions.insert(key, (remote_key, Role::User));
                        if let Some(remote) = self.remotes.get_mut(&remote_key) {
                            remote.login = Some(Login {
                                username,
                                session,
                                state: LoginState::Authenticating,
                                reply: Some(reply),
                            });
                        }
                    }
                    Err(err) => {
                        let _ = reply.send(Response::error(format!("{err:#}")));
                    }
                }
            }
            Request::PostAuthMessageResponse { response } => {
                let Some(login) = &mut remote.login else {
                    let _ = reply.send(Response::error("no session is being set up"));
                    return;
                };
                if login.state != LoginState::Authenticating {
                    let _ = reply.send(Response::error("not expecting a response"));
                    return;
                }
                match login.session.send(&WorkerInput::Answer { response }).await {
                    Ok(()) => login.reply = Some(reply),
                    Err(err) => {
                        let _ = reply.send(Response::error(format!("{err:#}")));
                    }
                }
            }
            Request::StartSession { cmd, env } => {
                let Some(login) = &mut remote.login else {
                    let _ = reply.send(Response::error("no session is being set up"));
                    return;
                };
                if login.state != LoginState::Authenticated {
                    let _ = reply.send(Response::error("not authenticated"));
                    return;
                }

                // Back to the session the user already has, if any (like GDM does).
                let username = login.username.clone();
                if let Some(existing) = self.find_user_session(&username, remote_key) {
                    tracing::info!(remote = remote_key, existing, username, "reconnecting to the existing session");
                    let remote = self.remotes.get_mut(&remote_key).unwrap();
                    if let Some(mut login) = remote.login.take() {
                        let _ = login.session.send(&WorkerInput::Cancel).await;
                    }
                    let _ = reply.send(Response::Success);
                    self.reconnect(remote_key, existing).await;
                    return;
                }

                let remote = self.remotes.get_mut(&remote_key).unwrap();
                let login = remote.login.as_mut().unwrap();
                tracing::debug!(remote = remote_key, ?cmd, "ignoring the greeter's session command");
                let input = WorkerInput::Start {
                    command: self.config.user_command.clone(),
                    env,
                };
                match login.session.send(&input).await {
                    Ok(()) => {
                        login.state = LoginState::Started;
                        let _ = reply.send(Response::Success);
                    }
                    Err(err) => {
                        let _ = reply.send(Response::error(format!("{err:#}")));
                    }
                }
            }
            Request::CancelSession => {
                let started = remote
                    .login
                    .as_ref()
                    .is_some_and(|login| login.state == LoginState::Started);
                if started {
                    let _ = reply.send(Response::error("the session already started"));
                    return;
                }
                if let Some(mut login) = remote.login.take() {
                    let _ = login.session.send(&WorkerInput::Cancel).await;
                }
                let _ = reply.send(Response::Success);
            }
        }
    }

    /// Another remote display whose user session for `username` is up.
    fn find_user_session(&self, username: &str, except: u64) -> Option<u64> {
        self.remotes.iter().find_map(|(&key, remote)| {
            let login = remote.login.as_ref()?;
            (key != except
                && login.username == username
                && login.state == LoginState::Started
                && remote.user_display.is_some())
            .then_some(key)
        })
    }

    /// Moves the connection of `remote_key` (now in its greeter session) to the user session of
    /// `existing`: pointing the existing session's display at the new client makes grd hand the
    /// client over to that session's `grd --handover`.
    async fn reconnect(&mut self, remote_key: u64, existing: u64) {
        let Some(remote_id) = self
            .remotes
            .get(&remote_key)
            .map(|remote| remote.request.remote_id.clone())
        else {
            return;
        };
        let Some(existing_remote) = self.remotes.get_mut(&existing) else {
            return;
        };
        let Some(path) = existing_remote.user_display.clone() else {
            return;
        };

        existing_remote.request.remote_id = remote_id.clone();
        if let Err(err) = self.displays.set_remote_id(&path, remote_id).await {
            tracing::warn!("error pointing the existing session at the new client: {err:?}");
        }
        // The new connection's greeter ends on its own once grd moved the client (Logout).
    }

    /// The greeter session is gone: forget its display and socket, free its user.
    async fn end_greeter(&mut self, remote_key: u64) {
        let Some(remote) = self.remotes.get_mut(&remote_key) else {
            return;
        };

        remote.greeter = None;
        remote.greeter_socket = None;
        if let Some(path) = remote.greeter_display.take() {
            let _ = self.displays.remove(&path).await;
        }
        let uid = remote.greeter_uid;
        let has_user_session = remote
            .login
            .as_ref()
            .is_some_and(|login| login.state == LoginState::Started);

        // Kill whatever the greeter user still has running, so the next greeter starts clean.
        if !self.config.session_bus {
            logind::terminate_user(&self.conn, uid).await;
        }

        if !has_user_session {
            self.end_remote(remote_key).await;
        }
    }

    /// The remote connection is over.
    async fn end_remote(&mut self, remote_key: u64) {
        let Some(mut remote) = self.remotes.remove(&remote_key) else {
            return;
        };
        tracing::info!(remote = remote_key, "remote display done");

        if let Some(greeter) = &mut remote.greeter {
            greeter.terminate();
        }
        if let Some(path) = remote.greeter_display.take() {
            let _ = self.displays.remove(&path).await;
        }
        if let Some(path) = remote.user_display.take() {
            let _ = self.displays.remove(&path).await;
        }
        if let Some(mut login) = remote.login.take() {
            login.session.terminate();
        }
        self.sessions.retain(|_, (key, _)| *key != remote_key);
    }
}
