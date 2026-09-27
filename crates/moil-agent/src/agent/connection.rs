//! One service's control channel: opens it, relays messages both ways, and
//! reopens it with backoff until told to stop (spec §6).
//!
//! The task only moves frames. What to say is the coordinator's business: a
//! reader hands it every message the service sends, and a writer sends what
//! it queues in the channel's [`Outbox`]. Either one ending ends the
//! channel, so a service that stops reading is noticed as surely as one
//! that stops talking.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use moil_proto::{AttemptRef, JobId, ServiceMessage, close_code};
use serde::Deserialize;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode, header};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::AgentOptions;
use super::coordinator::Input;
use crate::{Error, Machine, PairedService, ServiceId, net};

/// The largest control message either side may send (spec §10).
const MAX_MESSAGE_BYTES: usize = 1 << 20;

/// The deepest `params` may nest (spec §10).
const MAX_PARAMS_DEPTH: usize = 64;

/// How many frames may wait in an [`Outbox`].
const OUTBOX_FRAMES: usize = 64;

/// How long closing a channel may take: writing what the coordinator
/// queued last, then the close frame.
pub(super) const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// What a connection task tells the coordinator.
#[derive(Debug)]
pub(super) enum LinkEvent {
    /// Opening the channel, for the first time or again.
    Connecting,
    /// The channel is open. The coordinator speaks first (`hello`), and
    /// every frame it sends goes through `outbox`.
    Open(Outbox),
    Message(ServiceMessage),
    /// An `assign` naming `attempt` that the machine can't read in full,
    /// or that asks for more than the protocol allows (spec §7.2).
    Unreadable {
        attempt: AttemptRef,
        reason: String,
    },
    /// The outbox was full, and the connection has since written what it
    /// held.
    Drained,
    /// The channel is gone; the task opens it again at `retry_at`.
    Lost {
        error: String,
        retry_at: Instant,
    },
    /// The service removed this machine (4401). The task has ended.
    Unpaired,
    /// Another connection with this machine's token took over (4409). The
    /// task has ended.
    Replaced,
}

/// Where the coordinator queues frames for an open channel. It holds a few
/// frames at most, so a service that reads slowly holds the machine back
/// instead of making it queue without end: once [`queue`](Self::queue)
/// has failed, the connection reports [`LinkEvent::Drained`] when there is
/// room again.
#[derive(Debug)]
pub(super) struct Outbox {
    frames: mpsc::Sender<String>,
    /// Set when a frame didn't fit, for the writer to report `Drained`.
    full: Arc<AtomicBool>,
}

impl Outbox {
    /// Queues `frame`. Fails if the outbox is full, or the channel is gone
    /// (the connection then reports that).
    pub(super) fn queue(&self, frame: String) -> bool {
        match self.frames.try_send(frame) {
            Ok(()) => true,
            Err(TrySendError::Closed(_)) => false,
            Err(TrySendError::Full(frame)) => {
                self.full.store(true, Ordering::SeqCst);
                // The writer may have emptied the outbox since: trying again
                // either queues the frame or leaves the writer to find the
                // flag once it has written what's there.
                self.frames.try_send(frame).is_ok()
            }
        }
    }
}

/// The writer's end of an [`Outbox`].
struct Frames {
    frames: mpsc::Receiver<String>,
    full: Arc<AtomicBool>,
}

fn outbox() -> (Outbox, Frames) {
    let (sender, receiver) = mpsc::channel(OUTBOX_FRAMES);
    let full = Arc::new(AtomicBool::new(false));
    let outbox = Outbox {
        frames: sender,
        full: full.clone(),
    };
    let frames = Frames {
        frames: receiver,
        full,
    };
    (outbox, frames)
}

/// Starts the task that keeps `service`'s channel open until `stop` is
/// cancelled. Every event it reports carries `task`, so the coordinator
/// can tell it from the tasks it replaced.
pub(super) fn spawn(
    machine: Machine,
    service: &PairedService,
    task: u64,
    options: AgentOptions,
    inputs: mpsc::UnboundedSender<Input>,
    stop: CancellationToken,
) -> JoinHandle<()> {
    let channel = Channel {
        machine,
        service: service.id.clone(),
        name: service.name.clone(),
        base_url: service.base_url.clone(),
        task,
        options,
        inputs,
        stop,
    };
    tokio::spawn(channel.run())
}

