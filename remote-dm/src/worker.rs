//! Session worker: one process per PAM session (`remote-dm session-worker`).
//!
//! The daemon is async and multi-purpose; PAM and fork/setuid want a plain single-threaded
//! process. So the daemon starts a worker per session and talks to it with JSON lines: a
//! `WorkerRequest` first, then `WorkerInput`s, on the worker's stdin; `WorkerEvent`s on its stdout.
//!
//! The worker authenticates (relaying PAM's prompts: `Prompt` → `Answer`), reports
//! `Authenticated`, and, if the request had no command, waits for `Start` (or `Cancel`). Then it
//! opens the PAM session (which makes pam_systemd create the logind session), reports the session
//! ID, runs the session command as the user, waits for it, and closes the session. SIGTERM is
//! forwarded to the session command.
//!
//! Authentication and the session have to happen on the same PAM handle, which is why one worker
//! does both. The flow follows greetd's session worker.

use std::cell::RefCell;
use std::ffi::CString;
use std::io::{BufRead as _, StdinLock, Write as _};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{bail, Context as _};
use nix::sys::signal::{self, SigHandler, Signal};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{self, ForkResult, Pid, User};
use pam_sys::{PamFlag, PamItemType};
use serde::{Deserialize, Serialize};

use crate::pam::converse::Converse;
use crate::pam::session::PamSession;

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkerRequest {
    /// PAM service name (a file in /etc/pam.d).
    pub service: String,
    pub user: String,
    /// `XDG_SESSION_CLASS`: "greeter" or "user".
    pub class: String,
    /// Remote host, for `PAM_RHOST`; this also makes the logind session remote.
    pub rhost: Option<String>,
    /// Run with `sh -c` as the user. `None`: wait for `WorkerInput::Start` after authenticating.
    pub command: Option<String>,
    /// Extra environment for PAM and the session (`KEY=VALUE`).
    pub env: Vec<String>,
}

