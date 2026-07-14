//! 生产 provider 组装与 credential 预检；协议实现始终来自 adapters。

use std::sync::Arc;

use camera_toolbox_adapters::{
    PlatformRegistry,
    platforms::{
        hisilicon_cv610::HisiliconCv610Provider,
        ssh_managed::{
            CommandRecipeRegistry, CredentialResolver, ProductionCredentialResolver,
            SshManagedPlatformProvider,
        },
    },
};
use camera_toolbox_app::{PlatformBindings, PlatformConfig, PlatformProfile};

use crate::state::BindingBackend;

pub(crate) struct ProductionBindingBackend {
    registry: PlatformRegistry,
    credentials: Arc<ProductionCredentialResolver>,
}

impl ProductionBindingBackend {
    pub(crate) fn new(recipes: Arc<CommandRecipeRegistry>) -> Self {
        let credentials = Arc::new(ProductionCredentialResolver::new());
        let resolver: Arc<dyn CredentialResolver> = credentials.clone();
        let registry = PlatformRegistry::new(
            HisiliconCv610Provider::default(),
            SshManagedPlatformProvider::production(resolver, recipes),
        );
        Self {
            registry,
            credentials,
        }
    }
}

impl BindingBackend for ProductionBindingBackend {
    fn bind(&self, profile: &PlatformProfile) -> Result<PlatformBindings, String> {
        if let PlatformConfig::SshManaged(config) = &profile.config {
            self.credentials
                .resolve(&config.credential_ref)
                .map_err(|error| {
                    if config.credential_ref.starts_with("session:") {
                        format!(
                            "{error}; this TUI process has no registered session secret. Use a key-file:/absolute/path profile or register the secret in-process before binding"
                        )
                    } else {
                        error
                    }
                })?;
        }
        self.registry
            .bind(profile)
            .map_err(|error| error.to_string())
    }
}