struct Channel {
    machine: Machine,
    service: ServiceId,
    name: String,
    base_url: Url,
    task: u64,
    options: AgentOptions,
    inputs: mpsc::UnboundedSender<Input>,
    stop: CancellationToken,
}

/// Why a channel ended.
enum End {
    /// The agent closed it.
    Stopped,
    Unpaired,
    Replaced,
    /// Anything else; worth trying again.
    Lost(String),
}

type Sink = SplitSink<Socket, Message>;
type Stream = SplitStream<Socket>;

impl Channel {
    async fn run(self) {
        let mut backoff = Backoff::new(&self.options);
        loop {
            self.report(LinkEvent::Connecting);
            let end = match self.open().await {
                Ok(socket) => self.serve(socket, &mut backoff).await,
                Err(end) => end,
            };
            let error = match end {
                End::Stopped => return,
                End::Unpaired => return self.report(LinkEvent::Unpaired),
                End::Replaced => return self.report(LinkEvent::Replaced),
                End::Lost(error) => error,
            };
            let delay = backoff.next();
            self.report(LinkEvent::Lost {
                error,
                retry_at: Instant::now() + delay,
            });
            tokio::select! {
                () = self.stop.cancelled() => return,
                () = tokio::time::sleep(delay) => {}
            }
        }
    }

