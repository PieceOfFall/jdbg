//! Install the JDI sidecar jar during update/setup.
//!
//! cargo-dist's shell/powershell installers install only executable assets from
//! the release archive. The sidecar jar is intentionally pulled from that same
//! official archive here, rather than discovered from arbitrary local builds.

use std::fs;
#[cfg(not(windows))]
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, bail};

use crate::jdi::lifecycle::SIDECAR_JAR_NAME;

const RELEASE_BASE_URL: &str = "https://github.com/PieceOfFall/jdbg/releases/latest/download";

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Result<Self> {
        let path = std::env::temp_dir().join(format!("jdbg-update-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path)
            .with_context(|| format!("failed to create temp dir {}", path.display()))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    TarXz,
    Zip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReleaseArchive {
    name: &'static str,
    kind: ArchiveKind,
}

pub fn install_from_latest_release_next_to(exe_path: &Path) -> Result<PathBuf> {
    let install_dir = exe_path
        .parent()
        .with_context(|| format!("cannot determine install dir from {}", exe_path.display()))?;
    let archive = release_archive()?;
    let temp = TempDir::new("sidecar")?;
    let archive_path = temp.path().join(archive.name);
    let extract_dir = temp.path().join("extract");
    let checksum_path = temp.path().join(format!("{}.sha256", archive.name));
    fs::create_dir_all(&extract_dir)
        .with_context(|| format!("failed to create {}", extract_dir.display()))?;

    let url = release_archive_url(archive.name);
    download_archive(&url, &archive_path)?;
    download_archive(&format!("{url}.sha256"), &checksum_path)?;
    let expected_checksum = read_expected_checksum(&checksum_path)?;
    verify_archive_checksum(&archive_path, &expected_checksum)?;
    extract_archive(archive, &archive_path, &extract_dir)?;
    let extracted = find_sidecar_in_dir(&extract_dir)?.with_context(|| {
        format!(
            "official release archive {} did not contain {SIDECAR_JAR_NAME}",
            archive.name
        )
    })?;

    install_sidecar_file(&extracted, install_dir)
}

fn release_archive_url(archive_name: &str) -> String {
    format!("{RELEASE_BASE_URL}/{archive_name}")
}

fn release_archive() -> Result<ReleaseArchive> {
    let archive = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        ReleaseArchive {
            name: "java-agent-debugger-aarch64-apple-darwin.tar.xz",
            kind: ArchiveKind::TarXz,
        }
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        ReleaseArchive {
            name: "java-agent-debugger-x86_64-apple-darwin.tar.xz",
            kind: ArchiveKind::TarXz,
        }
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        ReleaseArchive {
            name: "java-agent-debugger-aarch64-unknown-linux-gnu.tar.xz",
            kind: ArchiveKind::TarXz,
        }
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        ReleaseArchive {
            name: "java-agent-debugger-x86_64-unknown-linux-gnu.tar.xz",
            kind: ArchiveKind::TarXz,
        }
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        ReleaseArchive {
            name: "java-agent-debugger-x86_64-pc-windows-msvc.zip",
            kind: ArchiveKind::Zip,
        }
    } else {
        bail!(
            "no official jdbg release archive is configured for this platform ({}-{})",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
    };
    Ok(archive)
}

#[cfg(windows)]
fn download_archive(url: &str, archive_path: &Path) -> Result<()> {
    let status = ProcessCommand::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            "$ProgressPreference = 'SilentlyContinue'; Invoke-WebRequest -Uri $env:JDBG_ARCHIVE_URL -OutFile $env:JDBG_ARCHIVE_PATH",
        ])
        .env("JDBG_ARCHIVE_URL", url)
        .env("JDBG_ARCHIVE_PATH", archive_path)
        .status()
        .context("failed to start powershell to download sidecar archive")?;
    if !status.success() {
        bail!("failed to download {url} with status {status}");
    }
    Ok(())
}

#[cfg(not(windows))]
fn download_archive(url: &str, archive_path: &Path) -> Result<()> {
    let status = ProcessCommand::new("curl")
        .args(["--proto", "=https", "--tlsv1.2", "-LsSf", "-o"])
        .arg(archive_path)
        .arg(url)
        .status()
        .context("failed to start curl to download sidecar archive")?;
    if !status.success() {
        bail!("failed to download {url} with status {status}");
    }
    Ok(())
}

fn read_expected_checksum(checksum_path: &Path) -> Result<String> {
    let content = fs::read_to_string(checksum_path)
        .with_context(|| format!("failed to read {}", checksum_path.display()))?;
    parse_checksum_token(&content)
        .with_context(|| format!("invalid sha256 file {}", checksum_path.display()))
}

fn parse_checksum_token(content: &str) -> Result<String> {
    let checksum = content
        .split_whitespace()
        .next()
        .context("missing checksum")?
        .to_ascii_lowercase();
    if checksum.len() != 64 || !checksum.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("expected a 64-character hex sha256 checksum");
    }
    Ok(checksum)
}

