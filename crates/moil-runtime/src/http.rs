//! Moving files over HTTP: inputs, outputs and assets (spec §3, §7.4).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::TryStreamExt;
use moil_proto::{FileInfo, Upload, UploadMethod, UrlPolicy, UrlRefused};
use reqwest::header::{CONTENT_LENGTH, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Body, Certificate, Method, redirect};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

/// How long to wait for a server to accept a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a transfer may go without receiving anything.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// How many redirects a download follows.
const MAX_REDIRECTS: usize = 10;
/// How much of an error response's body a failure message quotes.
const QUOTED_BODY_BYTES: usize = 300;

/// HTTP clients that trust the platform's roots plus any extra ones, and
/// only go where a transfer's URL policy allows.
#[derive(Debug)]
pub(crate) struct Http {
    roots: Vec<Certificate>,
    /// A client per policy, since a client's redirect policy is fixed when
    /// it's built: downloads follow redirects the policy allows.
    downloads: Mutex<HashMap<UrlPolicy, reqwest::Client>>,
    /// Uploads follow no redirects: an HTTP client turns an upload answered
    /// with `303` (or a `POST` with `301` or `302`) into a `GET`, which could
    /// succeed without uploading anything.
    uploads: reqwest::Client,
}

/// Why a transfer failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TransferError {
    /// The URL policy refused the URL, or a redirect: trying again won't
    /// help.
    #[error("{}", refused_message(.redirected, .why))]
    Refused { why: UrlRefused, redirected: bool },
    /// More arrived than the transfer allows, this many bytes.
    #[error("it is larger than the {} it may take", crate::units::bytes(*.0))]
    TooLarge(u64),
    /// Anything else, such as a server that is down.
    #[error("{0}")]
    Failed(String),
}

fn refused_message(redirected: &bool, why: &UrlRefused) -> String {
    if *redirected {
        format!("it was redirected to a URL that {why}")
    } else {
        format!("its URL {why}")
    }
}

impl TransferError {
    /// Whether another attempt could succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

impl Http {
    /// Clients trusting `extra_roots` (PEM text) on top of the platform's
    /// roots. Fails if an entry holds no valid certificate, or clients
    /// can't be built.
    pub fn new(extra_roots: &[String]) -> Result<Self, String> {
        let mut roots = Vec::new();
        for (i, pem) in extra_roots.iter().enumerate() {
            let certificates = Certificate::from_pem_bundle(pem.as_bytes())
                .map_err(|e| format!("extra root certificate {}: {}", i + 1, chain(&e)))?;
            if certificates.is_empty() {
                return Err(format!(
                    "extra root certificate {} holds no PEM certificate",
                    i + 1
                ));
            }
            roots.extend(certificates);
        }
        let uploads = client(&roots, redirect::Policy::none())?;
        Ok(Self {
            roots,
            downloads: Mutex::default(),
            uploads,
        })
    }

