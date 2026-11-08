//! `hegn network` handlers for the host-enforced guest network guard.

use anyhow::{Context, Result};

use crate::config::Config;

use crate::cli::args::NetworkCommands;

pub(crate) fn network_command(command: NetworkCommands, config: Config, json: bool) -> Result<i32> {
    match command {
        NetworkCommands::Status => {
            let marker = crate::network::ready_marker();
            let taps = crate::network::status(&config);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "ready": marker.is_some(),
                        "marker": marker,
                        "mode": config.network.label(),
                        "endpoints": config.endpoint_ports(),
                        "endpoints_applied": marker.as_ref().map(|marker| marker.endpoints.clone()),
                        "taps": taps,
                    }))?
                );
            } else {
                match &marker {
                    Some(marker) => println!(
                        "guard: ready (subnet {}, {} TAPs, host firewall {}, applied {})",
                        marker.subnet,
                        marker.pool,
                        marker.host_firewall.label(),
                        marker.applied_at
                    ),
                    None => println!(
                        "guard: not configured; run `sudo hegn network setup --user $USER`"
                    ),
                }
                println!("mode: {}", config.network.label());
                let ports = config.endpoint_ports();
                if ports.is_empty() {
                    println!("endpoints: none");
                } else {
                    println!(
                        "endpoints: {}",
                        config
                            .local_endpoints
                            .iter()
                            .map(|endpoint| format!(
                                "{}=127.0.0.1:{}",
                                endpoint.name, endpoint.port
                            ))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                if marker
                    .as_ref()
                    .is_some_and(|marker| marker.endpoints != ports)
                {
                    println!("note: endpoints changed; re-run `sudo hegn network setup`");
                }
                for tap in taps {
                    let owner = tap.owner.as_deref().unwrap_or("-");
                    println!(
                        "  {:<8} {:<15} {} owner={owner}",
                        tap.tap,
                        tap.host_ip,
                        if tap.exists { "up" } else { "missing" }
                    );
                }
            }
            Ok(0)
        }
        NetworkCommands::Setup { user, dry_run } => {
            let user = user
                .or_else(|| std::env::var("SUDO_USER").ok())
                .or_else(|| std::env::var("USER").ok())
                .context("could not determine the TAP owner; pass --user NAME")?;
            if dry_run {
                println!(
                    "{}",
                    crate::network::setup_script(&user, &config.endpoint_ports())?
                );
                return Ok(0);
            }
            crate::network::setup(&user, &config.endpoint_ports())?;
            if json {
                println!("{}", serde_json::json!({"ready": true, "user": user}));
            } else {
                println!(
                    "guarded network is active; TAPs are owned by '{user}'. Start an instance with \
                     `hegn instance start <name>`."
                );
            }
            Ok(0)
        }
        NetworkCommands::Reload { dry_run } => {
            let endpoints = config.endpoint_ports();
            if dry_run {
                println!("{}", crate::network::reload_script(&endpoints)?);
                return Ok(0);
            }
            crate::network::reload(&endpoints)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({"ready": true, "endpoints": endpoints})
                );
            } else {
                println!(
                    "network guard reloaded with {} endpoint(s); the TAP pool was left untouched.",
                    endpoints.len()
                );
            }
            Ok(0)
        }
        NetworkCommands::Teardown => {
            crate::network::teardown()?;
            if json {
                println!("{}", serde_json::json!({"ready": false}));
            } else {
                println!("guarded network removed.");
            }
            Ok(0)
        }
    }
}
