use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio::process::Command;

fn is_executable(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.file_type().is_file()
    }
}

fn check_path_candidate(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(metadata) => metadata.file_type().is_file() && is_executable(&metadata),
        Err(_) => false,
    }
}

pub fn command_exists(command: &str) -> bool {
    if command.trim().is_empty() {
        return false;
    }

    let has_separator = command.contains('/') || command.contains('\\');
    if has_separator {
        return check_path_candidate(Path::new(command));
    }

    let path_var = match std::env::var_os("PATH") {
        Some(value) => value,
        None => return false,
    };

    for path in std::env::split_paths(&path_var) {
        if path.as_os_str().is_empty() {
            continue;
        }
        let candidate = path.join(command);
        if check_path_candidate(&candidate) {
            return true;
        }
    }

    false
}

pub fn build_sensevoice_transcribe_command(
    command: &str,
    recording_path: &str,
    models_path: Option<&str>,
    output_path: Option<&str>,
) -> Command {
    let mut cmd = Command::new(command);

    let normalized_models_path = models_path
        .map(|path| path.trim())
        .filter(|path| !path.is_empty());

    if let Some(path) = normalized_models_path {
        cmd.env("MODELS_PATH", path);
        cmd.arg("--models-path").arg(path);
    }

    if let Some(path) = output_path
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        cmd.arg("-o").arg(path);
    }

    cmd.arg("--channels=2");
    cmd.arg(recording_path);
    cmd
}

pub fn model_file_path(base_dir: &str) -> PathBuf {
    Path::new(base_dir).join("model.int8.onnx")
}