fn verify_archive_checksum(archive_path: &Path, expected: &str) -> Result<()> {
    let actual = compute_sha256(archive_path)?;
    if actual != expected {
        bail!(
            "checksum mismatch for {}: expected {expected}, got {actual}",
            archive_path.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn compute_sha256(path: &Path) -> Result<String> {
    let output = ProcessCommand::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            "(Get-FileHash -Algorithm SHA256 -LiteralPath $env:JDBG_ARCHIVE_PATH).Hash",
        ])
        .env("JDBG_ARCHIVE_PATH", path)
        .output()
        .context("failed to start powershell to verify sidecar archive checksum")?;
    if !output.status.success() {
        bail!("failed to verify checksum for {}", path.display());
    }
    parse_checksum_token(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(not(windows))]
fn compute_sha256(path: &Path) -> Result<String> {
    if let Some(checksum) = checksum_with_unix_command("sha256sum", &[], path)? {
        return Ok(checksum);
    }
    if let Some(checksum) = checksum_with_unix_command("shasum", &["-a", "256"], path)? {
        return Ok(checksum);
    }
    bail!(
        "neither sha256sum nor shasum was found to verify {}",
        path.display()
    )
}

#[cfg(not(windows))]
fn checksum_with_unix_command(program: &str, args: &[&str], path: &Path) -> Result<Option<String>> {
    let output = match ProcessCommand::new(program).args(args).arg(path).output() {
        Ok(output) => output,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to start {program} to verify checksum"));
        }
    };
    if !output.status.success() {
        bail!(
            "{program} failed to verify checksum for {} with status {}",
            path.display(),
            output.status
        );
    }
    Ok(Some(parse_checksum_token(&String::from_utf8_lossy(
        &output.stdout,
    ))?))
}

fn extract_archive(archive: ReleaseArchive, archive_path: &Path, extract_dir: &Path) -> Result<()> {
    match archive.kind {
        ArchiveKind::TarXz => extract_tar_xz(archive_path, extract_dir),
        ArchiveKind::Zip => extract_zip(archive_path, extract_dir),
    }
}

fn extract_tar_xz(archive_path: &Path, extract_dir: &Path) -> Result<()> {
    let status = ProcessCommand::new("tar")
        .arg("xf")
        .arg(archive_path)
        .arg("-C")
        .arg(extract_dir)
        .status()
        .context("failed to start tar to extract sidecar archive")?;
    if !status.success() {
        bail!(
            "failed to extract {} with status {status}",
            archive_path.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn extract_zip(archive_path: &Path, extract_dir: &Path) -> Result<()> {
    let status = ProcessCommand::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            "Expand-Archive -LiteralPath $env:JDBG_ARCHIVE_PATH -DestinationPath $env:JDBG_EXTRACT_DIR -Force",
        ])
        .env("JDBG_ARCHIVE_PATH", archive_path)
        .env("JDBG_EXTRACT_DIR", extract_dir)
        .status()
        .context("failed to start powershell to extract sidecar archive")?;
    if !status.success() {
        bail!(
            "failed to extract {} with status {status}",
            archive_path.display()
        );
    }
    Ok(())
}

#[cfg(not(windows))]
fn extract_zip(archive_path: &Path, _extract_dir: &Path) -> Result<()> {
    bail!(
        "cannot extract Windows sidecar archive {} on this platform",
        archive_path.display()
    )
}

fn find_sidecar_in_dir(dir: &Path) -> Result<Option<PathBuf>> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current)
            .with_context(|| format!("failed to read {}", current.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|name| name.to_str()) == Some(SIDECAR_JAR_NAME) {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

fn install_sidecar_file(src: &Path, install_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(install_dir)
        .with_context(|| format!("failed to create install dir {}", install_dir.display()))?;
    let dest = install_dir.join(SIDECAR_JAR_NAME);
    let temp = install_dir.join(format!("{SIDECAR_JAR_NAME}.tmp.{}", std::process::id()));
    let _ = fs::remove_file(&temp);
    fs::copy(src, &temp).with_context(|| {
        format!(
            "failed to copy sidecar jar from {} to {}",
            src.display(),
            temp.display()
        )
    })?;
    if dest.exists() {
        fs::remove_file(&dest)
            .with_context(|| format!("failed to replace existing {}", dest.display()))?;
    }
    fs::rename(&temp, &dest).with_context(|| {
        format!(
            "failed to move sidecar jar from {} to {}",
            temp.display(),
            dest.display()
        )
    })?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_url_uses_latest_release_download() {
        assert_eq!(
            release_archive_url("java-agent-debugger-aarch64-apple-darwin.tar.xz"),
            "https://github.com/PieceOfFall/jdbg/releases/latest/download/java-agent-debugger-aarch64-apple-darwin.tar.xz"
        );
    }

    #[test]
    fn parses_dist_sha256_file() {
        assert_eq!(
            parse_checksum_token("E3DF562E8BE76042CCF0B4AEE DUMMY\n")
                .unwrap_err()
                .to_string(),
            "expected a 64-character hex sha256 checksum"
        );
        assert_eq!(
            parse_checksum_token(
                "E3DF562E8BE76042CCF0B4AEE D25A10DF1B5AB4C21735139F2E6D3EB15391700 *archive.tar.xz\n"
            )
            .unwrap_err()
            .to_string(),
            "expected a 64-character hex sha256 checksum"
        );
        assert_eq!(
            parse_checksum_token(
                "E3DF562E8BE76042CCF0B4AEED25A10DF1B5AB4C21735139F2E6D3EB15391700 *archive.tar.xz\n"
            )
            .unwrap(),
            "e3df562e8be76042ccf0b4aeed25a10df1b5ab4c21735139f2e6d3eb15391700"
        );
    }

    #[test]
    fn finds_sidecar_only_inside_given_dir() {
        let temp = TempDir::new("find-sidecar-test").unwrap();
        let nested = temp.path().join("pkg").join("nested");
        fs::create_dir_all(&nested).unwrap();
        let jar = nested.join(SIDECAR_JAR_NAME);
        fs::write(&jar, b"jar").unwrap();

        assert_eq!(find_sidecar_in_dir(temp.path()).unwrap(), Some(jar));
    }

    #[test]
    fn installs_sidecar_next_to_exe() {
        let temp = TempDir::new("install-sidecar-test").unwrap();
        let src = temp.path().join("source.jar");
        let install_dir = temp.path().join("bin");
        fs::write(&src, b"jar").unwrap();

        let installed = install_sidecar_file(&src, &install_dir).unwrap();
        assert_eq!(installed, install_dir.join(SIDECAR_JAR_NAME));
        assert_eq!(fs::read(installed).unwrap(), b"jar");
    }
}
