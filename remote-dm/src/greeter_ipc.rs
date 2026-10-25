//! greetd's IPC protocol, served to our greeters, so any greetd greeter works as the login screen.
//!
//! Each greeter session gets its own socket (`GREETD_SOCK`), owned by and only usable by its
//! greeter user. Messages are a native-endian u32 length followed by JSON; the types below match
//! greetd_ipc on the wire.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::worker::PromptKind;

pub const SOCKET_DIR: &str = "/run/remote-dm";

/// Larger messages aren't legitimate.
const MAX_MESSAGE: u32 = 64 * 1024;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    CreateSession {
        username: String,
    },
    PostAuthMessageResponse {
        response: Option<String>,
    },
    StartSession {
        cmd: Vec<String>,
        #[serde(default)]
        env: Vec<String>,
    },
    CancelSession,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorType {
    Error,
    AuthError,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Success,
    Error {
        error_type: ErrorType,
        description: String,
    },
    AuthMessage {
        auth_message_type: PromptKind,
        auth_message: String,
    },
}

impl Response {
    pub fn error(description: impl Into<String>) -> Self {
        Self::Error {
            error_type: ErrorType::Error,
            description: description.into(),
        }
    }
}

/// A request from a greeter, to be answered through `reply`.
pub struct GreeterRequest {
    /// Which remote display's greeter sent it.
    pub remote: u64,
    pub request: Request,
    pub reply: oneshot::Sender<Response>,
}

/// A greeter's socket; removed when dropped.
pub struct Listener {
    path: PathBuf,
    task: JoinHandle<()>,
}

impl Listener {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Creates the socket for the greeter of `remote`, usable only by `uid`.
pub fn listen(
    remote: u64,
    uid: u32,
    requests: mpsc::UnboundedSender<GreeterRequest>,
) -> anyhow::Result<Listener> {
    std::fs::create_dir_all(SOCKET_DIR)?;
    std::fs::set_permissions(SOCKET_DIR, std::fs::Permissions::from_mode(0o755))?;

    let path = PathBuf::from(format!("{SOCKET_DIR}/greeter-{remote}.sock"));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).with_context(|| format!("error binding {path:?}"))?;
    std::os::unix::fs::chown(&path, Some(uid), None)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;

    let task = tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(err) => {
                    tracing::warn!("error accepting greeter connection: {err}");
                    continue;
                }
            };

            // The socket permissions already restrict this; check anyway.
            match stream.peer_cred() {
                Ok(cred) if cred.uid() == uid || cred.uid() == 0 => (),
                Ok(cred) => {
                    tracing::warn!(uid = cred.uid(), "refusing greeter connection");
                    continue;
                }
                Err(err) => {
                    tracing::warn!("error getting greeter credentials: {err}");
                    continue;
                }
            }

            let requests = requests.clone();
            tokio::spawn(async move {
                if let Err(err) = serve(stream, remote, requests).await {
                    tracing::debug!(remote, "greeter connection ended: {err:#}");
                }
            });
        }
    });

    Ok(Listener { path, task })
}

async fn serve(
    mut stream: UnixStream,
    remote: u64,
    requests: mpsc::UnboundedSender<GreeterRequest>,
) -> anyhow::Result<()> {
    loop {
        // greetd frames messages with a native-endian length.
        let mut len = [0; 4];
        stream.read_exact(&mut len).await?;
        let len = u32::from_ne_bytes(len);
        anyhow::ensure!(len <= MAX_MESSAGE, "message too long: {len}");
        let mut body = vec![0; len as usize];
        stream.read_exact(&mut body).await?;

        let response = match serde_json::from_slice::<Request>(&body) {
            Ok(request) => {
                // Don't log responses to auth prompts, they're passwords.
                if !matches!(request, Request::PostAuthMessageResponse { .. }) {
                    tracing::debug!(remote, ?request, "greeter request");
                }
                let (reply, response) = oneshot::channel();
                requests
                    .send(GreeterRequest {
                        remote,
                        request,
                        reply,
                    })
                    .context("shutting down")?;
                response
                    .await
                    .unwrap_or_else(|_| Response::error("the session went away"))
            }
            Err(err) => Response::error(format!("bad request: {err}")),
        };

        let body = serde_json::to_vec(&response)?;
        stream.write_all(&(body.len() as u32).to_ne_bytes()).await?;
        stream.write_all(&body).await?;
        stream.flush().await?;
    }
}
