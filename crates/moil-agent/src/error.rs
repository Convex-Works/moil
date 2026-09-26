use std::path::PathBuf;

/// Everything that can go wrong on the machine side, phrased for the owner.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("moil is already running for {0}; quit it first")]
    AlreadyRunning(PathBuf),
    #[error("no paired service matches {0:?}")]
    UnknownService(String),
    #[error("{query:?} matches more than one service: {matches}")]
    AmbiguousService { query: String, matches: String },
    #[error("{url} isn't a moil service: {reason}")]
    NotAService { url: String, reason: String },
    #[error("{0} must use https; only loopback addresses may use http")]
    InsecureUrl(String),
    #[error("the pairing was rejected on the service")]
    PairingDenied,
    #[error("the pairing code expired before it was confirmed")]
    PairingExpired,
    #[error("the service no longer accepts this machine; pair it again")]
    Unauthorized,
    #[error("the service has no bundle {0}")]
    UnknownBundle(moil_proto::BundleHash),
    #[error(transparent)]
    Bundle(#[from] moil_bundle::BundleError),
    #[error(transparent)]
    BundleLoad(#[from] moil_bundle::LoadError),
    #[error("{0}")]
    Http(String),
    #[error("couldn't access the keychain: {0}")]
    Secrets(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A state file that can't be read back, e.g. after someone edited it.
    #[error("{} is damaged ({reason}); repair or remove it", path.display())]
    Damaged { path: PathBuf, reason: String },
    /// A setting from the environment that moil can't use.
    #[error("{0}")]
    Config(String),
}
