// Copyright Facebook, Inc. 2019

use std::path::PathBuf;

use failure::Fallible;

use types::Key;

pub trait EdenApi: Send + Sync {
    /// Hit the API server's /health_check endpoint.
    /// Returns Ok(()) if the expected response is received, or an Error otherwise
    /// (e.g., if there was a connection problem or an unexpected repsonse).
    fn health_check(&self) -> Fallible<()>;

    /// Fetch the content of the specified files from the API server and write
    /// them to a datapack in the configured cache directory. Returns the path
    /// of the resulting packfile.
    ///
    /// Note that the keys are passed in as a `Vec` rather than using `IntoIterator`
    /// in order to keep this trait object-safe.
    fn get_files(&self, keys: Vec<Key>) -> Fallible<PathBuf>;

    /// Fetch the history of the specified files from the API server and write
    /// them to a historypack in the configured cache directory. Returns the path
    /// of the resulting packfile.
    ///
    /// Note that the keys are passed in as a `Vec` rather than using `IntoIterator`
    /// in order to keep this trait object-safe.
    fn get_history(&self, keys: Vec<Key>, max_depth: Option<u32>) -> Fallible<PathBuf>;
}
