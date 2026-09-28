//! The agent's end: the process running it answers commands.

use std::fmt;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use moil_proto::BundleHash;
use serde::Serialize;
use tokio::io::BufReader;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use super::VERSION;
use super::messages::{
    ClientHello, PairStep, PairingCode, Reply, Request, ReviewFiles, ServerHello, ServiceBundles,
};
use super::socket::{self, ShortPath, private_dir, remove_if_present, user_id};
use crate::report::{AgentInfo, ListedBundle, ServiceRef};
use crate::status::{self, Clock};
use crate::{AgentHandle, Error, Service};

/// The longest request the agent reads, in bytes.
const MAX_REQUEST_BYTES: usize = 1 << 20;

/// What only the process running the agent can do for a command.
pub trait HostControls: Send + Sync + 'static {
    /// Starts quitting: the host shuts the agent down, then the control
    /// server, then exits (the app quits, or `moil agent` returns).
    /// Returns at once, and may be called again while the host quits.
    fn quit(&self);

    /// Whether the app opens at login, once turned on or off if `set` says
    /// to, or why that can't be told or changed, as a sentence for the
    /// owner. Only the app opens at login, so other hosts keep this
    /// answer.
    fn login_item(&self, set: Option<bool>) -> BoxFuture<'_, Result<bool, String>> {
        let _ = set;
        async {
            Err(
                "only the moil app can open at login, and this agent runs as `moil agent`; \
                 open the app to use Open at Login"
                    .to_owned(),
            )
        }
        .boxed()
    }
}

/// The control socket, served while it lives. Shut it down with
/// [`shutdown`](Self::shutdown); dropping it stops listening too, without
/// waiting for connections to close.
pub struct ServerHandle {
    socket: PathBuf,
    /// The socket file's device and inode, to remove it only while it is
    /// the one this server made.
    file: Option<(u64, u64)>,
    stop: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl fmt::Debug for ServerHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerHandle")
            .field("socket", &self.socket)
            .finish_non_exhaustive()
    }
}

impl ServerHandle {
    /// Where the socket is.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Stops listening, closes every connection and removes the socket. A
    /// client waiting for the agent to quit learns it has once its
    /// connection closes, so a host shuts the server down after the agent,
    /// just before it lets go of the home.
    pub async fn shutdown(mut self) {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        self.remove_socket();
    }

    fn remove_socket(&mut self) {
        let Some(file) = self.file.take() else {
            return;
        };
        if let Ok(meta) = std::fs::symlink_metadata(&self.socket)
            && (meta.dev(), meta.ino()) == file
            && let Err(e) = std::fs::remove_file(&self.socket)
        {
            tracing::warn!("couldn't remove {}: {e}", self.socket.display());
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop.cancel();
        self.remove_socket();
    }
}

/// Serves the control socket of the home `agent` runs on, until the handle
/// is shut down or dropped: commands then reach the agent, and `info` tells
/// them what runs it. A stale socket, left by an agent that didn't get to
/// remove it, is replaced: the agent holds the home's lock, so no other
/// agent serves it. Must be called from within a Tokio runtime.
pub fn serve(
    agent: AgentHandle,
    info: AgentInfo,
    host: Arc<dyn HostControls>,
) -> Result<ServerHandle, Error> {
    let socket = agent.machine().home().socket();
    let (listener, file) = listen(&socket).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "couldn't listen for moil commands at {}: {e}",
                socket.display()
            ),
        )
    })?;
    let shared = Arc::new(Shared {
        agent,
        info,
        host,
        changes: watch::Sender::new(()),
    });
    let stop = CancellationToken::new();
    let task = tokio::spawn(accept(listener, shared, stop.clone()));
    Ok(ServerHandle {
        socket,
        file: Some(file),
        stop,
        task: Some(task),
    })
}

/// Binds the socket, readable only by the owner, in its private directory.
fn listen(socket: &Path) -> io::Result<(UnixListener, (u64, u64))> {
    if let Some(dir) = socket.parent() {
        private_dir(dir)?;
    }
    remove_if_present(socket)?;
    let listener = UnixListener::bind(ShortPath::to(socket)?.path())?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    let meta = std::fs::symlink_metadata(socket)?;
    Ok((listener, (meta.dev(), meta.ino())))
}

