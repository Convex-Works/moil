//! One service's control channel: opens it, relays messages both ways, and
//! reopens it with backoff until told to stop (spec §6).
//!
//! The task only moves frames. What to say is the coordinator's business:
//! it gets every message the service sends, and sends through the outbox
//! it's handed when the channel opens.

use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use moil_proto::{ServiceMessage, close_code};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
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
use crate::{Error, Machine, ServiceId, net};

/// The largest control message either side may send (spec §10).
const MAX_MESSAGE_BYTES: usize = 1 << 20;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// What a connection task tells the coordinator.
#[derive(Debug)]
pub(super) enum LinkEvent {
    /// Opening the channel, for the first time or again.
    Connecting,
    /// The channel is open. The coordinator speaks first (`hello`), and
    /// every frame it sends goes through `outbox`.
    Open(mpsc::UnboundedSender<String>),
    Message(ServiceMessage),
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

/// Starts the task that keeps `service`'s channel open until `stop` is
/// cancelled. Every event it reports carries `task`, so the coordinator
/// can tell it from the tasks it replaced.
pub(super) fn spawn(
    machine: Machine,
    service: ServiceId,
    task: u64,
    options: AgentOptions,
    inputs: mpsc::UnboundedSender<Input>,
    stop: CancellationToken,
) -> JoinHandle<()> {
    let channel = Channel {
        machine,
        service,
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
        let service = match self.machine.service(&self.service) {
            Ok(service) => service,
            Err(Error::UnknownService(_)) => return Err(End::Unpaired),
            Err(e) => return Err(End::Lost(e.to_string())),
        };
        let lost = |e: Error| End::Lost(e.to_string());
        self.machine.check_url(&service.base_url).map_err(lost)?;
        let token = match self.machine.token(&self.service) {
            Ok(Some(token)) => token,
            Ok(None) => return Err(End::Unpaired),
            Err(e) => return Err(lost(e)),
        };

        let mut url = Url::parse(&net::endpoint(&service.base_url, "v1/connect"))
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
            connected = tokio::time::timeout(self.options.receive_timeout, connecting) => connected,
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
                service.name,
                response.status()
            ))),
            Ok(Err(e)) => Err(End::Lost(format!(
                "couldn't reach {}: {}",
                service.base_url,
                net::describe(&e)
            ))),
            Err(_) => Err(End::Lost(format!("{} didn't answer", service.base_url))),
        }
    }

    /// Relays frames until the channel ends.
    async fn serve(&self, socket: Socket, backoff: &mut Backoff) -> End {
        let (mut sink, mut stream) = socket.split();
        let (outbox, mut frames) = mpsc::unbounded_channel::<String>();
        self.report(LinkEvent::Open(outbox));
        let silence = self.options.receive_timeout;
        let mut dead_at = tokio::time::Instant::now() + silence;
        loop {
            tokio::select! {
                () = self.stop.cancelled() => {
                    // Whatever the coordinator queued last (such as a final
                    // `done`) goes out before the close frame.
                    while let Ok(text) = frames.try_recv() {
                        if sink.send(Message::text(text)).await.is_err() {
                            return End::Stopped;
                        }
                    }
                    let close = CloseFrame { code: CloseCode::Normal, reason: "moil stopped".into() };
                    let _ = sink.send(Message::Close(Some(close))).await;
                    return End::Stopped;
                }
                () = tokio::time::sleep_until(dead_at) => {
                    return End::Lost(format!(
                        "the service sent nothing for {} seconds",
                        silence.as_secs_f64()
                    ));
                }
                frame = stream.next() => {
                    dead_at = tokio::time::Instant::now() + silence;
                    match frame {
                        None => return End::Lost("the service closed the connection".into()),
                        Some(Err(e)) => return End::Lost(net::describe(&e)),
                        Some(Ok(Message::Text(text))) => self.receive(&text, backoff),
                        Some(Ok(Message::Close(frame))) => return closed(frame),
                        // Pings only show the channel is alive; tungstenite
                        // answers them.
                        Some(Ok(_)) => {}
                    }
                }
                Some(text) = frames.recv() => {
                    if let Err(e) = sink.send(Message::text(text)).await {
                        return End::Lost(net::describe(&e));
                    }
                }
            }
        }
    }

    fn receive(&self, text: &str, backoff: &mut Backoff) {
        match serde_json::from_str::<ServiceMessage>(text) {
            Ok(ServiceMessage::Unknown) => {
                tracing::debug!(service = %self.service, "ignoring a message type moil doesn't know");
            }
            Ok(message) => {
                if matches!(message, ServiceMessage::Welcome(_)) {
                    backoff.reset();
                }
                self.report(LinkEvent::Message(message));
            }
            Err(e) => {
                tracing::warn!(service = %self.service, "ignoring a message moil can't read: {e}");
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
}