    /// Opens the WebSocket with the machine's token (spec §6).
    async fn open(&self) -> Result<Socket, End> {
        let lost = |e: Error| End::Lost(e.to_string());
        self.machine.check_url(&self.base_url).map_err(lost)?;
        // The keychain may even ask the owner first.
        let id = self.service.clone();
        let token = self.machine.blocking(move |machine| machine.token(&id));
        let token = tokio::select! {
            () = self.stop.cancelled() => return Err(End::Stopped),
            token = token => match token {
                Ok(Some(token)) => token,
                Ok(None) => return Err(End::Unpaired),
                Err(e) => return Err(lost(e)),
            },
        };

        let mut url = Url::parse(&net::endpoint(&self.base_url, "v1/connect"))
            .expect("a base URL stays a URL when a path is added");
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .expect("http and https URLs can become ws and wss URLs");
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|e| End::Lost(net::describe(&e)))?;
        let headers = request.headers_mut();
        let bearer = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| End::Lost("the machine token isn't valid in a header".into()))?;
        headers.insert(header::AUTHORIZATION, bearer);
        if let Ok(agent) = HeaderValue::from_str(&format!("moil/{}", self.machine.app_version())) {
            headers.insert(header::USER_AGENT, agent);
        }

        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_MESSAGE_BYTES));
        let tls = Connector::Rustls(self.machine.net().tls.clone());
        let connecting = tokio_tungstenite::connect_async_tls_with_config(
            request,
            Some(config),
            true,
            Some(tls),
        );
        let connected = tokio::select! {
            () = self.stop.cancelled() => return Err(End::Stopped),
            connected = tokio::time::timeout(self.options.channel_timeout, connecting) => connected,
        };
        match connected {
            Ok(Ok((socket, _))) => Ok(socket),
            Ok(Err(tungstenite::Error::Http(response)))
                if response.status() == StatusCode::UNAUTHORIZED =>
            {
                Err(End::Unpaired)
            }
            Ok(Err(tungstenite::Error::Http(response))) => Err(End::Lost(format!(
                "{} answered {} instead of opening the channel",
                self.name,
                response.status()
            ))),
            Ok(Err(e)) => Err(End::Lost(format!(
                "couldn't reach {}: {}",
                self.base_url,
                net::describe(&e)
            ))),
            Err(_) => Err(End::Lost(format!("{} didn't answer", self.base_url))),
        }
    }

    /// Relays frames both ways until the channel ends.
    async fn serve(&self, socket: Socket, backoff: &mut Backoff) -> End {
        let (sink, stream) = socket.split();
        let (outbox, frames) = outbox();
        self.report(LinkEvent::Open(outbox));
        tokio::select! {
            end = self.write(sink, frames) => end,
            end = self.read(stream, backoff) => end,
        }
    }

    /// Writes what the coordinator queues until the agent stops the channel,
    /// then closes it. A write that makes no progress for the channel
    /// timeout means the service stopped reading, and the channel is dead
    /// (spec §6.1).
    async fn write(&self, mut sink: Sink, mut frames: Frames) -> End {
        let timeout = self.options.channel_timeout;
        loop {
            let first = tokio::select! {
                () = self.stop.cancelled() => return close(&mut sink, &mut frames).await,
                first = frames.frames.recv() => match first {
                    Some(first) => first,
                    // The coordinator let go of the channel.
                    None => return close(&mut sink, &mut frames).await,
                },
            };
            // Everything queued goes out together, flushed once.
            let written = async {
                sink.feed(Message::text(first)).await?;
                while let Ok(frame) = frames.frames.try_recv() {
                    sink.feed(Message::text(frame)).await?;
                }
                sink.flush().await
            };
            let written = tokio::select! {
                () = self.stop.cancelled() => None,
                written = tokio::time::timeout(timeout, written) => Some(written),
            };
            match written {
                None => return close(&mut sink, &mut frames).await,
                Some(Ok(Ok(()))) => {}
                Some(Ok(Err(e))) => return End::Lost(net::describe(&e)),
                Some(Err(_)) => {
                    return End::Lost(format!(
                        "the service took nothing for {} seconds",
                        timeout.as_secs_f64()
                    ));
                }
            }
            if frames.full.swap(false, Ordering::SeqCst) {
                self.report(LinkEvent::Drained);
            }
        }
    }

    /// Hands the coordinator every message the service sends. Silence for
    /// the channel timeout, pings included, means the channel is dead (spec
    /// §6.1).
    async fn read(&self, mut stream: Stream, backoff: &mut Backoff) -> End {
        let timeout = self.options.channel_timeout;
        loop {
            let Ok(frame) = tokio::time::timeout(timeout, stream.next()).await else {
                return End::Lost(format!(
                    "the service sent nothing for {} seconds",
                    timeout.as_secs_f64()
                ));
            };
            match frame {
                None => return End::Lost("the service closed the connection".into()),
                Some(Err(e)) => return End::Lost(net::describe(&e)),
                Some(Ok(Message::Text(text))) => self.receive(&text, backoff),
                Some(Ok(Message::Close(frame))) => return closed(frame),
                // Pings only show the channel is alive; tungstenite answers
                // them.
                Some(Ok(_)) => {}
            }
        }
    }

    fn receive(&self, text: &str, backoff: &mut Backoff) {
        match read_message(text) {
            Ok(ServiceMessage::Unknown) => {
                tracing::debug!(service = %self.service, "ignoring a message type moil doesn't know");
            }
            Ok(message) => {
                if matches!(message, ServiceMessage::Welcome(_)) {
                    backoff.reset();
                }
                self.report(LinkEvent::Message(message));
            }
            Err(Unreadable {
                assign: Some(attempt),
                reason,
            }) => self.report(LinkEvent::Unreadable { attempt, reason }),
            Err(Unreadable {
                assign: None,
                reason,
            }) => {
                tracing::warn!(service = %self.service, "ignoring a message moil can't read: {reason}");
            }
        }
    }

    fn report(&self, event: LinkEvent) {
        let _ = self.inputs.send(Input::Link {
            service: self.service.clone(),
            task: self.task,
            event,
        });
    }
}

/// Writes what the coordinator queued last (such as a final `done`), then
/// a close frame, giving a service that doesn't read [`CLOSE_TIMEOUT`].
async fn close(sink: &mut Sink, frames: &mut Frames) -> End {
    let closing = async {
        while let Ok(frame) = frames.frames.try_recv() {
            sink.feed(Message::text(frame)).await?;
        }
        let close = CloseFrame {
            code: CloseCode::Normal,
            reason: "moil stopped".into(),
        };
        sink.send(Message::Close(Some(close))).await
    };
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, closing).await;
    End::Stopped
}

/// A message the machine can't use.
#[derive(Debug, PartialEq)]
struct Unreadable {
    /// The attempt, if the message is an `assign` whose `job_id` and
    /// `attempt` could be read.
    assign: Option<AttemptRef>,
    reason: String,
}

