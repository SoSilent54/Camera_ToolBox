//! 目录发布证据与自动打开规则；不依赖 SSH 配置或 GUI 状态。

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{DirectoryRef, FileRef, FileVersion};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AutoOpenRuleId(String);

impl AutoOpenRuleId {
    /// # Errors
    ///
    /// 空白或控制字符标识会被拒绝。
    pub fn new(value: impl Into<String>) -> Result<Self, AutoOpenRuleError> {
        let value = value.into();
        if value.trim().is_empty() || value.chars().any(char::is_control) {
            return Err(AutoOpenRuleError::InvalidRuleId);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileMatcher {
    /// 精确文件名匹配，ASCII 大小写不敏感。
    #[serde(default)]
    pub exact_names: Vec<String>,
    /// 文件名后缀匹配，ASCII 大小写不敏感。
    #[serde(default)]
    pub suffixes: Vec<String>,
}

impl FileMatcher {
    #[must_use]
    pub fn matches(&self, file: &FileRef) -> bool {
        let Some(name) = file.path.file_name() else {
            return false;
        };
        (self.exact_names.is_empty() && self.suffixes.is_empty())
            || self
                .exact_names
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(name))
            || self.suffixes.iter().any(|suffix| {
                name.to_ascii_lowercase()
                    .ends_with(&suffix.to_ascii_lowercase())
            })
    }

    /// # Errors
    ///
    /// 精确名称或后缀含路径分隔符、NUL、`..` 或空白时会被拒绝。
    pub fn validate(&self) -> Result<(), AutoOpenRuleError> {
        for name in &self.exact_names {
            if name.trim().is_empty()
                || name.contains(['/', '\\', '\0'])
                || name == "."
                || name == ".."
                || name.chars().any(char::is_control)
            {
                return Err(AutoOpenRuleError::InvalidExactName(name.clone()));
            }
        }
        for suffix in &self.suffixes {
            validate_suffix(suffix)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoOpenActivation {
    FollowLatest,
    NewBackgroundTab,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionRequirement {
    AllowSettledHeuristic,
    ConfirmedOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntrySuffix(String);

impl EntrySuffix {
    /// # Errors
    ///
    /// 空后缀、路径语义、NUL 或控制字符会被拒绝。
    pub fn new(value: impl Into<String>) -> Result<Self, AutoOpenRuleError> {
        let value = value.into();
        validate_suffix(&value)?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn validate_suffix(suffix: &str) -> Result<(), AutoOpenRuleError> {
    if suffix.trim().is_empty()
        || suffix.contains(['/', '\\', '\0'])
        || suffix.contains("..")
        || suffix.chars().any(char::is_control)
    {
        return Err(AutoOpenRuleError::InvalidSuffix(suffix.to_owned()));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CompletionContract {
    AtomicFinalRename {
        temporary_suffix: EntrySuffix,
    },
    CompletionMarker {
        marker_suffix: EntrySuffix,
    },
    JsonSidecarV1 {
        sidecar_suffix: EntrySuffix,
        require_sha256: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoOpenRule {
    pub id: AutoOpenRuleId,
    pub directory: DirectoryRef,
    pub matcher: FileMatcher,
    pub recursive: bool,
    pub enabled: bool,
    pub activation: AutoOpenActivation,
    pub minimum_completion: CompletionRequirement,
    pub completion_contract: Option<CompletionContract>,
}

impl AutoOpenRule {
    /// # Errors
    ///
    /// `ConfirmedOnly` 缺少生产者契约、匹配器或契约后缀无效时返回错误。
    pub fn validate(&self) -> Result<(), AutoOpenRuleError> {
        self.matcher.validate()?;
        if self.minimum_completion == CompletionRequirement::ConfirmedOnly
            && self.completion_contract.is_none()
        {
            return Err(AutoOpenRuleError::ConfirmedWithoutContract);
        }
        if let Some(contract) = &self.completion_contract {
            let suffix = match contract {
                CompletionContract::AtomicFinalRename { temporary_suffix } => temporary_suffix,
                CompletionContract::CompletionMarker { marker_suffix } => marker_suffix,
                CompletionContract::JsonSidecarV1 { sidecar_suffix, .. } => sidecar_suffix,
            };
            validate_suffix(suffix.as_str())?;
            if self
                .matcher
                .suffixes
                .iter()
                .any(|matcher| matcher.eq_ignore_ascii_case(suffix.as_str()))
            {
                return Err(AutoOpenRuleError::AmbiguousCompanionSuffix(
                    suffix.as_str().to_owned(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionContractEvidence {
    pub contract: CompletionContract,
    pub companion: Option<FileRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionEvidence {
    ConfirmedComplete(CompletionContractEvidence),
    SettledHeuristic {
        observations: u8,
        interval: Duration,
    },
}

impl CompletionEvidence {
    #[must_use]
    pub const fn satisfies(&self, requirement: CompletionRequirement) -> bool {
        match requirement {
            CompletionRequirement::AllowSettledHeuristic => true,
            CompletionRequirement::ConfirmedOnly => {
                matches!(self, Self::ConfirmedComplete(_))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationReady {
    pub file: FileRef,
    pub version: FileVersion,
    pub evidence: CompletionEvidence,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AutoOpenRuleError {
    #[error("auto-open rule id must be non-empty and printable")]
    InvalidRuleId,
    #[error("invalid completion or matcher suffix: {0}")]
    InvalidSuffix(String),
    #[error("invalid exact file matcher: {0}")]
    InvalidExactName(String),
    #[error("confirmed-only auto-open requires a producer completion contract")]
    ConfirmedWithoutContract,
    #[error("completion suffix conflicts with a file matcher suffix: {0}")]
    AmbiguousCompanionSuffix(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::{FileSourceId, SourcePath};

    fn rule(requirement: CompletionRequirement) -> AutoOpenRule {
        AutoOpenRule {
            id: AutoOpenRuleId::new("latest").unwrap(),
            directory: DirectoryRef::root(FileSourceId::new("local").unwrap()),
            matcher: FileMatcher {
                exact_names: Vec::new(),
                suffixes: vec![".raw".to_owned()],
            },
            recursive: false,
            enabled: true,
            activation: AutoOpenActivation::FollowLatest,
            minimum_completion: requirement,
            completion_contract: None,
        }
    }

    #[test]
    fn confirmed_only_requires_typed_contract() {
        assert_eq!(
            rule(CompletionRequirement::ConfirmedOnly)
                .validate()
                .unwrap_err(),
            AutoOpenRuleError::ConfirmedWithoutContract
        );
        assert!(
            rule(CompletionRequirement::AllowSettledHeuristic)
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn companion_suffix_cannot_escape_directory() {
        assert!(EntrySuffix::new("../done").is_err());
        assert!(EntrySuffix::new("/done").is_err());
        assert_eq!(EntrySuffix::new(".done").unwrap().as_str(), ".done");
    }

    #[test]
    fn companion_suffix_conflict_is_case_insensitive() {
        let mut candidate = rule(CompletionRequirement::ConfirmedOnly);
        candidate.matcher.suffixes = vec![".RAW".to_owned()];
        candidate.completion_contract = Some(CompletionContract::CompletionMarker {
            marker_suffix: EntrySuffix::new(".raw").unwrap(),
        });
        assert!(matches!(
            candidate.validate(),
            Err(AutoOpenRuleError::AmbiguousCompanionSuffix(_))
        ));
    }

    #[test]
    fn matcher_is_case_insensitive() {
        let file = FileRef::new(
            FileSourceId::new("local").unwrap(),
            SourcePath::new("CAPTURE.RAW").unwrap(),
        );
        assert!(
            FileMatcher {
                exact_names: Vec::new(),
                suffixes: vec![".raw".into()],
            }
            .matches(&file)
        );
    }
}
