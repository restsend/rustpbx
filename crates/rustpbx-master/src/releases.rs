//! Release directory management — stage, activate, GC.

use std::collections::HashSet;
use std::fs::Permissions;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Manifest {
    pub name: String,
    pub sha256: String,
    pub size: u64,
    pub created: String,
    #[serde(default)]
    pub worker_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub name: String,
    pub dir: PathBuf,
    pub binary: PathBuf,
    pub manifest: Manifest,
}

#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    #[error("invalid release name `{0}`")]
    BadName(String),
    #[error("release `{0}` already exists")]
    Exists(String),
    #[error("release dir `{0}` missing binary")]
    MissingBinary(String),
    #[error("release `{0}`: {1}")]
    Invalid(String, String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_alphanumeric()
        && bytes.iter().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn sync_dir(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

pub fn stage(releases_dir: &Path, name: &str, body: &[u8]) -> Result<Release, ReleaseError> {
    if !valid_name(name) { return Err(ReleaseError::BadName(name.into())); }
    if body.is_empty() { return Err(ReleaseError::Invalid(name.into(), "empty".into())); }
    let releases = releases_dir;
    std::fs::create_dir_all(releases)?;
    let target = releases.join(name);
    if target.exists() { return Err(ReleaseError::Exists(name.into())); }
    std::fs::create_dir_all(releases.join(".staging"))?;
    let tmp = releases.join(".staging").join(format!("{name}-{}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(body)?;
        f.sync_all()?;
        f.set_permissions(Permissions::from_mode(0o755))?;
    }
    std::fs::create_dir(&target)?;
    let bin = target.join("rustpbx");
    std::fs::rename(&tmp, &bin).map_err(|e| { let _ = std::fs::remove_dir_all(&target); e })?;
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(body);
    let manifest = Manifest {
        name: name.into(), sha256: format!("{:x}", hasher.finalize()),
        size: body.len() as u64, created: chrono::Utc::now().to_rfc3339(),
        worker_args: vec![],
    };
    std::fs::write(target.join("manifest.toml"), toml::to_string_pretty(&manifest).unwrap())?;
    sync_dir(&target)?;
    sync_dir(releases)?;
    let _ = std::fs::remove_dir(releases.join(".staging"));
    Ok(Release { name: name.into(), dir: target, binary: bin, manifest })
}

pub fn validate(_releases_dir: &Path, dir: &Path) -> Result<Release, ReleaseError> {
    let name = dir.file_name().and_then(|n| n.to_str())
        .ok_or_else(|| ReleaseError::Invalid(dir.display().to_string(), "bad name".into()))?;
    if !valid_name(name) { return Err(ReleaseError::BadName(name.into())); }
    let binary = dir.join("rustpbx");
    let meta = std::fs::metadata(&binary)
        .map_err(|_| ReleaseError::MissingBinary(name.into()))?;
    if meta.len() == 0 { return Err(ReleaseError::Invalid(name.into(), "empty binary".into())); }
    if meta.permissions().mode() & 0o111 == 0 {
        return Err(ReleaseError::Invalid(name.into(), "not executable".into()));
    }
    let manifest: Manifest = std::fs::read_to_string(dir.join("manifest.toml"))
        .ok().and_then(|raw| toml::from_str(&raw).ok())
        .unwrap_or(Manifest { name: name.into(), sha256: String::new(), size: meta.len(), created: String::new(), worker_args: vec![] });
    Ok(Release { name: name.into(), dir: dir.to_path_buf(), binary, manifest })
}

pub fn discover(releases_dir: &Path) -> Vec<Release> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(releases_dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() && !p.file_name().and_then(|n| n.to_str()).unwrap_or("").starts_with('.') {
                if let Ok(r) = validate(releases_dir, &p) { out.push(r); }
            }
        }
    }
    out.sort_by(|a, b| b.name.cmp(&a.name));
    out
}

pub fn activate(_releases_dir: &Path, current_link: &Path, release: &Release) -> Result<(), ReleaseError> {
    if let Some(parent) = current_link.parent() { std::fs::create_dir_all(parent)?; }
    let target = release.dir.file_name().unwrap();
    let tmp = current_link.with_extension(format!("tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp)?;
    std::fs::rename(&tmp, current_link)?;
    Ok(())
}

pub fn current_target(current_link: &Path) -> Option<PathBuf> {
    std::fs::read_link(current_link).ok()
        .map(|rel| current_link.parent().unwrap_or(Path::new(".")).join(rel))
        .and_then(|p| std::fs::canonicalize(p).ok())
}

pub fn gc(releases_dir: &Path, current_link: &Path, keep: usize, live_pids: &[u32]) -> Result<Vec<String>, ReleaseError> {
    let mut protected: HashSet<PathBuf> = current_target(current_link).into_iter().collect();
    for pid in live_pids {
        if let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) {
            if let Some(dir) = exe.parent() {
                protected.insert(std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()));
            }
        }
    }
    let releases = discover(releases_dir);
    let mut removed = Vec::new();
    let mut kept = 0usize;
    for r in releases {
        let canonical = std::fs::canonicalize(&r.dir).unwrap_or_else(|_| r.dir.clone());
        if protected.contains(&canonical) { continue; }
        if kept < keep { kept += 1; continue; }
        std::fs::remove_dir_all(&r.dir)?;
        removed.push(r.name);
    }
    Ok(removed)
}


/// [`stage`] plus per-release worker args recorded in the manifest.
pub fn stage_with_args(
    releases_dir: &Path,
    name: &str,
    body: &[u8],
    worker_args: &[String],
) -> Result<Release, ReleaseError> {
    let mut release = stage(releases_dir, name, body)?;
    if !worker_args.is_empty() {
        // The manifest is the single source of truth for worker args
        // (`SupCore::upgrade_to` reads it back).
        release.manifest.worker_args = worker_args.to_vec();
        std::fs::write(
            release.dir.join("manifest.toml"),
            toml::to_string_pretty(&release.manifest).unwrap(),
        )?;
        sync_dir(&release.dir)?;
    }
    Ok(release)
}
