//! What the owner looks at before approving a bundle.

use moil_bundle::{Bundle, FileDiff};

use crate::PairedService;

/// A bundle fetched from a service and verified against the hash the owner
/// is asked to approve, with the version it would replace.
#[derive(Debug, Clone)]
pub struct Review {
    pub service: PairedService,
    pub bundle: Bundle,
    /// The most recently approved bundle with the same name from the same
    /// service, if any.
    pub previous: Option<Bundle>,
    /// Whether the owner already approved this exact bundle.
    pub approved: bool,
}

impl Review {
    /// How each file changed since `previous`.
    pub fn diff(&self) -> Option<Vec<FileDiff>> {
        self.previous
            .as_ref()
            .map(|previous| moil_bundle::diff(previous, &self.bundle))
    }
}
