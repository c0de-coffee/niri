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
//!
//! A client that never reaches its greeter (it doesn't follow grd's redirect) is dropped by grd
//! after 30 seconds; we then end the greeter session (see `handovers`).
//!
//! If grd already authenticated the user (Kerberos single sign-on, `preauthenticated-user`), there
//! is no greeter: the display object gets the user session directly, or the user's existing session
//! is pointed at the new client.

mod config;
mod display;
mod greeter_ipc;
mod handovers;
mod logind;
// Copied from greetd as is, including a few methods we don't use.
#[allow(dead_code)]
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

struct Args {
    /// --session-bus: serve on the session bus, for trying things out without root.
    session_bus: bool,
    config: config::Config,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut session_bus = false;
        let mut config_path = None;

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--session-bus" => session_bus = true,
                "--config" => {
                    config_path = Some(args.next().context("--config needs a path")?);
                }
                "--help" | "-h" => {
                    println!(
                        "usage: remote-dm [--config PATH] [--session-bus]\n\
                         \n\
                         Display manager for remote logins with gnome-remote-desktop.\n\
                         Config: {} (optional).",
                        config::DEFAULT_PATH
                    );
                    std::process::exit(0);
                }
                _ => anyhow::bail!("unknown argument: {arg}"),
            }
        }

        let explicit = config_path.is_some();
        let path = config_path.unwrap_or_else(|| config::DEFAULT_PATH.to_owned());
        let config = config::Config::load(std::path::Path::new(&path), explicit)?;
        Ok(Self {
            session_bus,
            config,
        })
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

/// A user logging in from a remote greeter, or preauthenticated by grd.
struct Login {
    username: String,
    session: Session,
    state: LoginState,
    /// The logind session, once it's open.
    session_id: Option<String>,
    /// The greeter request waiting for what PAM does next.
    reply: Option<oneshot::Sender<Response>>,
}

struct GreeterUser {
    name: String,
    uid: u32,
}

/// One remote connection.
struct Remote {
    request: RemoteDisplayRequest,
    /// `None` for preauthenticated users, who skip the greeter.
    greeter_user: Option<GreeterUser>,
    greeter: Option<Session>,
    greeter_display: Option<OwnedObjectPath>,
    /// The greeter's logind session, once it's open.
    greeter_session_id: Option<String>,
    greeter_socket: Option<greeter_ipc::Listener>,
    login: Option<Login>,
    user_display: Option<OwnedObjectPath>,
    /// The client is on its way from the greeter to a user session; the greeter ends on its own.
    handed_over: bool,
}

struct State {
    session_bus: bool,
    config: config::Config,
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

    let args = Args::parse()?;

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run(args))
}

