//! Daemon side of session workers (see `worker`).

use std::process::Stdio;

use anyhow::Context as _;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use crate::worker::{WorkerEvent, WorkerInput, WorkerRequest};

/// Identifies a session worker in events.
pub type SessionKey = u64;

/// A worker event, tagged with the worker it came from.
pub type Event = (SessionKey, WorkerEvent);

pub struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl Session {
    /// Starts a session worker. Its events arrive on `events` tagged with `key`; the last one is
    /// always `Exited` or `Failed`.
    pub async fn spawn(
        request: &WorkerRequest,
        key: SessionKey,
        events: mpsc::UnboundedSender<Event>,
    ) -> anyhow::Result<Self> {
        // Run the binary we are, even if it was replaced or removed since we started (e.g. a
        // package update before the service was restarted): in the forked child, /proc/self/exe
        // still is our binary. This also keeps the worker at the same version as the daemon.
        let mut child = Command::new("/proc/self/exe")
            .arg("session-worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("error starting session worker")?;

        let mut session = Self {
            stdin: child.stdin.take(),
            child,
        };
        session.write(request).await?;

        let stdout = session.child.stdout.take().unwrap();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let mut finished = false;
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<WorkerEvent>(&line) {
                    Ok(event) => {
                        finished |= matches!(
                            event,
                            WorkerEvent::Exited { .. } | WorkerEvent::Failed { .. }
                        );
                        let _ = events.send((key, event));
                    }
                    Err(err) => tracing::warn!("bad worker output {line:?}: {err}"),
                }
            }
            if !finished {
                let event = WorkerEvent::Failed {
                    error: "session worker died".to_owned(),
                    auth: false,
                };
                let _ = events.send((key, event));
            }
        });

        Ok(session)
    }

    pub async fn send(&mut self, input: &WorkerInput) -> anyhow::Result<()> {
        self.write(input).await
    }

    async fn write(&mut self, value: &impl serde::Serialize) -> anyhow::Result<()> {
        let stdin = self.stdin.as_mut().context("worker stdin is closed")?;
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        stdin.write_all(&line).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Asks the session to end (SIGTERM to the worker, which forwards it to the session).
    pub fn terminate(&mut self) {
        if let Some(pid) = self.child.id() {
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
    }
}
