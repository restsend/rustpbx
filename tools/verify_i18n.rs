//! Tool to verify TOML locale files have no duplicate keys

use std::collections::HashSet;
use std::env;
use std::fs;

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: {} <toml-file> [toml-file2 ...]", args[0]);
        std::process::exit(1);
    }

    let mut has_errors = false;

    for file_path in &args[1..] {
        if let Err(e) = verify_toml(file_path) {
            eprintln!("Error: {}", e);
            has_errors = true;
        }
    }

    if has_errors {
        std::process::exit(1);
    }

    println!("All TOML files validated successfully!");
}

fn verify_toml(path: &str) -> Result<(), String> {
    let content =
        fs::read_to_string(path).map_err(|e| format!("Failed to read {}: {}", path, e))?;

    // Parse TOML to check for syntax errors
    let _: toml::Value =
        toml::from_str(&content).map_err(|e| format!("TOML parse error at {}: {}", path, e))?;

    // Check for duplicate keys by parsing manually
    let mut seen_keys: HashSet<String> = HashSet::new();
    let mut current_section = String::new();

    for line in content.lines() {
        let trimmed = line.trim();

        // Skip comments and empty lines
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Track section headers
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            current_section = trimmed[1..trimmed.len() - 1].to_string();
            continue;
        }

        // Check for key = value patterns
        if let Some(eq_pos) = trimmed.find('=') {
            let key = trimmed[..eq_pos].trim();

            // Skip keys that don't look like simple keys (no dots, brackets, etc.)
            if key.is_empty() || key.contains('[') || key.contains(']') || key.contains('.') {
                continue;
            }

            let full_key = if current_section.is_empty() {
                key.to_string()
            } else {
                format!("{}.{}", current_section, key)
            };

            if !seen_keys.insert(full_key.clone()) {
                return Err(format!("Duplicate key found in {}: {}", path, full_key));
            }
        }
    }

    println!("✓ {} - OK", path);
    Ok(())
}