/// Reads a message from the service, holding assignments to the limits of
/// spec §10.
fn read_message(text: &str) -> Result<ServiceMessage, Unreadable> {
    let message = serde_json::from_str(text).map_err(|e| match assign_head(text) {
        Some(attempt) => Unreadable {
            assign: Some(attempt),
            reason: format!("the machine can't read this assignment: {e}"),
        },
        None => Unreadable {
            assign: None,
            reason: e.to_string(),
        },
    })?;
    if let ServiceMessage::Assign(assign) = &message {
        let refuse = |reason: String| Unreadable {
            assign: Some(assign.attempt_ref()),
            reason,
        };
        if assign.timeout_ms == Some(0) {
            return Err(refuse("timeout_ms is 0; it must be at least 1".into()));
        }
        if depth(&assign.params) > MAX_PARAMS_DEPTH {
            return Err(refuse(format!(
                "params are nested more than {MAX_PARAMS_DEPTH} levels deep"
            )));
        }
    }
    Ok(message)
}

/// The attempt `text` assigns, if it is an `assign` whose `job_id` and
/// `attempt` can be read, whatever the rest of it holds. The fields it
/// doesn't read are skipped without being parsed, so no nesting or number
/// there can stop it.
fn assign_head(text: &str) -> Option<AttemptRef> {
    #[derive(Deserialize)]
    struct Head {
        #[serde(rename = "type")]
        kind: String,
        job_id: JobId,
        attempt: u32,
    }
    let head: Head = serde_json::from_str(text).ok()?;
    (head.kind == "assign").then_some(AttemptRef {
        job_id: head.job_id,
        attempt: head.attempt,
    })
}

/// How many arrays and objects enclose the deepest value in `value`: 0 for
/// a scalar, 1 for `[1]` or `{}`.
fn depth(value: &Value) -> usize {
    let mut deepest = 0;
    let mut pending = vec![(value, 0)];
    while let Some((value, level)) = pending.pop() {
        let inner = level + 1;
        match value {
            Value::Array(items) => pending.extend(items.iter().map(|item| (item, inner))),
            Value::Object(fields) => pending.extend(fields.values().map(|item| (item, inner))),
            _ => continue,
        }
        deepest = deepest.max(inner);
    }
    deepest
}

/// What a close frame from the service means (spec §6.4).
fn closed(frame: Option<CloseFrame>) -> End {
    let Some(frame) = frame else {
        return End::Lost("the service closed the connection".into());
    };
    match u16::from(frame.code) {
        close_code::UNAUTHORIZED => End::Unpaired,
        close_code::REPLACED => End::Replaced,
        code if frame.reason.is_empty() => {
            End::Lost(format!("the service closed the connection ({code})"))
        }
        code => End::Lost(format!(
            "the service closed the connection ({code}: {})",
            frame.reason
        )),
    }
}

/// Exponential backoff with jitter (spec §6.1): each delay is between half
/// and all of a step that doubles from `reconnect_min` to `reconnect_max`.
struct Backoff {
    min: Duration,
    max: Duration,
    step: Duration,
}

impl Backoff {
    fn new(options: &AgentOptions) -> Self {
        Self {
            min: options.reconnect_min,
            max: options.reconnect_max,
            step: options.reconnect_min,
        }
    }

    fn next(&mut self) -> Duration {
        let step = self.step;
        self.step = (step * 2).min(self.max);
        step.mul_f64(0.5 + fastrand::f64() / 2.0)
    }

    /// Starts over, once a channel has worked.
    fn reset(&mut self) {
        self.step = self.min;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_the_maximum_with_jitter_and_resets() {
        let options = AgentOptions {
            reconnect_min: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(60),
            ..AgentOptions::default()
        };
        let mut backoff = Backoff::new(&options);
        let delays: Vec<Duration> = (0..10).map(|_| backoff.next()).collect();
        let steps = [1, 2, 4, 8, 16, 32, 60, 60, 60, 60].map(Duration::from_secs);
        for (delay, step) in delays.iter().zip(steps) {
            assert!(
                *delay >= step / 2 && *delay <= step,
                "{delay:?} for a step of {step:?}"
            );
        }
        backoff.reset();
        assert!(backoff.next() <= Duration::from_secs(1));
    }

    #[test]
    fn depth_counts_the_arrays_and_objects_around_the_deepest_value() {
        let depth = |json: &str| depth(&serde_json::from_str(json).unwrap());
        assert_eq!(depth("1"), 0);
        assert_eq!(depth("null"), 0);
        assert_eq!(depth("[]"), 1);
        assert_eq!(depth("{}"), 1);
        assert_eq!(depth(r#"{"a": [1, {"b": [[]]}], "c": 2}"#), 5);
    }
}
