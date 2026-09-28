//! A command's end: reaching the agent running on a home.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use moil_bundle::Bundle;
use moil_proto::BundleHash;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use super::VERSION;
use super::messages::{
    ClientHello, PairStep, PairingCode, Reply, Request, ReviewFiles, ServerHello, ServiceBundles,
};
use super::socket::{self, ShortPath, user_id};
use crate::report::{AgentInfo, RunningJob, ServiceRef, Status};
use crate::{Home, Review};

/// The longest answer a client reads, in bytes: a review holds two
/// versions of a bundle of up to 8 MiB, and JSON may escape much of them.
const MAX_REPLY_BYTES: usize = 256 << 20;

/// How long an agent may take to answer a client's hello. It answers at
/// once unless its process is stuck.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a command couldn't get what it asked of the agent.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// No agent runs on the home: there is no socket, or nothing listens
    /// on it. Commands then work on the home itself, or say that they need
    /// an agent.
    #[error("moil isn't running on {}; open the moil app or run `moil agent`", .0.display())]
    NoAgent(PathBuf),
    /// The agent runs another version of moil.
    #[error(
        "the agent running on {} is moil {agent}, and this is moil {ours}; use {}, which is the agent's own",
        home.display(),
        home.join(".machine/bin/moil").display()
    )]
    OtherVersion {
        home: PathBuf,
        agent: String,
        ours: String,
    },
    /// The agent's sentence for why it couldn't do what was asked.
    #[error("{0}")]
    Agent(String),
    /// The conversation with the agent broke off, or made no sense.
    #[error("couldn't talk to the moil agent running on {}: {reason}", home.display())]
    Broken { home: PathBuf, reason: String },
}

