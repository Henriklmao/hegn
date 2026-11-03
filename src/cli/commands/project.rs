//! `hegn project` handlers: create, import, inspect, delete and restore
//! projects under `projects_root`.

use std::io::IsTerminal;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::config::{Config, Harness};
use crate::project::{self, ProjectMeta};
use crate::tui::app::PACKAGE_CHOICES;

use super::instance::prompt_yes;
use crate::cli::args::ProjectCommands;
use crate::cli::resolve::{resolve_project, resolve_project_entry};

pub(crate) fn project_command(command: ProjectCommands, config: Config, json: bool) -> Result<i32> {
    match command {
        ProjectCommands::Create {
            name,
            harness,
            template,
            packages: package_args,
        } => {
            let harness = harness.unwrap_or(config.harness);
            let template = match template {
                Some(path) => {
                    if !path.is_file() {
                        bail!("template {} does not exist", path.display());
                    }
                    Some(path)
                }
                None => {
                    let path = config.templates_dir.join("AGENTS.md");
                    path.is_file().then_some(path)
                }
            };
            let path = project::create_project(
                &config.projects_root,
                &name,
                template.as_deref(),
                harness,
            )?;
            let explicit_packages = !package_args.is_empty();
            // `--package` adds to the harness preset; it never replaces it.
            let mut packages: Vec<String> = harness
                .packages()
                .iter()
                .map(|package| package.to_string())
                .collect();
            for package in package_args {
                if !crate::config::safe_package_name(&package) {
                    bail!("invalid package name: {package:?}");
                }
                if explicit_packages && !PACKAGE_CHOICES.contains(&package.as_str()) {
                    eprintln!("note: '{package}' is not a built-in template package");
                }
                if !packages.iter().any(|existing| existing == &package) {
                    packages.push(package);
                }
            }
            let meta = ProjectMeta {
                harness: Some(harness),
                packages: packages.clone(),
                template: template.map(|p| p.display().to_string()),
                ..ProjectMeta::default()
            };
            project::write_meta(&path, &meta)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "path": path,
                        "harness": harness,
                        "preset": harness.preset_summary(),
                        "packages": packages,
                    }))?
                );
            } else {
                println!("Created {}", path.display());
                println!(
                    "Harness: {} ({}) — {}",
                    harness.label(),
                    harness.command(),
                    harness.preset_summary()
                );
                println!("Packages: {}", packages.join(", "));
                println!("Next: hegn open {}", path.display());
            }
            Ok(0)
        }
        ProjectCommands::Import { path, name } => {
            let link = config.import_project(&path, name.as_deref())?;
            let target = link.canonicalize().with_context(|| {
                format!("failed to resolve imported project {}", link.display())
            })?;
            let info = project::Project::load(link.clone());
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "name": info.name,
                        "path": link,
                        "target": target,
                        "git": info.git,
                    }))?
                );
            } else {
                println!("Imported {}", info.name);
                println!("  link   {}", link.display());
                println!("  target {}", target.display());
                println!("  git    {}", info.git.badge());
                println!("Next: hegn open {}", info.name);
            }
            Ok(0)
        }
        ProjectCommands::Info { project } => {
            let path = resolve_project(&config, &project)?;
            let info = project::Project::load(path.clone());
            let meta = project::read_meta(&path);
            let harness = meta.harness.unwrap_or(config.harness);
            if json {
                #[derive(Serialize)]
                struct Info<'a> {
                    name: &'a str,
                    path: &'a Path,
                    git: &'a project::GitSummary,
                    harness: Harness,
                    packages: &'a [String],
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&Info {
                        name: &info.name,
                        path: &info.path,
                        git: &info.git,
                        harness,
                        packages: &meta.packages,
                    })?
                );
            } else {
                println!("{}", info.name);
                println!("  path     {}", info.path.display());
                println!("  git      {}", info.git.badge());
                if let Some(err) = &info.git.error {
                    println!("  git note {err}");
                }
                println!("  harness  {}", harness.label());
                if !meta.packages.is_empty() {
                    println!("  packages {}", meta.packages.join(", "));
                }
            }
            Ok(0)
        }
        ProjectCommands::Delete {
            project: selector,
            delete_files,
            keep_files,
            yes,
        } => {
            let entry = resolve_project_entry(&config, &selector)?;
            let info = project::Project::load(entry.clone());
            print_project_git(&entry, &info, json);
            if !yes
                && !prompt_yes(&format!(
                    "Remove project '{}' from the picker and delete its instances?",
                    info.name
                ))?
            {
                return Ok(0);
            }
            let delete_files = if delete_files {
                true
            } else if keep_files {
                false
            } else if std::io::stdin().is_terminal() {
                prompt_yes(&format!(
                    "Also delete the project files at {}?",
                    entry.display()
                ))?
            } else {
                bail!("choose --delete-files or --keep-files in non-interactive mode");
            };
            let outcome = project::delete_project(&config, &entry, delete_files)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "project": entry,
                        "files_deleted": outcome.files_deleted,
                        "instances": outcome.instances,
                    }))?
                );
            } else {
                println!(
                    "Removed {} ({}); {}",
                    info.name,
                    if outcome.instances.is_empty() {
                        "no instances".to_string()
                    } else {
                        format!("instances: {}", outcome.instances.join(", "))
                    },
                    if outcome.files_deleted {
                        "project files deleted"
                    } else {
                        "project files kept"
                    }
                );
            }
            Ok(0)
        }
        ProjectCommands::Restore { project: selector } => {
            let entry = resolve_project_entry(&config, &selector)?;
            let mut store =
                crate::session::state::StateStore::load(config.state_dir.join("state.json"));
            store.set_project_hidden(&entry, false)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "project": entry,
                        "hidden": false,
                    }))?
                );
            } else {
                println!("Restored {} to the picker", entry.display());
            }
            Ok(0)
        }
    }
}