    /// The client for downloads under `policy`.
    fn downloads(&self, policy: UrlPolicy) -> Result<reqwest::Client, TransferError> {
        let mut clients = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(client) = clients.get(&policy) {
            return Ok(client.clone());
        }
        let redirects = redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else if let Err(why) = policy.check(attempt.url()) {
                attempt.error(why)
            } else {
                attempt.follow()
            }
        });
        let client = client(&self.roots, redirects).map_err(TransferError::Failed)?;
        clients.insert(policy, client.clone());
        Ok(client)
    }

    /// Downloads `url` with `GET` into `file`, following redirects that
    /// `policy` allows, and returns the size and SHA-256 of what arrived.
    /// Gives up as soon as it's clear that more than `max_bytes` would.
    pub async fn download(
        &self,
        policy: UrlPolicy,
        url: &str,
        headers: &BTreeMap<String, String>,
        file: &mut tokio::fs::File,
        max_bytes: u64,
    ) -> Result<FileInfo, TransferError> {
        let failed = TransferError::Failed;
        let url = check(policy, url)?;
        let request = self
            .downloads(policy)?
            .get(url)
            .headers(header_map(headers).map_err(failed)?);
        let mut response = request.send().await.map_err(sending_failed)?;
        let status = response.status();
        if !status.is_success() {
            return Err(failed(format!(
                "the server answered {status}{}",
                quote_body(response).await
            )));
        }
        if response
            .content_length()
            .is_some_and(|length| length > max_bytes)
        {
            return Err(TransferError::TooLarge(max_bytes));
        }
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        while let Some(chunk) = response.chunk().await.map_err(|e| failed(chain(&e)))? {
            size += chunk.len() as u64;
            if size > max_bytes {
                return Err(TransferError::TooLarge(max_bytes));
            }
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|e| failed(format!("couldn't write it to disk: {e}")))?;
        }
        file.flush()
            .await
            .map_err(|e| failed(format!("couldn't write it to disk: {e}")))?;
        Ok(FileInfo {
            size_bytes: size,
            sha256: hex::encode(hasher.finalize()),
        })
    }

    /// Uploads `file` to `target`, whose URL `policy` must allow,
    /// streaming it with a `Content-Length`. Any `2xx` answer is success; a
    /// redirect is not. Returns the size and SHA-256 of the bytes sent.
    pub async fn upload(
        &self,
        policy: UrlPolicy,
        target: &Upload,
        file: std::fs::File,
    ) -> Result<FileInfo, TransferError> {
        let failed = TransferError::Failed;
        let url = check(policy, &target.url)?;
        let file = tokio::fs::File::from_std(file);
        let size = file
            .metadata()
            .await
            .map_err(|e| failed(format!("couldn't read it: {e}")))?
            .len();
        let sent = Arc::new(Mutex::new((Sha256::new(), 0u64)));
        let counter = sent.clone();
        let body = ReaderStream::new(file).inspect_ok(move |chunk| {
            let mut sent = counter.lock().unwrap_or_else(|e| e.into_inner());
            sent.0.update(chunk);
            sent.1 += chunk.len() as u64;
        });
        let mut headers = header_map(&target.headers).map_err(failed)?;
        headers.insert(CONTENT_LENGTH, HeaderValue::from(size));
        let method = match target.method {
            UploadMethod::Put => Method::PUT,
            UploadMethod::Post => Method::POST,
        };
        let response = self
            .uploads
            .request(method, url)
            .headers(headers)
            .body(Body::wrap_stream(body))
            .send()
            .await
            .map_err(sending_failed)?;
        let status = response.status();
        if !status.is_success() {
            return Err(failed(format!(
                "the server answered {status}{}",
                quote_body(response).await
            )));
        }
        let (hasher, sent_bytes) =
            std::mem::take(&mut *sent.lock().unwrap_or_else(|e| e.into_inner()));
        if sent_bytes != size {
            return Err(failed(
                "the file changed while it was being uploaded".into(),
            ));
        }
        Ok(FileInfo {
            size_bytes: size,
            sha256: hex::encode(hasher.finalize()),
        })
    }
}

/// A client trusting `roots` on top of the platform's, with `redirects`.
fn client(roots: &[Certificate], redirects: redirect::Policy) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("moil/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .redirect(redirects)
        .tls_certs_merge(roots.iter().cloned())
        .build()
        .map_err(|e| chain(&e))
}

/// Parses `url` and fails unless `policy` allows it.
fn check(policy: UrlPolicy, url: &str) -> Result<reqwest::Url, TransferError> {
    policy.check_str(url).map_err(|why| TransferError::Refused {
        why,
        redirected: false,
    })
}

/// Why sending a request failed: a redirect the policy refused, or
/// whatever else went wrong.
fn sending_failed(error: reqwest::Error) -> TransferError {
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        if let Some(why) = cause.downcast_ref::<UrlRefused>() {
            return TransferError::Refused {
                why: why.clone(),
                redirected: true,
            };
        }
        source = cause.source();
    }
    TransferError::Failed(chain(&error))
}

fn header_map(headers: &BTreeMap<String, String>) -> Result<HeaderMap, String> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let name =
            HeaderName::try_from(name).map_err(|_| format!("header name {name:?} is invalid"))?;
        let value = HeaderValue::try_from(value)
            .map_err(|_| format!("the value of header {name} is invalid"))?;
        map.insert(name, value);
    }
    Ok(map)
}

/// The start of an error response's body, as `": …"`, or nothing.
async fn quote_body(mut response: reqwest::Response) -> String {
    let mut body = Vec::new();
    while body.len() < QUOTED_BODY_BYTES {
        match response.chunk().await {
            Ok(Some(chunk)) => body.extend_from_slice(&chunk),
            _ => break,
        }
    }
    let text = String::from_utf8_lossy(&body);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        String::new()
    } else {
        format!(": {}", crate::stdout::truncate(&text, QUOTED_BODY_BYTES))
    }
}

/// An error and its causes, as one line. Leaves out URLs, which may hold
/// credentials.
fn chain(error: &reqwest::Error) -> String {
    let mut message = if error.is_timeout() {
        "timed out".to_owned()
    } else if error.is_connect() {
        "couldn't connect".to_owned()
    } else {
        String::new()
    };
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let text = cause.to_string();
        // hyper's "client error (Connect)" only wraps the real cause.
        let generic = text.starts_with("client error (");
        if !text.is_empty() && !generic && !message.contains(&text) {
            if !message.is_empty() {
                message.push_str(": ");
            }
            message.push_str(&text);
        }
        source = cause.source();
    }
    if message.is_empty() {
        message = error.to_string();
    }
    message
}
