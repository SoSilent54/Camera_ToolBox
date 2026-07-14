//! Camera Toolbox ratatui 平台控制台。

mod args;
mod production;
mod state;
mod terminal;
mod ui;

use std::{io::Write, sync::Arc};

use anyhow::{Context, Result};
use camera_toolbox_adapters::platforms::ssh_managed::production_recipe_registry_from_env;
use camera_toolbox_app::ProfileStore;

pub use args::{Args, RemoteFormatArg};
use production::ProductionBindingBackend;
use state::ConsoleState;
pub use terminal::write_fatal_error;

/// 构造生产 profile/registry/credential/recipe/controller 链路并运行。
pub fn run(args: Args) -> Result<()> {
    if args.snapshot {
        let snapshot = snapshot_to_string(args)?;
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        lock.write_all(snapshot.as_bytes())
            .context("failed to write snapshot")?;
        return Ok(());
    }
    terminal::run_interactive(production_state(args)?)
}

/// 从生产 profile/provider/resolver 链路生成确定性的非交互快照。
pub fn snapshot_to_string(args: Args) -> Result<String> {
    Ok(production_state(args)?.snapshot_text())
}

fn production_state(args: Args) -> Result<ConsoleState> {
    let (profiles, startup_message) = load_profiles(&args)?;
    let recipes = Arc::new(
        production_recipe_registry_from_env()
            .context("invalid production SSH recipe environment")?,
    );
    let backend = Box::new(ProductionBindingBackend::new(Arc::clone(&recipes)));
    ConsoleState::new(args, profiles, backend, recipes, startup_message)
}

fn load_profiles(args: &Args) -> Result<(ProfileStore, Option<String>)> {
    if let Some(path) = args.profile_store.as_deref() {
        return ProfileStore::load_from_path(path)
            .with_context(|| format!("failed to load profile store {}", path.display()))
            .map(|store| (store, None));
    }
    let project_path =
        ProfileStore::project_file_path().context("profile store path unavailable")?;
    if project_path.exists() {
        return ProfileStore::load_from_path(&project_path)
            .with_context(|| format!("failed to load profile store {}", project_path.display()))
            .map(|store| (store, None));
    }
    let store = ProfileStore::with_builtin_local().context("failed to create builtin profile")?;
    Ok((
        store,
        Some(format!(
            "Profile store {} does not exist; using read-only builtin Local profile",
            project_path.display()
        )),
    ))
}

#[cfg(test)]
mod tests;
