//! `memdusa self-update`: replace the running binary with a release archive,
//! using the same archives and SHA-256 checksums as `install.sh`.
//!
//! The pure helpers in this module (target mapping, version comparison,
//! checksum verification, archive extraction, executable replacement) are kept
//! separate from the network calls below so they can be unit tested offline.

use std::cmp::Ordering;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

const REPO: &str = "Krishnacore/memdusa";
const LATEST_URL: &str = "https://github.com/Krishnacore/memdusa/releases/latest";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const OLD_EXE: &str = "memdusa.old.exe";

/// Map `std::env::consts::{OS, ARCH}` to the five targets built by release.yml.
fn target_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

/// Split a version into numeric components and whether it is a pre-release.
/// A leading `v` and any `+build` suffix are ignored for ordering.
fn version_parts(v: &str) -> (Vec<u64>, bool) {
    let s = v.trim().trim_start_matches(['v', 'V']);
    let end = s.find(['-', '+']).unwrap_or(s.len());
    let core = &s[..end];
    let prerelease = s[end..].starts_with('-');
    let numbers = core.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect();
    (numbers, prerelease)
}

/// Semver-ish ordering: compare each numeric component, then non-pre-release
/// beats pre-release. Missing components are treated as zero (`1.2` == `1.2.0`).
fn compare_versions(a: &str, b: &str) -> Ordering {
    let (a, a_pre) = version_parts(a);
    let (b, b_pre) = version_parts(b);
    for i in 0..a.len().max(b.len()) {
        match a.get(i).unwrap_or(&0).cmp(b.get(i).unwrap_or(&0)) {
            Ordering::Equal => {}
            ord => return ord,
        }
    }
    b_pre.cmp(&a_pre)
}

/// Normalize a user-supplied version to `X.Y.Z` (without the leading `v`).
fn normalize_version(s: &str) -> Result<String> {
    let v = s.trim().trim_start_matches(['v', 'V']);
    let end = v.find(['-', '+']).unwrap_or(v.len());
    let parts: Vec<&str> = v[..end].split('.').collect();
    if parts.is_empty()
        || parts.len() > 3
        || parts.iter().any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        bail!("invalid version {s:?}; expected vX.Y.Z (e.g. v0.1.1)");
    }
    Ok(v.to_owned())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Check the archive bytes against a `.sha256` file (`<hex>  <filename>`).
fn verify_checksum(data: &[u8], checksum: &str) -> Result<()> {
    let expected = checksum.split_whitespace().next().context("empty .sha256 file")?;
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("malformed SHA-256 checksum: {expected:?}");
    }
    let actual = hex(&Sha256::digest(data));
    if !expected.eq_ignore_ascii_case(&actual) {
        bail!("SHA-256 checksum mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

fn looks_like_binary(name: &str, exe_name: &str) -> bool {
    name == exe_name || name.ends_with(&format!("/{exe_name}")) || name.ends_with(&format!("\\{exe_name}"))
}

/// Find `memdusa` in a `.tar.gz` produced by release.yml (top-level directory
/// `memdusa-vX.Y.Z-<target>/`).
fn extract_from_tar_gz(data: &[u8]) -> Result<Vec<u8>> {
    let decoder = flate2::read::GzDecoder::new(data);
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().context("reading tar.gz entries")? {
        let mut entry = entry.context("reading tar.gz entry")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().context("reading tar.gz entry path")?;
        let name = path.to_string_lossy();
        if !looks_like_binary(&name, "memdusa") {
            continue;
        }
        let mut binary = Vec::new();
        entry.read_to_end(&mut binary).context("reading memdusa from tar.gz")?;
        return Ok(binary);
    }
    bail!("archive did not contain a memdusa binary")
}

/// Find `memdusa.exe` in a `.zip` produced by release.yml.
fn extract_from_zip(data: &[u8]) -> Result<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(data)).context("opening zip archive")?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).context("reading zip entry")?;
        let name = entry.name().context("reading zip entry name")?;
        if !looks_like_binary(&name, "memdusa.exe") {
            continue;
        }
        let mut binary = Vec::new();
        entry.read_to_end(&mut binary).context("reading memdusa.exe from zip")?;
        return Ok(binary);
    }
    bail!("archive did not contain a memdusa.exe binary")
}

fn extract_binary(data: &[u8], target: &str) -> Result<Vec<u8>> {
    if target.contains("windows") { extract_from_zip(data) } else { extract_from_tar_gz(data) }
}

/// Return the package manager that owns this executable, if any.
fn package_manager_blocker(exe: &Path) -> Option<&'static str> {
    let components: Vec<String> = exe.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    if components.iter().any(|c| c == "Cellar" || c == "homebrew") {
        return Some("installed with Homebrew: run `brew upgrade memdusa`");
    }
    if components.windows(2).any(|w| w[0] == ".cargo" && w[1] == "bin") {
        return Some("installed by cargo: reinstall with `cargo install memdusa` or `cargo binstall memdusa`");
    }
    None
}