async fn accept(listener: UnixListener, shared: Arc<Shared>, stop: CancellationToken) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            () = stop.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    connections.spawn(converse(stream, shared.clone()));
                }
                Err(e) => {
                    // Such as running out of file descriptors, which
                    // retrying at once wouldn't fix.
                    tracing::warn!("couldn't accept a command: {e}");
                    tokio::select! {
                        () = stop.cancelled() => break,
                        () = tokio::time::sleep(Duration::from_millis(100)) => {}
                    }
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    drop(listener);
    connections.shutdown().await;
}

/// Everything connections share.
struct Shared {
    agent: AgentHandle,
    info: AgentInfo,
    host: Arc<dyn HostControls>,
    /// Marked changed whenever a command changed what a status shows, for
    /// watchers: not every such change shows in the agent's snapshot.
    changes: watch::Sender<()>,
}

/// One client's conversation: the hellos, then its requests, one at a
/// time.
async fn converse(stream: UnixStream, shared: Arc<Shared>) {
    match stream.peer_cred() {
        Ok(peer) if peer.uid() == user_id() => {}
        Ok(peer) => {
            tracing::warn!(
                "refused a command from user {}: only this machine's owner may control moil",
                peer.uid()
            );
            return;
        }
        Err(e) => {
            tracing::warn!("refused a command from a process whose user moil couldn't tell: {e}");
            return;
        }
    }
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let Ok(Some(hello)) = socket::read::<ClientHello>(&mut reader, MAX_REQUEST_BYTES).await else {
        return;
    };
    let ours = ServerHello {
        version: VERSION.to_owned(),
        agent: shared.info.clone(),
    };
    if socket::write(&mut writer, &ours).await.is_err() || hello.version != VERSION {
        return;
    }
    loop {
        let request = match socket::read::<Request>(&mut reader, MAX_REQUEST_BYTES).await {
            Ok(Some(request)) => request,
            Ok(None) => return,
            Err(e) => {
                let error = format!("the agent couldn't read a command: {e}");
                let _ = socket::write(&mut writer, &Reply::<()>::Error(error)).await;
                return;
            }
        };
        let reply = match request {
            Request::Watch => return shared.watch(reader, writer).await,
            Request::Pair { service, name } => {
                return shared.pair(service, name, reader, writer).await;
            }
            Request::Quit => {
                // The connection closes as the host shuts the server down,
                // which tells the client that the agent has stopped.
                let replied = socket::write(&mut writer, &Reply::Ok(())).await;
                shared.host.quit();
                if replied.is_err() {
                    return;
                }
                continue;
            }
            request => shared.answer(request).await,
        };
        if socket::write(&mut writer, &Reply::from(reply))
            .await
            .is_err()
        {
            return;
        }
    }
}

impl Shared {
    /// Carries out a request that has one answer.
    async fn answer(&self, request: Request) -> Result<serde_json::Value, String> {
        let agent = &self.agent;
        match request {
            Request::Status => json(status::from_agent(agent, &self.info).await),
            Request::Pause | Request::Resume => {
                let paused = matches!(request, Request::Pause);
                agent.set_paused(paused);
                agent.settled().await;
                value(agent.snapshot().paused)
            }
            Request::Stop => {
                let stopped = agent.stop_running_job().await;
                value(stopped.map(|job| status::running_job(&job, &Clock::now())))
            }
            Request::Unpair { service } => {
                let service = self.find(service).await?;
                let unpaired = agent.unpair(&service.id).await;
                self.changed();
                json(unpaired.map(|()| ServiceRef::from(&service)))
            }
            Request::Bundles { service } => {
                let service = self.find(service).await?;
                let offered = agent.machine().bundles(&service.id).await;
                json(offered.map(|offered| ServiceBundles {
                    bundles: ListedBundle::listing(&offered, &service.approvals),
                    service: ServiceRef::from(&service),
                }))
            }
            Request::Review { service, hash } => {
                let service = self.find(service).await?;
                let review = agent.review(&service.id, hash).await;
                json(review.map(|review| ReviewFiles {
                    files: ReviewFiles::files(&review.bundle),
                    previous: review.previous.as_ref().map(ReviewFiles::files),
                    approved: review.approved,
                    service: review.service,
                }))
            }
            Request::Approve { service, hash } => {
                let service = self.find(service).await?;
                let review = agent
                    .review(&service.id, hash)
                    .await
                    .map_err(|e| e.to_string())?;
                let approved = agent.approve(&review).await;
                self.changed();
                json(approved)
            }
            Request::Revoke { service, hash } => {
                let service = self.find(service).await?;
                self.revoke(&service, hash).await
            }
            Request::LoginItem { set } => json(self.host.login_item(set).await),
            Request::Watch | Request::Pair { .. } | Request::Quit => {
                unreachable!("{request:?} takes the connection over")
            }
        }
    }

