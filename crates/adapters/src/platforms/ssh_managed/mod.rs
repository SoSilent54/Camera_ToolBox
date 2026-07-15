//! 独立 SSH-managed platform provider；不依赖或回退到 CV610 transport。

pub mod command;
pub mod connection;
pub mod credential;
pub mod host_key;
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
pub use credential::{
    PrivateKeyDiscoveryError, ProductionCredentialResolver, discover_private_key_files,
    discover_private_key_files_in,
};
pub use host_key::{
    HostKeyAssessment, HostKeyError, HostKeyManager, HostKeyTarget, HostKeyTrustOutcome,
    RusshServerHostKeyProbe, ServerHostKey, ServerHostKeyProbe,
};
pub use memory_transport::{MemoryRemoteFile, MemorySshTransport};
pub use provider::{SshManagedPlatformProvider, SshManagedProviderError};
pub use remote_file::SshRemoteFileService;

#[cfg(test)]
mod tests;
