//! A loopback HTTP server standing in for a service's object storage, so
//! `moil run` moves inputs and outputs the way a real job does: the runtime
//! downloads inputs from it and uploads outputs to it.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use futures::StreamExt;
use moil_proto::{Download, FileName, Upload, UploadMethod};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

/// Serves input files at `/in/<name>` and writes outputs uploaded to
/// `/out/<name>` into a directory. Stops when dropped.
#[derive(Debug)]
pub struct Storage {
    addr: SocketAddr,
    server: tokio::task::JoinHandle<()>,
}

#[derive(Debug, Clone)]
struct Files {
    inputs: Arc<BTreeMap<FileName, PathBuf>>,
    outputs: Arc<BTreeSet<FileName>>,
    out_dir: Arc<PathBuf>,
}

impl Storage {
    /// Starts serving `inputs` and accepting `outputs` into `out_dir`.
    pub async fn start(
        inputs: BTreeMap<FileName, PathBuf>,
        outputs: BTreeSet<FileName>,
        out_dir: PathBuf,
    ) -> io::Result<Self> {
        let files = Files {
            inputs: Arc::new(inputs),
            outputs: Arc::new(outputs),
            out_dir: Arc::new(out_dir),
        };
        let app = Router::new()
            .route("/in/{name}", get(serve_input))
            .route("/out/{name}", put(receive_output))
            .with_state(files);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                eprintln!("moil: the local file server stopped: {error}");
            }
        });
        Ok(Self { addr, server })
    }

    pub fn download(&self, name: &FileName) -> Download {
        Download {
            url: format!("http://{}/in/{name}", self.addr),
            headers: BTreeMap::new(),
        }
    }

    pub fn upload(&self, name: &FileName) -> Upload {
        Upload {
            url: format!("http://{}/out/{name}", self.addr),
            method: UploadMethod::Put,
            headers: BTreeMap::new(),
        }
    }
}

impl Drop for Storage {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn serve_input(State(files): State<Files>, UrlPath(name): UrlPath<String>) -> Response {
    let Some(path) = FileName::new(name)
        .ok()
        .and_then(|name| files.inputs.get(&name))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::fs::File::open(path).await {
        Ok(file) => Body::from_stream(ReaderStream::new(file)).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

async fn receive_output(
    State(files): State<Files>,
    UrlPath(name): UrlPath<String>,
    body: Body,
) -> Response {
    let Some(name) = FileName::new(name)
        .ok()
        .filter(|name| files.outputs.contains(name))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match write_output(&files.out_dir, &name, body).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

/// Writes `body` to `<dir>/<name>`, replacing the file only once all of it
/// has arrived.
async fn write_output(dir: &Path, name: &FileName, body: Body) -> io::Result<()> {
    let partial = dir.join(format!(".{name}.partial"));
    let mut file = tokio::fs::File::create(&partial).await?;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        file.write_all(&chunk.map_err(io::Error::other)?).await?;
    }
    file.flush().await?;
    tokio::fs::rename(&partial, dir.join(name.as_str())).await
}
