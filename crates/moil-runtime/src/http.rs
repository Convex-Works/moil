//! Moving files over HTTP: inputs, outputs and assets (spec §3, §7.4).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::TryStreamExt;
use moil_proto::{FileInfo, Upload, UploadMethod};
use reqwest::header::{CONTENT_LENGTH, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Body, Method, redirect};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;
use url::{Host, Url};

/// How long to wait for a server to accept a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a transfer may go without receiving anything.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// How much of an error response's body a failure message quotes.
const QUOTED_BODY_BYTES: usize = 300;

/// An HTTP client that trusts the platform's roots plus any extra ones, and
/// only goes where the URL policy allows, redirects included.
#[derive(Debug, Clone)]
pub(crate) struct Http {
    client: reqwest::Client,
    allow_insecure_http: bool,
}

/// Why a download failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DownloadError {
    #[error("{0}")]
    Failed(String),
    #[error("it is larger than the expected {0} bytes")]
    TooLarge(u64),
}

impl Http {
    /// A client trusting `extra_roots` (PEM text) on top of the platform's
    /// roots. Fails if an entry holds no valid certificate.
    pub fn new(extra_roots: &[String], allow_insecure_http: bool) -> Result<Self, String> {
        let mut roots = Vec::new();
        for (i, pem) in extra_roots.iter().enumerate() {
            let certificates = reqwest::Certificate::from_pem_bundle(pem.as_bytes())
                .map_err(|e| format!("extra root certificate {}: {}", i + 1, chain(&e)))?;
            if certificates.is_empty() {
                return Err(format!(
                    "extra root certificate {} holds no PEM certificate",
                    i + 1
                ));
            }
            roots.extend(certificates);
        }
        let redirects = redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 10 {
                attempt.error("too many redirects")
            } else if let Err(why) = check_url(attempt.url().as_str(), allow_insecure_http) {
                attempt.error(format!("redirected to a URL that {why}"))
            } else {
                attempt.follow()
            }
        });
        let client = reqwest::Client::builder()
            .user_agent(concat!("moil/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .redirect(redirects)
            .tls_certs_merge(roots)
            .build()
            .map_err(|e| chain(&e))?;
        Ok(Self {
            client,
            allow_insecure_http,
        })
    }

    /// Fails unless the policy allows `url` (see [`check_url`]).
    pub fn check(&self, url: &str) -> Result<Url, String> {
        check_url(url, self.allow_insecure_http)
    }

    /// Downloads `url` with `GET` into `file`, returning the size and
    /// SHA-256 of what arrived. Gives up as soon as more than `max_bytes`
    /// arrive.
    pub async fn download(
        &self,
        url: &str,
        headers: &BTreeMap<String, String>,
        file: &mut tokio::fs::File,
        max_bytes: Option<u64>,
    ) -> Result<FileInfo, DownloadError> {
        let failed = DownloadError::Failed;
        let url = self
            .check(url)
            .map_err(|why| failed(format!("its URL {why}")))?;
        let request = self
            .client
            .get(url)
            .headers(header_map(headers).map_err(failed)?);
        let mut response = request.send().await.map_err(|e| failed(chain(&e)))?;
        let status = response.status();
        if !status.is_success() {
            return Err(failed(format!(
                "the server answered {status}{}",
                quote_body(response).await
            )));
        }
        if let (Some(max), Some(length)) = (max_bytes, response.content_length())
            && length > max
        {
            return Err(DownloadError::TooLarge(max));
        }
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        while let Some(chunk) = response.chunk().await.map_err(|e| failed(chain(&e)))? {
            size += chunk.len() as u64;
            if let Some(max) = max_bytes
                && size > max
            {
                return Err(DownloadError::TooLarge(max));
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

    /// Uploads the file at `path` to `target`, streaming it with a
    /// `Content-Length`. Any `2xx` answer is success. Returns the size and
    /// SHA-256 of the bytes sent.
    pub async fn upload(&self, target: &Upload, path: &Path) -> Result<FileInfo, String> {
        let url = self
            .check(&target.url)
            .map_err(|why| format!("its URL {why}"))?;
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|e| format!("couldn't open it: {e}"))?;
        let size = file
            .metadata()
            .await
            .map_err(|e| format!("couldn't read it: {e}"))?
            .len();
        let sent = Arc::new(Mutex::new((Sha256::new(), 0u64)));
        let counter = sent.clone();
        let body = ReaderStream::new(file).inspect_ok(move |chunk| {
            let mut sent = counter.lock().unwrap_or_else(|e| e.into_inner());
            sent.0.update(chunk);
            sent.1 += chunk.len() as u64;
        });
        let mut headers = header_map(&target.headers)?;
        headers.insert(CONTENT_LENGTH, HeaderValue::from(size));
        let method = match target.method {
            UploadMethod::Put => Method::PUT,
            UploadMethod::Post => Method::POST,
        };
        let response = self
            .client
            .request(method, url)
            .headers(headers)
            .body(Body::wrap_stream(body))
            .send()
            .await
            .map_err(|e| chain(&e))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!(
                "the server answered {status}{}",
                quote_body(response).await
            ));
        }
        let (hasher, sent_bytes) =
            std::mem::take(&mut *sent.lock().unwrap_or_else(|e| e.into_inner()));
        if sent_bytes != size {
            return Err("the file changed while it was being uploaded".into());
        }
        Ok(FileInfo {
            size_bytes: size,
            sha256: hex::encode(hasher.finalize()),
        })
    }
}

/// Fails unless `url` may be used to move a job's files (spec §3): `https`,
/// or `http` to a loopback host, or `http` anywhere if `allow_insecure_http`.
/// The error completes the sentence "its URL …".
pub(crate) fn check_url(url: &str, allow_insecure_http: bool) -> Result<Url, String> {
    let parsed = Url::parse(url).map_err(|e| format!("isn't valid: {e}"))?;
    match parsed.scheme() {
        "https" if parsed.has_host() => Ok(parsed),
        "http" if allow_insecure_http || is_loopback(&parsed) => Ok(parsed),
        "http" => Err(format!(
            "uses http to {}, which isn't this machine; only https is allowed",
            parsed.host_str().unwrap_or("no host")
        )),
        scheme => Err(format!("uses {scheme}; only https is allowed")),
    }
}

/// `localhost`, `127.0.0.0/8` or `::1`.
fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(domain)) => {
            let domain = domain.strip_suffix('.').unwrap_or(domain);
            domain.eq_ignore_ascii_case("localhost")
        }
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_policy_allows_https_and_loopback_http_only() {
        for allowed in [
            "https://s3.example.com/bucket/key?X-Amz-Signature=abc",
            "http://localhost:8080/in/a",
            "http://LOCALHOST./a",
            "http://127.0.0.1/a",
            "http://127.200.3.4:9/a",
            "http://[::1]:9/a",
        ] {
            assert!(
                check_url(allowed, false).is_ok(),
                "{allowed} should be allowed"
            );
        }
        for refused in [
            "http://example.com/a",
            "http://0.0.0.0:9/a",
            "http://10.0.0.1/a",
            "http://localhost.example.com/a",
            "http://[::ffff:127.0.0.1]/a",
            "ftp://127.0.0.1/a",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(
                check_url(refused, false).is_err(),
                "{refused} should be refused"
            );
        }
        assert!(check_url("http://example.com/a", true).is_ok());
        assert!(check_url("file:///etc/passwd", true).is_err());
    }
}
