//! `hegn images` handlers for printing, downloading and verifying the pinned
//! upstream VM assets.

use anyhow::Result;

use crate::config::Config;
use crate::vm::images::{self, AssetStatus};

use crate::cli::args::ImageCommands;
use crate::cli::output::human_size;

pub(crate) fn image_command(command: ImageCommands, config: Config, json: bool) -> Result<i32> {
    match command {
        ImageCommands::Download {
            force,
            allow_unverified,
        } => {
            let assets = images::download_all(&config, force, allow_unverified)?;
            print_assets(&assets, json)?;
            Ok(0)
        }
        ImageCommands::Status => {
            print_assets(&images::status(&config), json)?;
            Ok(0)
        }
        ImageCommands::Verify => {
            let mut results = Vec::new();
            let mut failed = false;
            for asset in images::status(&config) {
                let result = if !asset.exists {
                    Err("missing".to_string())
                } else {
                    images::verify_file(&asset.path, &asset.sha256)
                        .map(|_| ())
                        .map_err(|err| err.to_string())
                };
                if result.is_err() {
                    failed = true;
                }
                results.push(serde_json::json!({
                    "name": asset.name,
                    "path": asset.path,
                    "sha256": asset.sha256,
                    "ok": result.is_ok(),
                    "error": result.err(),
                }));
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                for result in &results {
                    let ok = result["ok"].as_bool().unwrap_or(false);
                    println!(
                        "{} {:<8} {}",
                        if ok { "✓" } else { "✗" },
                        result["name"].as_str().unwrap_or("asset"),
                        result["path"].as_str().unwrap_or("")
                    );
                    if let Some(error) = result["error"].as_str() {
                        println!("  {error}");
                    }
                }
            }
            Ok(if failed { 1 } else { 0 })
        }
    }
}

fn print_assets(assets: &[AssetStatus], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(assets)?);
        return Ok(());
    }
    println!("{:<8} {:<8} {:>12}  PATH", "NAME", "STATE", "SIZE");
    for asset in assets {
        println!(
            "{:<8} {:<8} {:>12}  {}",
            asset.name,
            if asset.exists { "present" } else { "missing" },
            asset
                .size_bytes
                .map(human_size)
                .unwrap_or_else(|| "-".into()),
            asset.path.display()
        );
    }
    if assets.iter().any(AssetStatus::missing) {
        println!("\nDownload with: hegn images download");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_support::config;

    #[test]
    fn image_status_and_missing_verify() {
        let (dir, mut config) = config();
        config.kernel_image = dir.path().join("missing-kernel");
        config.rootfs_image = dir.path().join("missing-rootfs");
        assert_eq!(
            image_command(ImageCommands::Status, config.clone(), true).unwrap(),
            0
        );
        // Missing images are a verification failure, not a pass.
        assert_eq!(
            image_command(ImageCommands::Verify, config, true).unwrap(),
            1
        );
    }
}