fn print_project_git(path: &Path, info: &project::Project, json: bool) {
    if json {
        return;
    }
    println!("{}", info.name);
    println!("  path     {}", path.display());
    println!("  git      {}", info.git.badge());
    if let Some(error) = &info.git.error {
        println!("  git note {error}");
    }
    if info.git.is_dirty() {
        println!(
            "  changes  {} staged, {} modified, {} untracked",
            info.git.staged, info.git.modified, info.git.untracked
        );
        println!("  warning  uncommitted changes are lost if the files are deleted");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_support::config;
    use crate::cli::{Cli, Commands};
    use clap::Parser;
    use std::path::PathBuf;

    #[test]
    fn project_create_writes_metadata() {
        let (_dir, config) = config();
        let code = project_command(
            ProjectCommands::Create {
                name: "demo".into(),
                harness: Some(Harness::Codex),
                template: None,
                packages: vec!["git".into(), "python3".into()],
            },
            config.clone(),
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        let meta = project::read_meta(&config.projects_root.join("demo"));
        assert_eq!(meta.harness, Some(Harness::Codex));
        // The preset stays complete; --package only adds to it.
        for package in ["git", "nodejs", "curl", "python3"] {
            assert!(
                meta.packages.iter().any(|existing| existing == package),
                "missing {package}: {:?}",
                meta.packages
            );
        }
        assert_eq!(
            meta.packages
                .iter()
                .filter(|package| *package == "git")
                .count(),
            1
        );
    }

    #[test]
    fn project_import_creates_a_picker_symlink() {
        let (dir, config) = config();
        let source = dir.path().join("outside/demo");
        std::fs::create_dir_all(&source).unwrap();
        let code = project_command(
            ProjectCommands::Import {
                path: source.clone(),
                name: None,
            },
            config.clone(),
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        let link = config.projects_root.join("demo");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(link.canonicalize().unwrap(), source.canonicalize().unwrap());
    }

    #[test]
    fn project_import_parses_and_rejects_unsafe_sources() {
        let (_dir, config) = config();
        let cli = Cli::try_parse_from(["hegn", "project", "import", "/tmp/example", "--name", "x"])
            .unwrap();
        match cli.command {
            Some(Commands::Project {
                command: ProjectCommands::Import { path, name },
            }) => {
                assert_eq!(path, PathBuf::from("/tmp/example"));
                assert_eq!(name.as_deref(), Some("x"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(config.import_project(Path::new("/"), None).is_err());
    }

    #[test]
    fn project_delete_keeps_or_removes_files_and_can_be_restored() {
        let (_dir, config) = config();
        let project = config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("README.md"), b"demo").unwrap();

        let code = project_command(
            ProjectCommands::Delete {
                project: "demo".into(),
                delete_files: false,
                keep_files: true,
                yes: true,
            },
            config.clone(),
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert!(project.join("README.md").is_file(), "files must be kept");
        let store = crate::session::state::StateStore::load(config.state_dir.join("state.json"));
        assert!(store.is_hidden(&project));

        let code = project_command(
            ProjectCommands::Restore {
                project: "demo".into(),
            },
            config.clone(),
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        let store = crate::session::state::StateStore::load(config.state_dir.join("state.json"));
        assert!(!store.is_hidden(&project));

        let code = project_command(
            ProjectCommands::Delete {
                project: "demo".into(),
                delete_files: true,
                keep_files: false,
                yes: true,
            },
            config,
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert!(!project.exists());
    }

    #[test]
    fn project_delete_never_follows_an_import_link() {
        let (dir, config) = config();
        let source = dir.path().join("outside/demo");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("README.md"), b"demo").unwrap();
        project_command(
            ProjectCommands::Import {
                path: source.clone(),
                name: None,
            },
            config.clone(),
            true,
        )
        .unwrap();
        let link = config.projects_root.join("demo");

        assert!(
            resolve_project_entry(&config, &source.display().to_string()).is_err(),
            "deletion must only target picker entries"
        );
        project_command(
            ProjectCommands::Delete {
                project: "demo".into(),
                delete_files: true,
                keep_files: false,
                yes: true,
            },
            config,
            true,
        )
        .unwrap();
        assert!(!link.exists());
        assert!(source.join("README.md").is_file());
    }

    #[test]
    fn project_create_rejects_a_missing_template() {
        let (_dir, config) = config();
        let err = project_command(
            ProjectCommands::Create {
                name: "demo".into(),
                harness: None,
                template: Some(PathBuf::from("/definitely/missing/AGENTS.md")),
                packages: Vec::new(),
            },
            config,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }
}