/// From the daemon, after the request.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerInput {
    /// Answer to the last `Prompt` (`None` for info and error messages).
    Answer { response: Option<String> },
    /// Open the session and run this.
    Start { command: String, env: Vec<String> },
    /// Give up before the session started.
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptKind {
    /// Show what the user types (e.g. a user name).
    Visible,
    /// Hide what the user types (e.g. a password).
    Secret,
    /// Just a message; answer with `None`.
    Info,
    /// Just an error message; answer with `None`.
    Error,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerEvent {
    /// PAM asks something; the daemon must send an `Answer`.
    Prompt { kind: PromptKind, message: String },
    /// Authentication and account checks passed.
    Authenticated,
    /// The PAM session is open and the command started.
    Opened { session_id: String },
    /// Something failed before the command could start. `auth`: it was the authentication.
    Failed { error: String, auth: bool },
    /// The command exited and the session was closed.
    Exited { code: i32 },
}

/// The running session command, for forwarding SIGTERM.
static CHILD: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward_sigterm(_: libc::c_int) {
    let pid = CHILD.load(Ordering::SeqCst);
    if pid > 0 {
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

fn send(event: &WorkerEvent) {
    let mut out = std::io::stdout().lock();
    let _ = serde_json::to_writer(&mut out, event);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

fn recv(stdin: &mut StdinLock<'static>) -> anyhow::Result<WorkerInput> {
    let mut line = String::new();
    if stdin.read_line(&mut line)? == 0 {
        bail!("the daemon went away");
    }
    Ok(serde_json::from_str(&line)?)
}

/// Relays PAM's conversation to the daemon.
struct Conversation {
    stdin: Rc<RefCell<StdinLock<'static>>>,
}

impl Conversation {
    fn ask(&self, kind: PromptKind, message: &str) -> Result<Option<String>, ()> {
        send(&WorkerEvent::Prompt {
            kind,
            message: message.to_owned(),
        });
        match recv(&mut self.stdin.borrow_mut()) {
            Ok(WorkerInput::Answer { response }) => Ok(response),
            Ok(other) => {
                eprintln!("expected an answer, got {other:?}");
                Err(())
            }
            Err(err) => {
                eprintln!("{err:#}");
                Err(())
            }
        }
    }
}

impl Converse for Conversation {
    fn prompt_echo(&self, msg: &str) -> Result<String, ()> {
        self.ask(PromptKind::Visible, msg)?.ok_or(())
    }

    fn prompt_blind(&self, msg: &str) -> Result<String, ()> {
        self.ask(PromptKind::Secret, msg)?.ok_or(())
    }

    fn info(&self, msg: &str) -> Result<(), ()> {
        self.ask(PromptKind::Info, msg).map(drop)
    }

    fn error(&self, msg: &str) -> Result<(), ()> {
        self.ask(PromptKind::Error, msg).map(drop)
    }
}

/// An error that's the user's fault (wrong password and such), as opposed to ours.
#[derive(Debug)]
struct AuthFailed(String);

impl std::fmt::Display for AuthFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AuthFailed {}

pub fn main() -> ! {
    let code = match run() {
        Ok(code) => code,
        Err(err) => {
            let auth = err.downcast_ref::<AuthFailed>().is_some();
            send(&WorkerEvent::Failed {
                error: format!("{err:#}"),
                auth,
            });
            1
        }
    };
    std::process::exit(code);
}

fn run() -> anyhow::Result<i32> {
    let stdin = Rc::new(RefCell::new(std::io::stdin().lock()));
    let mut line = String::new();
    stdin
        .borrow_mut()
        .read_line(&mut line)
        .context("error reading request")?;
    let req: WorkerRequest = serde_json::from_str(&line).context("error parsing request")?;

    let user = User::from_name(&req.user)?
        .with_context(|| format!("no such user: {}", req.user))?;

    // The logind session belongs to our process group / session.
    unistd::setsid().context("error becoming session leader")?;

    let conv = Box::pin(Conversation {
        stdin: stdin.clone(),
    });
    let mut pam = PamSession::start(&req.service, &req.user, conv as Pin<Box<dyn Converse>>)
        .context("pam_start")?;

    if let Some(rhost) = &req.rhost {
        pam.set_item(PamItemType::RHOST, rhost)?;
    }

    pam.authenticate(PamFlag::NONE)
        .map_err(|err| AuthFailed(err.to_string()))?;
    pam.acct_mgmt(PamFlag::NONE)
        .map_err(|err| AuthFailed(err.to_string()))?;
    send(&WorkerEvent::Authenticated);

    let (command, start_env) = match req.command {
        Some(command) => (command, Vec::new()),
        None => {
            match recv(&mut stdin.borrow_mut())? {
                WorkerInput::Start { command, env } => (command, env),
                WorkerInput::Cancel => {
                    let _ = pam.end();
                    std::process::exit(0);
                }
                other => bail!("expected start or cancel, got {other:?}"),
            }
        }
    };

    pam.setcred(PamFlag::ESTABLISH_CRED)
        .context("error establishing credentials")?;

    let base_env = [
        format!("XDG_SESSION_CLASS={}", req.class),
        "XDG_SESSION_TYPE=wayland".to_owned(),
        format!("USER={}", user.name),
        format!("LOGNAME={}", user.name),
        format!("HOME={}", user.dir.to_string_lossy()),
        format!("SHELL={}", user.shell.to_string_lossy()),
    ];
    let extra_env = req.env.iter().chain(&start_env);
    // A default PATH, unless we were given one (PAM modules like pam_env may still change it).
    if !extra_env.clone().any(|var| var.starts_with("PATH=")) {
        pam.putenv("PATH=/usr/local/sbin:/usr/local/bin:/usr/bin")?;
    }
    for var in base_env.iter().chain(extra_env) {
        pam.putenv(var)?;
    }

    pam.open_session(PamFlag::NONE)
        .context("error opening session")?;

    let env: Vec<CString> = pam
        .getenvlist()?
        .to_vec()
        .into_iter()
        .map(|var| var.to_owned())
        .collect();
    let find = |key: &str| {
        env.iter()
            .filter_map(|var| var.to_str().ok()?.strip_prefix(key))
            .next()
            .map(str::to_owned)
    };
    let session_id = find("XDG_SESSION_ID=");
    let runtime_dir = find("XDG_RUNTIME_DIR=");
    let Some(session_id) = session_id else {
        let _ = pam.close_session(PamFlag::NONE);
        bail!("PAM didn't give us a logind session (is pam_systemd in the stack?)");
    };

    // The user's D-Bus session bus, started by their systemd user instance.
    let mut child_env = env;
    if let Some(runtime_dir) = &runtime_dir {
        child_env.push(CString::new(format!(
            "DBUS_SESSION_BUS_ADDRESS=unix:path={runtime_dir}/bus"
        ))?);
    }

    let shell = CString::new("/bin/sh")?;
    let args = [
        shell.clone(),
        CString::new("-c")?,
        CString::new(format!("exec {command}"))?,
    ];
    let user_name = CString::new(user.name.clone())?;
    let home = user.dir.clone();

    let child = match unsafe { unistd::fork() }.context("fork")? {
        ForkResult::Child => {
            // Drop privileges and run the session command.
            let result = (|| -> nix::Result<()> {
                unistd::initgroups(&user_name, user.gid)?;
                unistd::setgid(user.gid)?;
                unistd::setuid(user.uid)?;
                let _ = unistd::chdir(&home);
                unistd::execve(&shell, &args, &child_env)?;
                Ok(())
            })();
            eprintln!("error starting session command: {result:?}");
            unsafe { libc::_exit(127) };
        }
        ForkResult::Parent { child } => child,
    };

    CHILD.store(child.as_raw(), Ordering::SeqCst);
    unsafe {
        signal::signal(Signal::SIGTERM, SigHandler::Handler(forward_sigterm))?;
        // We're in the session's scope: when logind ends the session, systemd sends SIGTERM and
        // then SIGHUP to everything in it. We need to outlive the session command to close the
        // PAM session and report back. (Only here, after the fork: an ignored signal would stay
        // ignored in the session command.)
        signal::signal(Signal::SIGHUP, SigHandler::SigIgn)?;
    }

    send(&WorkerEvent::Opened { session_id });

    let code = wait_for(child);

    let _ = pam.close_session(PamFlag::NONE);
    let _ = pam.setcred(PamFlag::DELETE_CRED);
    let _ = pam.end();

    send(&WorkerEvent::Exited { code });
    Ok(code)
}

fn wait_for(child: Pid) -> i32 {
    loop {
        match waitpid(child, None) {
            Ok(WaitStatus::Exited(_, code)) => return code,
            Ok(WaitStatus::Signaled(_, signal, _)) => return 128 + signal as i32,
            Ok(_) => continue,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(err) => {
                eprintln!("waitpid: {err}");
                return 1;
            }
        }
    }
}
