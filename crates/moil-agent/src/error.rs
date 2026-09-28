use std::path::PathBuf;

/// Everything that can go wrong on the machine side, phrased for the owner.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("moil is already running for {0}; quit it first")]
    AlreadyRunning(PathBuf),
    #[error("no service matches {0:?}")]
    UnknownService(String),
    /// A service the home knows, which this machine isn't paired with
    /// (spec/machine.md §1.2).
    #[error("this machine isn't paired with {0}; pair with it first")]
    NotPaired(crate::ServiceId),
    #[error("{query:?} matches more than one service: {matches}")]
    AmbiguousService { query: String, matches: String },
    /// An address that can't be a service's, as a sentence for the owner.
    #[error("{0}")]
    InvalidAddress(String),
    #[error("{url} isn't a moil service: {reason}")]
    NotAService { url: String, reason: String },
    /// A service that asked the owner to confirm pairing on another site's
    /// page, as one relaying another service's pairing would (spec §4).
    #[error(
        "{url} wants you to confirm on {site}, another site, which is how a service would get you to pair its own machine with your account there; to pair with {site}, use the address it shows for moil"
    )]
    ForeignPairingPage { url: String, site: String },
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
    /// A file the owner may edit, such as `settings.json`, that isn't
    /// valid (spec/machine.md §1).
    #[error("{} isn't valid ({reason}); fix it or move it out of the way", path.display())]
    Invalid { path: PathBuf, reason: String },
    /// A setting from the environment that moil can't use.
    #[error("{0}")]
    Config(String),
}