    async fn revoke(
        &self,
        service: &Service,
        hash: BundleHash,
    ) -> Result<serde_json::Value, String> {
        if !service.is_approved(&hash) {
            return Err(format!("{hash} isn't approved for {}", service.id));
        }
        let revoked = self.agent.revoke(&service.id, hash).await;
        self.changed();
        json(revoked)
    }

    /// The paired service `query` names.
    async fn find(&self, query: String) -> Result<Service, String> {
        self.agent
            .machine()
            .blocking(move |machine| machine.find_service(&query))
            .await
            .map_err(|e| e.to_string())
    }

    fn changed(&self) {
        self.changes.send_replace(());
    }

    /// Sends the status now, then whenever it changes, until the client
    /// leaves or the agent stops.
    async fn watch(&self, mut reader: BufReader<OwnedReadHalf>, mut writer: OwnedWriteHalf) {
        let mut snapshots = self.agent.subscribe();
        let mut changes = self.changes.subscribe();
        let mut sent = None;
        loop {
            snapshots.borrow_and_update();
            changes.borrow_and_update();
            let status = match status::from_agent(&self.agent, &self.info).await {
                Ok(status) => status,
                Err(e) => {
                    let _ = socket::write(&mut writer, &Reply::<()>::Error(e.to_string())).await;
                    return;
                }
            };
            if sent.as_ref() != Some(&status) {
                if socket::write(&mut writer, &Reply::Ok(&status))
                    .await
                    .is_err()
                {
                    return;
                }
                sent = Some(status);
            }
            tokio::select! {
                changed = snapshots.changed() => if changed.is_err() {
                    return;
                },
                _ = changes.changed() => {}
                () = socket::closed(&mut reader) => return,
            }
        }
    }

    /// Pairs as the owner would in the app: sends the code to confirm,
    /// then the paired service. Abandons the pairing if the client leaves
    /// before the owner confirms.
    async fn pair(
        &self,
        target: String,
        name: Option<String>,
        mut reader: BufReader<OwnedReadHalf>,
        mut writer: OwnedWriteHalf,
    ) {
        let (session, machine_name) = match self.start_pairing(&target, name).await {
            Ok(started) => started,
            Err(e) => {
                let _ = socket::write(&mut writer, &Reply::<()>::Error(e.to_string())).await;
                return;
            }
        };
        let code = PairingCode {
            user_code: session.user_code.clone(),
            verification_uri: session.verification_uri.clone(),
            verification_uri_complete: session.verification_uri_complete.clone(),
            service_name: session.service_name.clone(),
            base_url: session.base_url().clone(),
            machine_name,
            expires_at: Clock::now().wall(session.expires_at),
        };
        if socket::write(&mut writer, &Reply::Ok(PairStep::Code(code)))
            .await
            .is_err()
        {
            return;
        }
        let paired = tokio::select! {
            paired = session.wait() => paired,
            () = socket::closed(&mut reader) => return,
        };
        let reply = match paired {
            Ok(service) => {
                // The agent learns of the pairing as it completes; once it
                // has, a status shows the service connecting.
                self.agent.settled().await;
                self.changed();
                Reply::Ok(PairStep::Paired(ServiceRef::from(&service)))
            }
            Err(e) => Reply::Error(e.to_string()),
        };
        let _ = socket::write(&mut writer, &reply).await;
    }

    async fn start_pairing(
        &self,
        target: &str,
        name: Option<String>,
    ) -> Result<(crate::PairingSession, String), Error> {
        let machine = self.agent.machine();
        if let Some(name) = name {
            machine
                .blocking(|machine| machine.update_settings(|s| s.machine_name = Some(name)))
                .await?;
        }
        let session = self.agent.pair(target).await?;
        // The computer's name may take detecting.
        let machine_name = machine.blocking(|machine| Ok(machine.info().name)).await?;
        Ok((session, machine_name))
    }
}

fn json<T: Serialize, E: fmt::Display>(result: Result<T, E>) -> Result<serde_json::Value, String> {
    value(result.map_err(|e| e.to_string())?)
}

fn value(value: impl Serialize) -> Result<serde_json::Value, String> {
    Ok(serde_json::to_value(value).expect("control messages serialize to JSON"))
}
