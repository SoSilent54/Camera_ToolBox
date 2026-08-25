//! Allowlisted recipe → argv 编译与 `CommandService`。

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use camera_toolbox_app::{
    CommandParameter, CommandResult, CommandService, CommandServiceError, CommandTerminal,
    RemoteOperationControl, RemoteStage, TypedCommandRequest,
};

use super::connection::{
    CredentialResolver, SshConnectionTarget, SshTransportError, SshTransportFactory,
    TransportCommandOutput,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandParameterKind {
    Signed { min: i64, max: i64 },
    Unsigned { min: u64, max: u64 },
    Bool,
    Choice(BTreeSet<String>),
    RemotePath { root: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandParameterSpec {
    pub name: String,
    pub kind: CommandParameterKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipeArg {
    Literal(String),
    Parameter(String),
    BoolFlag { parameter: String, flag: String },
}

/// program 与参数布局均由应用部署时注册；profile 只能选择 recipe id。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRecipe {
    pub id: String,
    pub program: String,
    pub parameters: Vec<CommandParameterSpec>,
    pub argv: Vec<RecipeArg>,
    /// 成功时 stdout 必须是一个 UTF-8 path line。
    pub artifact_path_from_stdout: bool,
}

#[derive(Debug, Clone, Default)]
pub struct CommandRecipeRegistry {
    recipes: BTreeMap<String, CommandRecipe>,
}

impl CommandRecipeRegistry {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            recipes: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn contains(&self, recipe_id: &str) -> bool {
        self.recipes.contains_key(recipe_id)
    }

    #[must_use]
    pub fn get(&self, recipe_id: &str) -> Option<&CommandRecipe> {
        self.recipes.get(recipe_id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &CommandRecipe> {
        self.recipes.values()
    }

    /// # Errors
    ///
    /// recipe 无效或 id 重复时拒绝。
    pub fn register(&mut self, recipe: CommandRecipe) -> Result<(), CommandServiceError> {
        validate_recipe(&recipe)?;
        if self.recipes.contains_key(&recipe.id) {
            return Err(CommandServiceError::InvalidRequest(format!(
                "duplicate command recipe {}",
                recipe.id
            )));
        }
        self.recipes.insert(recipe.id.clone(), recipe);
        Ok(())
    }

    fn build_argv(
        &self,
        request: &TypedCommandRequest,
    ) -> Result<(Vec<String>, bool), CommandServiceError> {
        let recipe = self.recipes.get(&request.recipe_id).ok_or_else(|| {
            CommandServiceError::RecipeNotAllowed {
                recipe_id: request.recipe_id.clone(),
            }
        })?;
        let specs: BTreeMap<_, _> = recipe
            .parameters
            .iter()
            .map(|spec| (spec.name.as_str(), spec))
            .collect();
        for name in request.parameters.keys() {
            if !specs.contains_key(name.as_str()) {
                return Err(CommandServiceError::InvalidParameter {
                    parameter: name.clone(),
                    reason: "parameter is not declared by recipe".to_owned(),
                });
            }
        }
        for spec in &recipe.parameters {
            if !request.parameters.contains_key(&spec.name) {
                return Err(CommandServiceError::InvalidParameter {
                    parameter: spec.name.clone(),
                    reason: "required parameter is missing".to_owned(),
                });
            }
        }

        let mut argv = Vec::with_capacity(recipe.argv.len().saturating_add(1));
        argv.push(recipe.program.clone());
        for item in &recipe.argv {
            match item {
                RecipeArg::Literal(value) => argv.push(value.clone()),
                RecipeArg::Parameter(name) => {
                    let spec = specs.get(name.as_str()).ok_or_else(|| {
                        CommandServiceError::InvalidRequest(format!(
                            "argv references undeclared parameter {name}"
                        ))
                    })?;
                    let value = request.parameters.get(name).ok_or_else(|| {
                        CommandServiceError::InvalidParameter {
                            parameter: name.clone(),
                            reason: "required parameter is missing".to_owned(),
                        }
                    })?;
                    argv.push(parameter_to_argv(spec, value)?);
                }
                RecipeArg::BoolFlag { parameter, flag } => {
                    let spec = specs.get(parameter.as_str()).ok_or_else(|| {
                        CommandServiceError::InvalidRequest(format!(
                            "flag references undeclared parameter {parameter}"
                        ))
                    })?;
                    let value = request.parameters.get(parameter).ok_or_else(|| {
                        CommandServiceError::InvalidParameter {
                            parameter: parameter.clone(),
                            reason: "required parameter is missing".to_owned(),
                        }
                    })?;
                    match (&spec.kind, value) {
                        (CommandParameterKind::Bool, CommandParameter::Bool(true)) => {
                            argv.push(flag.clone());
                        }
                        (CommandParameterKind::Bool, CommandParameter::Bool(false)) => {}
                        _ => {
                            return Err(CommandServiceError::InvalidParameter {
                                parameter: parameter.clone(),
                                reason: "boolean flag requires a bool value".to_owned(),
                            });
                        }
                    }
                }
            }
        }
        Ok((argv, recipe.artifact_path_from_stdout))
    }
}

fn validate_recipe(recipe: &CommandRecipe) -> Result<(), CommandServiceError> {
    if recipe.id.trim().is_empty() {
        return Err(CommandServiceError::InvalidRequest(
            "recipe id must not be empty".to_owned(),
        ));
    }
    if !is_absolute_safe_path(&recipe.program) {
        return Err(CommandServiceError::InvalidRequest(
            "recipe program must be an absolute normalized path".to_owned(),
        ));
    }
    let mut names = BTreeSet::new();
    for spec in &recipe.parameters {
        if spec.name.is_empty()
            || !spec
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            || !names.insert(spec.name.as_str())
        {
            return Err(CommandServiceError::InvalidRequest(
                "recipe parameter names must be unique ASCII identifiers".to_owned(),
            ));
        }
        match &spec.kind {
            CommandParameterKind::Signed { min, max } if min > max => {
                return Err(CommandServiceError::InvalidRequest(format!(
                    "invalid range for {}",
                    spec.name
                )));
            }
            CommandParameterKind::Unsigned { min, max } if min > max => {
                return Err(CommandServiceError::InvalidRequest(format!(
                    "invalid range for {}",
                    spec.name
                )));
            }
            CommandParameterKind::Choice(choices) if choices.is_empty() => {
                return Err(CommandServiceError::InvalidRequest(format!(
                    "empty choice set for {}",
                    spec.name
                )));
            }
            CommandParameterKind::RemotePath { root } if !is_absolute_safe_path(root) => {
                return Err(CommandServiceError::InvalidRequest(format!(
                    "invalid remote root for {}",
                    spec.name
                )));
            }
            _ => {}
        }
    }
    for item in &recipe.argv {
        match item {
            RecipeArg::Literal(value) if value.as_bytes().contains(&0) => {
                return Err(CommandServiceError::InvalidRequest(
                    "literal argv contains NUL".to_owned(),
                ));
            }
            RecipeArg::Parameter(name) if !names.contains(name.as_str()) => {
                return Err(CommandServiceError::InvalidRequest(format!(
                    "argv references undeclared parameter {name}"
                )));
            }
            RecipeArg::BoolFlag { parameter, flag } => {
                if !names.contains(parameter.as_str())
                    || flag.is_empty()
                    || flag.as_bytes().contains(&0)
                {
                    return Err(CommandServiceError::InvalidRequest(
                        "invalid boolean flag declaration".to_owned(),
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn parameter_to_argv(
    spec: &CommandParameterSpec,
    value: &CommandParameter,
) -> Result<String, CommandServiceError> {
    let invalid = |reason: &str| CommandServiceError::InvalidParameter {
        parameter: spec.name.clone(),
        reason: reason.to_owned(),
    };
    match (&spec.kind, value) {
        (CommandParameterKind::Signed { min, max }, CommandParameter::Signed(value))
            if (*min..=*max).contains(value) =>
        {
            Ok(value.to_string())
        }
        (CommandParameterKind::Unsigned { min, max }, CommandParameter::Unsigned(value))
            if (*min..=*max).contains(value) =>
        {
            Ok(value.to_string())
        }
        (CommandParameterKind::Bool, CommandParameter::Bool(value)) => Ok(value.to_string()),
        (CommandParameterKind::Choice(choices), CommandParameter::Choice(value))
            if choices.contains(value) =>
        {
            Ok(value.clone())
        }
        (CommandParameterKind::RemotePath { root }, CommandParameter::RemotePath(value))
            if path_is_within_root(value, root) =>
        {
            Ok(value.clone())
        }
        (CommandParameterKind::Signed { .. }, CommandParameter::Signed(_))
        | (CommandParameterKind::Unsigned { .. }, CommandParameter::Unsigned(_)) => {
            Err(invalid("numeric value is outside the allowlisted range"))
        }
        (CommandParameterKind::Choice(_), CommandParameter::Choice(_)) => {
            Err(invalid("choice is not allowlisted"))
        }
        (CommandParameterKind::RemotePath { .. }, CommandParameter::RemotePath(_)) => {
            Err(invalid("path is outside the allowlisted root"))
        }
        _ => Err(invalid("parameter type does not match recipe schema")),
    }
}

pub(crate) fn path_is_within_root(path: &str, root: &str) -> bool {
    if !is_absolute_safe_path(path) || !is_absolute_safe_path(root) {
        return false;
    }
    let root = root.trim_end_matches('/');
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

/// 从完整的部署环境变量组加载一个 production typed recipe；部分配置会明确失败。
pub fn production_recipe_registry_from_env() -> Result<CommandRecipeRegistry, CommandServiceError> {
    const KEYS: [(&str, &str); 8] = [
        ("CAMERA_TOOLBOX_SSH_RECIPE_ID", "recipe id"),
        ("CAMERA_TOOLBOX_SSH_RECIPE_PROGRAM", "absolute program"),
        (
            "CAMERA_TOOLBOX_SSH_RECIPE_OUTPUT_ROOT",
            "remote output root",
        ),
        (
            "CAMERA_TOOLBOX_SSH_RECIPE_FORMATS",
            "comma-separated formats",
        ),
        (
            "CAMERA_TOOLBOX_SSH_RECIPE_OUTPUT_DIR_FLAG",
            "output-dir flag",
        ),
        ("CAMERA_TOOLBOX_SSH_RECIPE_FORMAT_FLAG", "format flag"),
        ("CAMERA_TOOLBOX_SSH_RECIPE_ONCE_FLAG", "one-shot flag"),
        (
            "CAMERA_TOOLBOX_SSH_RECIPE_PATH_STDOUT",
            "path-on-stdout boolean",
        ),
    ];
    let values: Vec<_> = KEYS
        .iter()
        .map(|(key, _)| std::env::var(key).ok())
        .collect();
    if values.iter().all(Option::is_none) {
        return Ok(CommandRecipeRegistry::new());
    }
    let mut required = Vec::with_capacity(KEYS.len());
    for ((key, description), value) in KEYS.iter().zip(values) {
        let value = value.filter(|value| !value.is_empty()).ok_or_else(|| {
            CommandServiceError::InvalidRequest(format!(
                "production SSH recipe is partially configured: {key} ({description}) is missing"
            ))
        })?;
        required.push(value);
    }
    if required[7] != "true" {
        return Err(CommandServiceError::InvalidRequest(
            "CAMERA_TOOLBOX_SSH_RECIPE_PATH_STDOUT must be exactly true; capture flow requires one returned artifact path"
                .to_owned(),
        ));
    }
    let formats: BTreeSet<_> = required[3]
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect();
    if formats.is_empty() {
        return Err(CommandServiceError::InvalidRequest(
            "CAMERA_TOOLBOX_SSH_RECIPE_FORMATS must contain at least one explicit value".to_owned(),
        ));
    }
    let mut registry = CommandRecipeRegistry::new();
    registry.register(CommandRecipe {
        id: required[0].clone(),
        program: required[1].clone(),
        parameters: vec![
            CommandParameterSpec {
                name: "output_dir".to_owned(),
                kind: CommandParameterKind::RemotePath {
                    root: required[2].clone(),
                },
            },
            CommandParameterSpec {
                name: "format".to_owned(),
                kind: CommandParameterKind::Choice(formats),
            },
        ],
        argv: vec![
            RecipeArg::Literal(required[4].clone()),
            RecipeArg::Parameter("output_dir".to_owned()),
            RecipeArg::Literal(required[5].clone()),
            RecipeArg::Parameter("format".to_owned()),
            RecipeArg::Literal(required[6].clone()),
        ],
        artifact_path_from_stdout: true,
    })?;
    Ok(registry)
}

fn is_absolute_safe_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains('\0')
        && !path.split('/').any(|component| component == "..")
}

pub struct SshCommandService {
    service_id: String,
    target: SshConnectionTarget,
    credential_ref: String,
    allowed_recipe_id: String,
    resolver: Arc<dyn CredentialResolver>,
    transport: Arc<dyn SshTransportFactory>,
    recipes: Arc<CommandRecipeRegistry>,
    output_limit: usize,
}

impl SshCommandService {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        service_id: String,
        target: SshConnectionTarget,
        credential_ref: String,
        allowed_recipe_id: String,
        resolver: Arc<dyn CredentialResolver>,
        transport: Arc<dyn SshTransportFactory>,
        recipes: Arc<CommandRecipeRegistry>,
        output_limit: usize,
    ) -> Self {
        Self {
            service_id,
            target,
            credential_ref,
            allowed_recipe_id,
            resolver,
            transport,
            recipes,
            output_limit,
        }
    }
}

impl CommandService for SshCommandService {
    fn service_id(&self) -> &str {
        &self.service_id
    }

    fn execute(
        &self,
        request: TypedCommandRequest,
        control: RemoteOperationControl,
    ) -> Result<CommandResult, CommandServiceError> {
        check_control(&control)?;
        if request.recipe_id != self.allowed_recipe_id {
            return Err(CommandServiceError::RecipeNotAllowed {
                recipe_id: request.recipe_id,
            });
        }
        let (argv, artifact_path_from_stdout) = self.recipes.build_argv(&request)?;
        control.report(RemoteStage::ResolvingCredential);
        let credential = self
            .resolver
            .resolve(&self.credential_ref)
            .map_err(CommandServiceError::CredentialResolution)?;
        check_control(&control)?;
        control.report(RemoteStage::Connecting);
        let mut session = self
            .transport
            .connect(&self.target, credential, &control)
            .map_err(map_transport_error)?;
        let _interrupt = InterruptRegistration(&control);
        control.report(RemoteStage::Executing);
        let output = session
            .execute_argv(&argv, self.output_limit, &control)
            .map_err(map_transport_error)?;
        command_result(output, artifact_path_from_stdout)
    }
}

struct InterruptRegistration<'a>(&'a RemoteOperationControl);

impl Drop for InterruptRegistration<'_> {
    fn drop(&mut self) {
        self.0.cancellation.clear_interrupt();
    }
}

fn check_control(control: &RemoteOperationControl) -> Result<(), CommandServiceError> {
    if control.cancellation.is_cancelled() {
        Err(CommandServiceError::Cancelled)
    } else if control.deadline_expired() {
        Err(CommandServiceError::DeadlineExceeded)
    } else {
        Ok(())
    }
}

fn map_transport_error(error: SshTransportError) -> CommandServiceError {
    match error {
        SshTransportError::HostKeyMismatch => CommandServiceError::HostKeyMismatch,
        SshTransportError::AuthenticationFailed => CommandServiceError::AuthenticationFailed,
        SshTransportError::Cancelled => CommandServiceError::Cancelled,
        SshTransportError::TimedOut => CommandServiceError::DeadlineExceeded,
        SshTransportError::Transport(reason) => CommandServiceError::Transport(reason),
        SshTransportError::HelperProtocol(reason) => CommandServiceError::HelperProtocol(reason),
        SshTransportError::NotFound(reason)
        | SshTransportError::PermissionDenied(reason)
        | SshTransportError::Disconnected(reason)
        | SshTransportError::AlreadyExists(reason)
        | SshTransportError::ChangedDuringRead(reason) => CommandServiceError::Transport(reason),
        SshTransportError::ReadLimitExceeded { requested, limit } => {
            CommandServiceError::Transport(format!(
                "remote read exceeds bound: {requested} > {limit}"
            ))
        }
        SshTransportError::InvalidContinuation => {
            CommandServiceError::HelperProtocol("invalid remote directory continuation".to_owned())
        }
        SshTransportError::Unsupported => {
            CommandServiceError::HelperProtocol("remote operation is unsupported".to_owned())
        }
    }
}

fn command_result(
    output: TransportCommandOutput,
    artifact_path_from_stdout: bool,
) -> Result<CommandResult, CommandServiceError> {
    let terminal = match output.exit_status {
        None => CommandTerminal::RemoteStateUnknown,
        Some(status) if output.stdout_truncated || output.stderr_truncated => {
            CommandTerminal::OutputTruncated {
                status: Some(status),
            }
        }
        Some(0) => CommandTerminal::Succeeded,
        Some(status) => CommandTerminal::ExitFailure { status },
    };
    let artifact_path =
        if artifact_path_from_stdout && matches!(terminal, CommandTerminal::Succeeded) {
            let text = std::str::from_utf8(&output.stdout).map_err(|error| {
                CommandServiceError::HelperProtocol(format!(
                    "artifact path stdout is not UTF-8: {error}"
                ))
            })?;
            let path = text.trim_end_matches(['\r', '\n']);
            if path.is_empty() || path.contains(['\r', '\n', '\0']) {
                return Err(CommandServiceError::HelperProtocol(
                    "artifact path stdout must contain exactly one non-empty line".to_owned(),
                ));
            }
            Some(path.to_owned())
        } else {
            None
        };
    Ok(CommandResult {
        terminal,
        stdout: output.stdout,
        stderr: output.stderr,
        stdout_truncated: output.stdout_truncated,
        stderr_truncated: output.stderr_truncated,
        artifact_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipe_registry() -> CommandRecipeRegistry {
        let mut registry = CommandRecipeRegistry::new();
        registry
            .register(CommandRecipe {
                id: "capture".to_owned(),
                program: "/usr/libexec/camera-toolbox-capture".to_owned(),
                parameters: vec![
                    CommandParameterSpec {
                        name: "frames".to_owned(),
                        kind: CommandParameterKind::Unsigned { min: 1, max: 8 },
                    },
                    CommandParameterSpec {
                        name: "format".to_owned(),
                        kind: CommandParameterKind::Choice(BTreeSet::from([
                            "raw10".to_owned(),
                            "raw12".to_owned(),
                        ])),
                    },
                    CommandParameterSpec {
                        name: "output".to_owned(),
                        kind: CommandParameterKind::RemotePath {
                            root: "/data/captures".to_owned(),
                        },
                    },
                ],
                argv: vec![
                    RecipeArg::Literal("--frames".to_owned()),
                    RecipeArg::Parameter("frames".to_owned()),
                    RecipeArg::Literal("--format".to_owned()),
                    RecipeArg::Parameter("format".to_owned()),
                    RecipeArg::Parameter("output".to_owned()),
                ],
                artifact_path_from_stdout: true,
            })
            .unwrap();
        registry
    }

    #[test]
    fn rejects_unknown_recipe_and_extra_parameters() {
        let registry = recipe_registry();
        let unknown = TypedCommandRequest::new("not-registered").unwrap();
        assert!(matches!(
            registry.build_argv(&unknown),
            Err(CommandServiceError::RecipeNotAllowed { .. })
        ));
        let request = TypedCommandRequest::new("capture")
            .unwrap()
            .with_parameter("frames", CommandParameter::Unsigned(1))
            .with_parameter("format", CommandParameter::Choice("raw12".to_owned()))
            .with_parameter(
                "output",
                CommandParameter::RemotePath("/data/captures/a.raw".to_owned()),
            )
            .with_parameter("injected", CommandParameter::Bool(true));
        assert!(matches!(
            registry.build_argv(&request),
            Err(CommandServiceError::InvalidParameter { parameter, .. }) if parameter == "injected"
        ));
    }

    #[test]
    fn injection_text_never_becomes_structure_or_escapes_path_root() {
        let registry = recipe_registry();
        let request = TypedCommandRequest::new("capture")
            .unwrap()
            .with_parameter("frames", CommandParameter::Unsigned(1))
            .with_parameter(
                "format",
                CommandParameter::Choice("raw12; touch /tmp/pwned".to_owned()),
            )
            .with_parameter(
                "output",
                CommandParameter::RemotePath("/data/captures/a.raw".to_owned()),
            );
        assert!(matches!(
            registry.build_argv(&request),
            Err(CommandServiceError::InvalidParameter { parameter, .. }) if parameter == "format"
        ));

        let traversal = TypedCommandRequest::new("capture")
            .unwrap()
            .with_parameter("frames", CommandParameter::Unsigned(1))
            .with_parameter("format", CommandParameter::Choice("raw12".to_owned()))
            .with_parameter(
                "output",
                CommandParameter::RemotePath("/data/captures/../pwned".to_owned()),
            );
        assert!(matches!(
            registry.build_argv(&traversal),
            Err(CommandServiceError::InvalidParameter { parameter, .. }) if parameter == "output"
        ));
    }

    #[test]
    fn terminal_precedence_reports_truncation_and_unknown_remote_state() {
        let truncated = command_result(
            TransportCommandOutput {
                stdout: b"partial".to_vec(),
                stderr: Vec::new(),
                exit_status: Some(0),
                stdout_truncated: true,
                stderr_truncated: false,
            },
            false,
        )
        .unwrap();
        assert_eq!(
            truncated.terminal,
            CommandTerminal::OutputTruncated { status: Some(0) }
        );
        let unknown = command_result(
            TransportCommandOutput {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit_status: None,
                stdout_truncated: false,
                stderr_truncated: false,
            },
            false,
        )
        .unwrap();
        assert_eq!(unknown.terminal, CommandTerminal::RemoteStateUnknown);
    }
}
