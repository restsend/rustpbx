use super::CliTtsConfig;
use anyhow::{anyhow, Result};

pub async fn synthesize_cli(cfg: &CliTtsConfig, text: &str, output_path: &str) -> Result<()> {
    let args: Vec<String> = cfg
        .args
        .iter()
        .map(|arg| arg.replace("{text}", text).replace("{output}", output_path))
        .collect();

    let output = tokio::process::Command::new(&cfg.command)
        .args(&args)
        .output()
        .await
        .map_err(|e| anyhow!("Failed to execute TTS CLI {}: {}", cfg.command, e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("TTS CLI failed: {}", stderr));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_cli_tts() {
        let output = tempfile::NamedTempFile::with_suffix(".wav").unwrap();
        let path = output.path().to_string_lossy().to_string();

        let cfg = CliTtsConfig {
            command: "echo".to_string(),
            args: vec!["hello {text}".to_string()],
            output_format: "wav".to_string(),
        };

        // echo doesn't write to file, so this just tests arg replacement and success path
        let result = synthesize_cli(&cfg, "world", &path).await;
        assert!(result.is_ok());
    }
}