async fn run(args: Args) -> anyhow::Result<()> {
    let Args {
        session_bus,
        config,
    } = args;

    let (requests_tx, mut requests) = mpsc::unbounded_channel();
    let (session_events_tx, mut session_events) = mpsc::unbounded_channel();
    let (greeter_requests_tx, mut greeter_requests) = mpsc::unbounded_channel();
    let (handovers_removed_tx, mut handovers_removed) = mpsc::unbounded_channel();

    let builder = if session_bus {
        zbus::connection::Builder::session()?
    } else {
        zbus::connection::Builder::system()?
    };
    let extra_allowed_uid = session_bus.then(nix::unistd::getuid).map(|uid| uid.as_raw());
    let conn = builder
        .serve_at(FACTORY_PATH, Factory::new(requests_tx, extra_allowed_uid))?
        .serve_at(DISPLAYS_PATH, zbus::fdo::ObjectManager)?
        .name(BUS_NAME)?
        .build()
        .await
        .with_context(|| format!("error connecting to the bus as {BUS_NAME}"))?;
    tracing::info!("serving {BUS_NAME}");

    if let Err(err) = handovers::watch(&conn, handovers_removed_tx).await {
        tracing::warn!("error watching grd's handovers, abandoned greeters won't end: {err:?}");
    }

    let mut state = State {
        session_bus,
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
            Some(session_id) = handovers_removed.recv() => state.on_handover_removed(&session_id),
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
        if request.preauthenticated_user.is_some()
            && !self.config.session.preauthenticated_pam_service.is_empty()
        {
            self.start_preauthenticated(request).await;
            return;
        }

        let in_use: Vec<&str> = self
            .remotes
            .values()
            .filter_map(|r| Some(r.greeter_user.as_ref()?.name.as_str()))
            .collect();
        let free = greeter_users(&self.config.greeter.user_prefix)
            .into_iter()
            .find(|(name, _)| !in_use.contains(&name.as_str()));
        let Some((greeter_user, greeter_uid)) = free else {
            tracing::warn!(
                "no free greeter user (all {}* in use), refusing connection",
                self.config.greeter.user_prefix
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
        let socket = if self.session_bus {
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
            service: self.config.greeter.pam_service.clone(),
            user: greeter_user.clone(),
            class: "greeter".to_owned(),
            rhost: request.hostname.clone(),
            command: Some(self.config.greeter.command.clone()),
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
                greeter_user: Some(GreeterUser {
                    name: greeter_user,
                    uid: greeter_uid,
                }),
                greeter: Some(greeter),
                greeter_display: Some(path),
                greeter_session_id: None,
                greeter_socket: socket,
                login: None,
                user_display: None,
                handed_over: false,
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
                remote.greeter_session_id = Some(session_id.clone());
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
                login.session_id = Some(session_id.clone());
                if let Some(path) = &remote.user_display {
                    // Preauthenticated: the client is still at grd's system daemon, waiting for
                    // this display to get a session.
                    tracing::info!(remote = remote_key, session_id, "user session opened");
                    if let Err(err) = self.displays.set_session_id(path, &session_id).await {
                        tracing::warn!("error setting session id: {err:?}");
                    }
                } else {
                    tracing::info!(remote = remote_key, session_id, "user session opened, handing over");
                    self.hand_over(remote_key, &session_id).await;
                }
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
            remote.handed_over = true;
        }
    }

    /// grd removed the handover object of a session. If that's a greeter the client never got to,
    /// grd gave up on the client, and nothing else would end the greeter.
    fn on_handover_removed(&mut self, session_id: &str) {
        let Some((&remote_key, remote)) = self.remotes.iter_mut().find(|(_, remote)| {
            remote.greeter_session_id.as_deref() == Some(session_id)
        }) else {
            return;
        };
        let logging_in = remote
            .login
            .as_ref()
            .is_some_and(|login| login.state == LoginState::Started);
        if remote.handed_over || logging_in {
            return;
        }
        let Some(greeter) = &mut remote.greeter else {
            return;
        };
        // grd sends two signals per handover object (interface removed, object unexported).
        remote.greeter_session_id = None;

        tracing::info!(
            remote = remote_key,
            session_id,
            "grd dropped the client of this greeter, ending it"
        );
        greeter.terminate();
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
                    service: self.config.session.pam_service.clone(),
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
                                session_id: None,
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
                    command: self.config.session.command.clone(),
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
                && login.session_id.is_some()
                && remote.user_display.is_some())
            .then_some(key)
        })
    }

    /// Moves the connection of `remote_key` (now in its greeter session) to the user session of
    /// `existing`: pointing the existing session's display at the new client makes grd hand the
    /// client over to that session's `grd --handover`.
    async fn reconnect(&mut self, remote_key: u64, existing: u64) {
        let Some(remote) = self.remotes.get_mut(&remote_key) else {
            return;
        };
        remote.handed_over = true;
        let remote_id = remote.request.remote_id.clone();
        self.point_session_at(existing, remote_id).await;
        // The new connection's greeter ends on its own once grd moved the client (Logout).
    }

    /// Points the display of the user session of `existing` at the client `remote_id`, which
    /// makes grd hand that client over to the session's `grd --handover`.
    async fn point_session_at(&mut self, existing: u64, remote_id: OwnedObjectPath) {
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
    }

    /// A user grd already authenticated (Kerberos): no greeter, straight to their session, or
    /// back to the one they have (like GDM's autologin displays).
    async fn start_preauthenticated(&mut self, request: RemoteDisplayRequest) {
        let username = request.preauthenticated_user.clone().unwrap();

        // grd refuses root already; don't rely on it.
        if nix::unistd::User::from_name(&username)
            .ok()
            .flatten()
            .is_none_or(|user| user.uid.is_root())
        {
            tracing::warn!(username, "refusing preauthenticated login");
            return;
        }

        if let Some(existing) = self.find_user_session(&username, 0) {
            tracing::info!(existing, username, "preauthenticated user, reconnecting to the existing session");
            self.point_session_at(existing, request.remote_id).await;
            return;
        }

        let path = match self.displays.add(request.remote_id.clone(), "").await {
            Ok(path) => path,
            Err(err) => {
                tracing::warn!("error adding remote display: {err:?}");
                return;
            }
        };

        let worker_request = WorkerRequest {
            service: self.config.session.preauthenticated_pam_service.clone(),
            user: username.clone(),
            class: "user".to_owned(),
            rhost: request.hostname.clone(),
            command: Some(self.config.session.command.clone()),
            env: vec!["XDG_SESSION_DESKTOP=niri".to_owned()],
        };
        let remote_key = self.next_key();
        let session_key = self.next_key();
        let session =
            match Session::spawn(&worker_request, session_key, self.session_events.clone()).await {
                Ok(session) => session,
                Err(err) => {
                    tracing::warn!("error starting user session: {err:?}");
                    let _ = self.displays.remove(&path).await;
                    return;
                }
            };
        self.sessions.insert(session_key, (remote_key, Role::User));

        tracing::info!(
            remote = remote_key,
            %path,
            remote_id = %request.remote_id,
            username,
            "new remote display for a preauthenticated user"
        );
        self.remotes.insert(
            remote_key,
            Remote {
                request,
                greeter_user: None,
                greeter: None,
                greeter_display: None,
                greeter_session_id: None,
                greeter_socket: None,
                login: Some(Login {
                    username,
                    session,
                    state: LoginState::Started,
                    session_id: None,
                    reply: None,
                }),
                user_display: Some(path),
                handed_over: false,
            },
        );
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
        let uid = remote.greeter_user.as_ref().map(|user| user.uid);
        let has_user_session = remote
            .login
            .as_ref()
            .is_some_and(|login| login.state == LoginState::Started);

        // Kill whatever the greeter user still has running, so the next greeter starts clean.
        if let Some(uid) = uid.filter(|_| !self.session_bus) {
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
