pub mod embeddings;
pub mod memory;
pub mod operations;
pub mod protocol;
pub mod search;

pub type Result<T> = std::result::Result<T, String>;

#[cfg(test)]
pub(crate) fn test_directory() -> std::io::Result<tempfile::TempDir> {
    tempfile::tempdir_in(std::env::temp_dir().canonicalize()?)
}
