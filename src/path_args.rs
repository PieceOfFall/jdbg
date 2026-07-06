//! Helpers for path-like CLI/MCP arguments that are interpreted by the daemon.

use std::path::{Path, PathBuf};

/// Build source roots for debugger source lookup.
///
/// The daemon and sidecar are long-lived, so relative paths from a later CLI/MCP
/// request may otherwise be resolved against an older daemon working directory.
pub fn sourcepath_or_current(raw: Option<&str>) -> Vec<String> {
    absolutize_path_list(raw)
}

/// Build classpath entries for a launched target VM.
///
/// The sidecar/`jdb` launches the target via a long-lived daemon, so relative
/// classpath entries (e.g. `.` or `out`) would otherwise resolve against the
/// daemon's working directory rather than the caller's — the class is then not
/// found and the VM exits immediately. Absolutize every entry against the CLI's
/// current directory (and default to it when none is supplied) so the launch is
/// independent of the daemon cwd.
pub fn classpath_or_current(raw: Option<&str>) -> Vec<String> {
    absolutize_path_list(raw)
}

/// Split an OS-path-separated list, absolutize each entry against the current
/// directory, and default to the current directory when the list is empty.
fn absolutize_path_list(raw: Option<&str>) -> Vec<String> {
    let paths: Vec<PathBuf> = match raw.filter(|s| !s.trim().is_empty()) {
        Some(value) => std::env::split_paths(value).collect(),
        None => vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))],
    };

    paths
        .into_iter()
        .map(|path| absolutize_source_path(&path))
        .map(protocol_path_string)
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

fn protocol_path_string(path: PathBuf) -> String {
    normalize_windows_verbatim_prefix(&path.to_string_lossy()).into_owned()
}

fn normalize_windows_verbatim_prefix(path: &str) -> std::borrow::Cow<'_, str> {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return std::borrow::Cow::Owned(format!(r"\\{rest}"));
    }
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        return std::borrow::Cow::Borrowed(rest);
    }
    std::borrow::Cow::Borrowed(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_current_dir() {
        assert_eq!(
            sourcepath_or_current(None),
            vec![protocol_path_string(
                std::env::current_dir().unwrap().canonicalize().unwrap()
            )]
        );
    }

    #[test]
    fn relative_sourcepath_is_absolutized() {
        let paths = sourcepath_or_current(Some("."));

        assert_eq!(
            paths,
            vec![protocol_path_string(
                std::env::current_dir().unwrap().canonicalize().unwrap()
            )]
        );
    }

    #[test]
    fn classpath_defaults_to_current_dir() {
        assert_eq!(
            classpath_or_current(None),
            vec![protocol_path_string(
                std::env::current_dir().unwrap().canonicalize().unwrap()
            )]
        );
    }

    #[test]
    fn relative_classpath_is_absolutized() {
        assert_eq!(
            classpath_or_current(Some(".")),
            vec![protocol_path_string(
                std::env::current_dir().unwrap().canonicalize().unwrap()
            )]
        );
    }

    #[test]
    fn windows_verbatim_prefixes_are_normalized_for_java_sidecar() {
        assert_eq!(
            normalize_windows_verbatim_prefix(r"\\?\D:\a\jdbg\jdbg\tests\fixtures\java"),
            r"D:\a\jdbg\jdbg\tests\fixtures\java"
        );
        assert_eq!(
            normalize_windows_verbatim_prefix(r"\\?\UNC\server\share\src"),
            r"\\server\share\src"
        );
        assert_eq!(
            normalize_windows_verbatim_prefix(r"C:\plain\src"),
            r"C:\plain\src"
        );
    }
}
