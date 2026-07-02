//! Helpers for path-like CLI/MCP arguments that are interpreted by the daemon.

use std::path::{Path, PathBuf};

/// Build source roots for debugger source lookup.
///
/// The daemon and sidecar are long-lived, so relative paths from a later CLI/MCP
/// request may otherwise be resolved against an older daemon working directory.
pub fn sourcepath_or_current(raw: Option<&str>) -> Vec<String> {
    let paths: Vec<PathBuf> = match raw.filter(|s| !s.trim().is_empty()) {
        Some(value) => std::env::split_paths(value).collect(),
        None => vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))],
    };

    paths
        .into_iter()
        .map(|path| absolutize_source_path(&path))
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

fn absolutize_source_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    absolute.canonicalize().unwrap_or(absolute)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_current_dir() {
        assert_eq!(
            sourcepath_or_current(None),
            vec![
                std::env::current_dir()
                    .unwrap()
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            ]
        );
    }

    #[test]
    fn relative_sourcepath_is_absolutized() {
        let paths = sourcepath_or_current(Some("."));

        assert_eq!(
            paths,
            vec![
                std::env::current_dir()
                    .unwrap()
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            ]
        );
    }
}