fn cleanup_old_backup(exe: &Path) {
    if cfg!(windows) {
        let old = exe.with_file_name(OLD_EXE);
        if old.exists() {
            let _ = std::fs::remove_file(old);
        }
    }
}

/// Write the new binary next to the running one, then rename it into place.
/// On Windows the running exe is first renamed to `memdusa.old.exe` (deleted
/// on the next run), because Windows will not overwrite a running executable.
fn replace_executable(current: &Path, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        bail!("the release archive contained an empty memdusa binary");
    }
    let dir = current.parent().context("running executable has no parent directory")?;
    let name = current.file_name().context("running executable has no file name")?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let tmp: PathBuf = dir.join(format!("{}.memdusa-update-{}-{nonce}", name.to_string_lossy(), std::process::id()));

    let result = (|| -> Result<()> {
        {
            let mut file = std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        }
        if cfg!(windows) {
            let old = dir.join(OLD_EXE);
            std::fs::rename(current, &old).context("moving the running exe to memdusa.old.exe")?;
            if let Err(e) = std::fs::rename(&tmp, current) {
                let _ = std::fs::rename(&old, current); // put the old binary back
                return Err(e).context("installing the new executable");
            }
        } else {
            std::fs::rename(&tmp, current).with_context(|| format!("replacing {}", current.display()))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder().user_agent(format!("memdusa/{CURRENT_VERSION}")).build().context("building HTTP client")
}

/// Follow the `/releases/latest` redirect and return the tag (`vX.Y.Z`).
/// No API token or API rate limit is involved.
async fn latest_tag(client: &reqwest::Client) -> Result<String> {
    let response = client.get(LATEST_URL).send().await.context("looking up the latest release")?;
    let status = response.status();
    let url = response.url().clone();
    if !status.is_success() {
        bail!("could not determine latest release: HTTP {status} for {LATEST_URL}");
    }
    let tag = url.path().rsplit('/').next().context("latest release URL has no tag")?;
    if !tag.starts_with('v') || tag.len() == 1 {
        bail!("could not determine the latest memdusa release version (got {tag:?})");
    }
    Ok(tag.to_owned())
}

async fn download(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let response = client.get(url).send().await.with_context(|| format!("downloading {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("download failed: HTTP {status} for {url}");
    }
    Ok(response.bytes().await.context("reading the download")?.to_vec())
}

/// Entry point for the CLI subcommand.
pub(crate) async fn run(version: Option<String>, check: bool) -> Result<()> {
    let exe = std::env::current_exe().context("finding the running executable")?;
    cleanup_old_backup(&exe);
    // `--check` is safe for any install; actually replacing a package-managed
    // binary would be wrong, so refuse before doing any network work.
    if !check && let Some(blocker) = package_manager_blocker(&exe) {
        bail!("{blocker}");
    }

    let requested = version.as_deref().map(normalize_version).transpose()?;
    let tag = match &requested {
        Some(v) => format!("v{v}"),
        None => latest_tag(&client()?).await?,
    };
    let latest = tag.trim_start_matches('v');
    let ordering = compare_versions(latest, CURRENT_VERSION);

    if check {
        if ordering == Ordering::Greater {
            println!("memdusa {latest} is available (you have {CURRENT_VERSION})");
        } else {
            println!("memdusa {CURRENT_VERSION} is up to date");
        }
        return Ok(());
    }

    // An explicit --version is honoured even when it is the same or older.
    if requested.is_none() && ordering != Ordering::Greater {
        println!("memdusa {CURRENT_VERSION} is up to date");
        return Ok(());
    }

    let target = target_for(std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| {
        anyhow!(
            "unsupported platform {}-{}; releases exist for aarch64/x86_64 linux, aarch64/x86_64 macOS and x86_64 windows",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let archive = if target.contains("windows") {
        format!("memdusa-{tag}-{target}.zip")
    } else {
        format!("memdusa-{tag}-{target}.tar.gz")
    };
    let base = format!("https://github.com/{REPO}/releases/download/{tag}");
    let http = client()?;

    println!("Downloading {archive} ...");
    let archive_bytes = download(&http, &format!("{base}/{archive}")).await?;
    let checksum = String::from_utf8(download(&http, &format!("{base}/{archive}.sha256")).await?)
        .context(".sha256 file is not valid UTF-8")?;

    println!("Verifying SHA-256 checksum ...");
    verify_checksum(&archive_bytes, &checksum).context("verifying the downloaded archive")?;

    let binary = extract_binary(&archive_bytes, target).context("extracting memdusa from the archive")?;
    replace_executable(&exe, &binary)?;
    println!("updated memdusa {CURRENT_VERSION} -> {latest}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_mapping_covers_the_five_release_targets() {
        assert_eq!(target_for("linux", "x86_64"), Some("x86_64-unknown-linux-gnu"));
        assert_eq!(target_for("linux", "aarch64"), Some("aarch64-unknown-linux-gnu"));
        assert_eq!(target_for("macos", "x86_64"), Some("x86_64-apple-darwin"));
        assert_eq!(target_for("macos", "aarch64"), Some("aarch64-apple-darwin"));
        assert_eq!(target_for("windows", "x86_64"), Some("x86_64-pc-windows-msvc"));
        assert_eq!(target_for("linux", "riscv64"), None);
        assert_eq!(target_for("windows", "aarch64"), None);
        assert_eq!(target_for("freebsd", "x86_64"), None);
    }

    #[test]
    fn version_comparison_handles_prefixes_components_and_prereleases() {
        assert_eq!(compare_versions("v0.1.2", "0.1.1"), Ordering::Greater);
        assert_eq!(compare_versions("0.1.1", "v0.1.1"), Ordering::Equal);
        assert_eq!(compare_versions("0.10.0", "0.9.9"), Ordering::Greater);
        assert_eq!(compare_versions("1.2", "1.2.0"), Ordering::Equal);
        assert_eq!(compare_versions("2.0.0-rc.1", "2.0.0"), Ordering::Less);
        assert_eq!(compare_versions(" v1.0.0 ", "1.0.0+build.7"), Ordering::Equal);
        assert_eq!(compare_versions("0.1.1", "0.2.0"), Ordering::Less);
    }

    #[test]
    fn version_normalization_accepts_tags_and_rejects_garbage() {
        assert_eq!(normalize_version("v0.1.1").unwrap(), "0.1.1");
        assert_eq!(normalize_version(" 0.1.1 ").unwrap(), "0.1.1");
        assert_eq!(normalize_version("V1.2.3-rc.1").unwrap(), "1.2.3-rc.1");
        assert!(normalize_version("latest").is_err());
        assert!(normalize_version("v1.2.3.4").is_err());
        assert!(normalize_version("v").is_err());
    }

    #[test]
    fn checksum_verification_accepts_good_and_rejects_bad() {
        let data = b"a tiny release archive";
        let hash = hex(&Sha256::digest(data));
        assert!(verify_checksum(data, &format!("{hash}  memdusa.tar.gz\n")).is_ok());
        assert!(verify_checksum(data, &hash.to_uppercase()).is_ok());
        let err = verify_checksum(b"tampered", &format!("{hash}  memdusa.tar.gz")).unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"), "{err:#}");
        assert!(verify_checksum(data, "deadbeef  memdusa.tar.gz").is_err());
        assert!(verify_checksum(data, "").is_err());
    }

    fn tar_gz_with(name: &str, contents: &[u8]) -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        builder.append_data(&mut header, name, contents).unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn zip_with(name: &str, contents: &[u8]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        writer.start_file(name, options).unwrap();
        writer.write_all(contents).unwrap();
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn extracts_binary_from_small_tar_gz_and_zip() {
        let payload = b"#!/bin/sh\necho memdusa\n";
        let tgz = tar_gz_with("memdusa-v0.1.1-x86_64-unknown-linux-gnu/memdusa", payload);
        assert_eq!(extract_binary(&tgz, "x86_64-unknown-linux-gnu").unwrap(), payload);

        let zip = zip_with("memdusa-v0.1.1-x86_64-pc-windows-msvc/memdusa.exe", payload);
        assert_eq!(extract_binary(&zip, "x86_64-pc-windows-msvc").unwrap(), payload);

        let wrong = tar_gz_with("memdusa-v0.1.1-x86_64-unknown-linux-gnu/README.md", payload);
        assert!(extract_from_tar_gz(&wrong).is_err());
        assert!(extract_from_zip(&tgz).is_err());
    }

    #[test]
    fn package_managers_are_detected_from_the_executable_path() {
        let brew = Path::new("/opt/homebrew/Cellar/memdusa/0.1.2/bin/memdusa");
        assert_eq!(package_manager_blocker(brew), Some("installed with Homebrew: run `brew upgrade memdusa`"));
        assert!(package_manager_blocker(Path::new("/usr/local/Cellar/memdusa/0.1.2/bin/memdusa")).is_some());
        let cargo = Path::new("/Users/me/.cargo/bin/memdusa");
        assert!(package_manager_blocker(cargo).is_some_and(|m| m.contains("cargo binstall")));
        assert_eq!(package_manager_blocker(Path::new("/tmp/memdusa-update-test/memdusa")), None);
    }
}