/// A connection to the agent running on a home, for one command. Requests
/// are answered one at a time; those that keep answering ([`watch`],
/// [`pair`]) or end the agent ([`quit`]) take the connection over.
///
/// [`watch`]: Self::watch
/// [`pair`]: Self::pair
/// [`quit`]: Self::quit
#[derive(Debug)]
pub struct Client {
    home: PathBuf,
    agent: AgentInfo,
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Client {
    /// Connects to the agent running on `home`, failing with
    /// [`ClientError::NoAgent`] when none does.
    pub async fn connect(home: &Home) -> Result<Self, ClientError> {
        let root = std::path::absolute(home.root()).unwrap_or_else(|_| home.root().to_owned());
        let no_agent = || ClientError::NoAgent(root.clone());
        let broken = |reason: String| ClientError::Broken {
            home: root.clone(),
            reason,
        };
        let socket = home.socket();
        let stream = match ShortPath::to(&socket) {
            Ok(short) => UnixStream::connect(short.path()).await,
            Err(e) => return Err(broken(e.to_string())),
        };
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) if is_nobody_there(&e) => return Err(no_agent()),
            Err(e) => return Err(broken(format!("{}: {e}", socket.display()))),
        };
        match stream.peer_cred() {
            Ok(peer) if peer.uid() == user_id() => {}
            Ok(peer) => {
                return Err(broken(format!(
                    "{} belongs to user {}, not to you",
                    socket.display(),
                    peer.uid()
                )));
            }
            Err(e) => return Err(broken(e.to_string())),
        }
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let hello = async {
            let ours = ClientHello {
                version: VERSION.to_owned(),
            };
            socket::write(&mut writer, &ours).await?;
            socket::read::<serde_json::Value>(&mut reader, MAX_REPLY_BYTES).await
        };
        let hello = match tokio::time::timeout(HELLO_TIMEOUT, hello).await {
            Ok(Ok(Some(hello))) => hello,
            // An agent that is shutting down closes connections as they
            // come.
            Ok(Ok(None)) => return Err(no_agent()),
            Ok(Err(e)) if is_nobody_there(&e) => return Err(no_agent()),
            Ok(Err(e)) => return Err(broken(e.to_string())),
            Err(_) => {
                return Err(broken(format!(
                    "it didn't answer within {} seconds",
                    HELLO_TIMEOUT.as_secs()
                )));
            }
        };
        let version = hello.get("version").and_then(|v| v.as_str());
        if version != Some(VERSION) {
            return Err(ClientError::OtherVersion {
                home: root.clone(),
                agent: version.unwrap_or("of an unknown version").to_owned(),
                ours: VERSION.to_owned(),
            });
        }
        let hello: ServerHello =
            serde_json::from_value(hello).map_err(|e| broken(format!("its hello: {e}")))?;
        Ok(Self {
            home: root,
            agent: hello.agent,
            reader,
            writer,
        })
    }

    /// What runs the agent.
    pub fn agent(&self) -> &AgentInfo {
        &self.agent
    }

    pub async fn status(&mut self) -> Result<Status, ClientError> {
        self.call(&Request::Status).await
    }

    /// The status now, then whenever it changes.
    pub async fn watch(mut self) -> Result<Watch, ClientError> {
        self.send(&Request::Watch).await?;
        Ok(Watch { client: self })
    }

    /// Stops or resumes taking new jobs, and says whether the machine is
    /// paused now. A running job carries on.
    pub async fn set_paused(&mut self, paused: bool) -> Result<bool, ClientError> {
        let request = if paused {
            Request::Pause
        } else {
            Request::Resume
        };
        self.call(&request).await
    }

    /// Stops the running job, and says which it was: `None` if no job
    /// ran. Its service may give it to another machine.
    pub async fn stop(&mut self) -> Result<Option<RunningJob>, ClientError> {
        self.call(&Request::Stop).await
    }

    /// Starts pairing with `service`, a base URL, calling this machine
    /// `name` from now on if given. The agent pairs, keeps the token, and
    /// connects to the service once the owner confirms.
    pub async fn pair(mut self, service: &str, name: Option<&str>) -> Result<Pairing, ClientError> {
        let request = Request::Pair {
            service: service.to_owned(),
            name: name.map(str::to_owned),
        };
        match self.call(&request).await? {
            PairStep::Code(code) => Ok(Pairing { code, client: self }),
            PairStep::Paired(_) => Err(self.broken("it paired before sending a code".into())),
        }
    }

    /// Unpairs from a service, and says which it was.
    pub async fn unpair(&mut self, service: &str) -> Result<ServiceRef, ClientError> {
        let service = service.to_owned();
        self.call(&Request::Unpair { service }).await
    }

    /// The bundles a service offers, as it describes them, and those
    /// approved for it that it no longer offers.
    pub async fn bundles(&mut self, service: &str) -> Result<ServiceBundles, ClientError> {
        let service = service.to_owned();
        self.call(&Request::Bundles { service }).await
    }

    /// A bundle in full, fetched by the agent and verified here against
    /// `hash`, with the approved version it would replace.
    pub async fn review(&mut self, service: &str, hash: BundleHash) -> Result<Review, ClientError> {
        let service = service.to_owned();
        let sent: ReviewFiles = self.call(&Request::Review { service, hash }).await?;
        let bundle = Bundle::from_files(sent.files)
            .and_then(|bundle| bundle.verify(&hash).map(|()| bundle))
            .map_err(|e| self.broken(format!("the bundle it sent: {e}")))?;
        let previous = sent
            .previous
            .map(Bundle::from_files)
            .transpose()
            .map_err(|e| self.broken(format!("the approved version it sent: {e}")))?;
        Ok(Review {
            service: sent.service,
            bundle,
            previous,
            approved: sent.approved,
        })
    }

    /// Approves the bundle `hash` names for a service. The agent fetches
    /// it from the service and approves it only if it hashes to `hash`.
    pub async fn approve(&mut self, service: &str, hash: BundleHash) -> Result<(), ClientError> {
        let service = service.to_owned();
        self.call(&Request::Approve { service, hash }).await
    }

    /// Withdraws an approval, stopping a running attempt of that bundle for
    /// that service.
    pub async fn revoke(&mut self, service: &str, hash: BundleHash) -> Result<(), ClientError> {
        let service = service.to_owned();
        self.call(&Request::Revoke { service, hash }).await
    }

    /// Whether the app opens at login, once turned on or off if `set` says
    /// to. Only the app can.
    pub async fn login_item(&mut self, set: Option<bool>) -> Result<bool, ClientError> {
        self.call(&Request::LoginItem { set }).await
    }

    /// Stops the agent: the running job is interrupted and reported, and
    /// the process running the agent exits. Returns once it has stopped.
    pub async fn quit(mut self) -> Result<(), ClientError> {
        self.call::<()>(&Request::Quit).await?;
        // The agent closes the connection once it has stopped, and says
        // nothing more before.
        loop {
            match self.reader.fill_buf().await {
                Ok([]) | Err(_) => return Ok(()),
                Ok(unexpected) => {
                    let n = unexpected.len();
                    self.reader.consume(n);
                }
            }
        }
    }

    async fn call<T: DeserializeOwned>(&mut self, request: &Request) -> Result<T, ClientError> {
        self.send(request).await?;
        match self.receive().await? {
            Some(value) => Ok(value),
            None => Err(self.broken("it closed the connection; it may have quit".into())),
        }
    }

    async fn send(&mut self, request: &Request) -> Result<(), ClientError> {
        socket::write(&mut self.writer, request)
            .await
            .map_err(|e| self.broken(e.to_string()))
    }

    /// The next answer, or `None` once the agent has closed the
    /// connection.
    async fn receive<T: DeserializeOwned>(&mut self) -> Result<Option<T>, ClientError> {
        match socket::read::<Reply<T>>(&mut self.reader, MAX_REPLY_BYTES).await {
            Ok(Some(Reply::Ok(value))) => Ok(Some(value)),
            Ok(Some(Reply::Error(sentence))) => Err(ClientError::Agent(sentence)),
            Ok(None) => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => Ok(None),
            Err(e) => Err(self.broken(e.to_string())),
        }
    }

    fn broken(&self, reason: String) -> ClientError {
        ClientError::Broken {
            home: self.home.clone(),
            reason,
        }
    }
}

/// Whether connecting failed because no agent is there to answer.
fn is_nobody_there(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::NotADirectory
    )
}

/// The status of the home, now and whenever it changes.
#[derive(Debug)]
pub struct Watch {
    client: Client,
}

impl Watch {
    /// The next status: the current one first, then each that differs
    /// from the one before. `None` once the agent has stopped.
    pub async fn next(&mut self) -> Result<Option<Status>, ClientError> {
        self.client.receive().await
    }
}

/// A pairing waiting for the owner to confirm its code on the service.
/// Dropping it abandons the pairing, unless the service has already sent
/// the machine's token: then the pairing completes anyway.
#[derive(Debug)]
pub struct Pairing {
    pub code: PairingCode,
    client: Client,
}

impl Pairing {
    /// Waits until the owner confirms or rejects the pairing on the
    /// service, or the code expires. Once confirmed, the agent connects to
    /// the service.
    pub async fn confirmed(mut self) -> Result<ServiceRef, ClientError> {
        let client = &mut self.client;
        match client.receive().await? {
            Some(PairStep::Paired(service)) => Ok(service),
            Some(PairStep::Code(_)) => Err(client.broken("it sent a second code".into())),
            None => Err(client.broken("it stopped before the pairing was confirmed".into())),
        }
    }
}
