//! 独立 SSH-managed platform provider；不依赖或回退到 CV610 transport。

pub mod command;
pub mod connection;
pub mod credential;
pub mod memory_transport;
pub mod provider;
pub mod remote_file;
pub mod watcher;
pub use command::{
    CommandParameterKind, CommandParameterSpec, CommandRecipe, CommandRecipeRegistry, RecipeArg,
    SshCommandService, production_recipe_registry_from_env,
};
pub use connection::{
    CredentialResolver, RusshTransportFactory, SshConnectionTarget, SshCredential,
    SshTransportError, SshTransportFactory, SshTransportSession, TransportCommandOutput,
    TransportDirEntry,
};
pub use credential::ProductionCredentialResolver;
pub use memory_transport::{MemoryRemoteFile, MemorySshTransport};
pub use provider::{SshManagedPlatformProvider, SshManagedProviderError};
pub use remote_file::SshRemoteFileService;

#[cfg(test)]
mod tests;
