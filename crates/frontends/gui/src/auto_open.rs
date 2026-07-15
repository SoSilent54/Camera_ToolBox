//! 文件工作区自动打开：目录监控只产出候选，真正解码仍由 app 调度与限流。

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use camera_toolbox_adapters::filesystem::{
    DirectoryMonitorConfig, DirectoryMonitorEvent, DirectoryMonitorHandle, DirectoryMonitorState,
    LocalDirectoryMonitor, RemoteDirectoryMonitor,
};
use camera_toolbox_app::{
    AutoOpenActivation, AutoOpenRule, AutoOpenRuleId, CompletionRequirement, DirectoryChange,
    DirectorySnapshot, FileEntry, FileKind, FileRef, MountedSourceConfig, PublicationTracker,
    PublicationTransition, WorkspaceSettings,
};

use crate::explorer::{ExplorerState, MountBinding};

const AUTO_OPEN_REQUIRED_OBSERVATIONS: u8 = 2;
const AUTO_OPEN_MAX_PENDING_CANDIDATES: usize = 64;

#[derive(Debug, Clone)]
pub(crate) struct AutoOpenCandidate {
    pub(crate) rule_id: AutoOpenRuleId,
    pub(crate) activation: AutoOpenActivation,
    pub(crate) reference: FileRef,
}

struct AutoOpenRuntime {
    rule: AutoOpenRule,
    snapshot: Option<DirectorySnapshot>,
    tracker: PublicationTracker,
    monitor: DirectoryMonitorHandle,
}

#[derive(Default)]
pub(crate) struct AutoOpenCoordinator {
    runtimes: Vec<AutoOpenRuntime>,
}

impl AutoOpenCoordinator {
    #[must_use]
    pub(crate) fn from_settings(settings: &WorkspaceSettings, explorer: &ExplorerState) -> Self {
        let config = DirectoryMonitorConfig::default();
        let runtimes = settings
            .auto_open_rules()
            .iter()
            .filter(|rule| rule.enabled)
            .filter_map(|rule| build_runtime(rule.clone(), explorer, &config))
            .collect();
        Self { runtimes }
    }

    pub(crate) fn poll(&mut self) -> Vec<AutoOpenCandidate> {
        let mut ready = Vec::new();
        for runtime in &mut self.runtimes {
            while let Some(event) = runtime.monitor.try_recv() {
                match event {
                    DirectoryMonitorEvent::Baseline(snapshot) => {
                        let filtered = filtered_snapshot(&runtime.rule, &snapshot);
                        runtime.tracker.install_baseline(&filtered);
                        runtime.snapshot = Some(filtered);
                    }
                    DirectoryMonitorEvent::Delta(delta) => {
                        let Some(snapshot) = runtime.snapshot.as_ref() else {
                            continue;
                        };
                        let changes = filtered_changes(&runtime.rule, &delta.changes);
                        if changes.is_empty() {
                            continue;
                        }
                        let next_snapshot = apply_filtered_changes(snapshot, &changes);
                        for transition in runtime.tracker.observe_snapshot(&next_snapshot) {
                            if let PublicationTransition::Ready(publication) = transition {
                                if publication
                                    .evidence
                                    .satisfies(runtime.rule.minimum_completion)
                                    && runtime.rule.matcher.matches(&publication.file)
                                {
                                    ready.push(AutoOpenCandidate {
                                        rule_id: runtime.rule.id.clone(),
                                        activation: runtime.rule.activation,
                                        reference: publication.file,
                                    });
                                }
                            }
                        }
                        runtime.snapshot = Some(next_snapshot);
                    }
                    DirectoryMonitorEvent::State(DirectoryMonitorState::Stale) => {
                        tracing::warn!(
                            operation = "auto_open_monitor_state",
                            rule_id = runtime.rule.id.as_str(),
                            directory = runtime.rule.directory.path.as_str(),
                            "auto-open monitor became stale"
                        );
                    }
                    DirectoryMonitorEvent::State(DirectoryMonitorState::Stopped) => {
                        tracing::debug!(
                            operation = "auto_open_monitor_state",
                            rule_id = runtime.rule.id.as_str(),
                            directory = runtime.rule.directory.path.as_str(),
                            "auto-open monitor stopped"
                        );
                    }
                    DirectoryMonitorEvent::State(DirectoryMonitorState::Connected) => {}
                    DirectoryMonitorEvent::Error(error) => {
                        tracing::warn!(
                            operation = "auto_open_monitor_error",
                            rule_id = runtime.rule.id.as_str(),
                            directory = runtime.rule.directory.path.as_str(),
                            error = %error,
                            "auto-open monitor reported an error"
                        );
                    }
                }
            }
        }
        if ready.len() > AUTO_OPEN_MAX_PENDING_CANDIDATES {
            ready.truncate(AUTO_OPEN_MAX_PENDING_CANDIDATES);
        }
        ready
    }
}

fn build_runtime(
    rule: AutoOpenRule,
    explorer: &ExplorerState,
    config: &DirectoryMonitorConfig,
) -> Option<AutoOpenRuntime> {
    if rule.recursive {
        tracing::warn!(
            operation = "auto_open_rule_skip",
            rule_id = rule.id.as_str(),
            directory = rule.directory.path.as_str(),
            "recursive auto-open rules are not implemented yet; skipping"
        );
        return None;
    }
    if rule.minimum_completion == CompletionRequirement::ConfirmedOnly {
        tracing::warn!(
            operation = "auto_open_rule_skip",
            rule_id = rule.id.as_str(),
            directory = rule.directory.path.as_str(),
            "confirmed-only auto-open is not wired in GUI runtime yet; skipping"
        );
        return None;
    }
    let Some(binding) = explorer.mount_binding(&rule.directory.source_id) else {
        tracing::warn!(
            operation = "auto_open_rule_skip",
            rule_id = rule.id.as_str(),
            source_id = %rule.directory.source_id,
            "auto-open rule mount is not currently available; skipping"
        );
        return None;
    };
    let monitor = spawn_monitor(&rule, binding, config);
    Some(AutoOpenRuntime {
        tracker: PublicationTracker::new(
            AUTO_OPEN_REQUIRED_OBSERVATIONS,
            config.poll_interval,
            AUTO_OPEN_MAX_PENDING_CANDIDATES,
        ),
        rule,
        snapshot: None,
        monitor,
    })
}

fn spawn_monitor(
    rule: &AutoOpenRule,
    binding: MountBinding,
    config: &DirectoryMonitorConfig,
) -> DirectoryMonitorHandle {
    match binding.config {
        MountedSourceConfig::Local { root, .. } => LocalDirectoryMonitor::spawn(
            Arc::clone(&binding.file_system),
            rule.directory.clone(),
            local_directory_path(&root, &rule.directory.path),
            config.clone(),
        ),
        MountedSourceConfig::Sftp { .. } => RemoteDirectoryMonitor::spawn(
            Arc::clone(&binding.file_system),
            rule.directory.clone(),
            config.clone(),
        ),
    }
}

fn local_directory_path(root: &std::path::Path, path: &camera_toolbox_app::SourcePath) -> PathBuf {
    if path.is_root() {
        root.to_path_buf()
    } else {
        root.join(path.as_str())
    }
}

fn filtered_snapshot(rule: &AutoOpenRule, snapshot: &DirectorySnapshot) -> DirectorySnapshot {
    DirectorySnapshot::new(
        snapshot
            .entries()
            .filter(|entry| entry_matches_rule(rule, entry))
            .cloned(),
    )
}

fn filtered_changes(rule: &AutoOpenRule, changes: &[DirectoryChange]) -> Vec<DirectoryChange> {
    changes
        .iter()
        .filter_map(|change| filter_change(rule, change))
        .collect()
}

fn apply_filtered_changes(
    snapshot: &DirectorySnapshot,
    changes: &[DirectoryChange],
) -> DirectorySnapshot {
    let mut entries: BTreeMap<String, FileEntry> = snapshot
        .entries()
        .cloned()
        .map(|entry| (entry.name.as_str().to_owned(), entry))
        .collect();
    for change in changes {
        match change {
            DirectoryChange::Created(after) => {
                entries.insert(after.name.as_str().to_owned(), after.clone());
            }
            DirectoryChange::Modified { before: _, after } => {
                entries.insert(after.name.as_str().to_owned(), after.clone());
            }
            DirectoryChange::Removed(before) => {
                entries.remove(before.name.as_str());
            }
        }
    }
    DirectorySnapshot::new(entries.into_values())
}

fn filter_change(rule: &AutoOpenRule, change: &DirectoryChange) -> Option<DirectoryChange> {
    match change {
        DirectoryChange::Created(after) if entry_matches_rule(rule, after) => {
            Some(DirectoryChange::Created(after.clone()))
        }
        DirectoryChange::Created(_) => None,
        DirectoryChange::Modified { before, after } => {
            let before_matches = entry_matches_rule(rule, before);
            let after_matches = entry_matches_rule(rule, after);
            match (before_matches, after_matches) {
                (true, true) => Some(DirectoryChange::Modified {
                    before: before.clone(),
                    after: after.clone(),
                }),
                (true, false) => Some(DirectoryChange::Removed(before.clone())),
                (false, true) => Some(DirectoryChange::Created(after.clone())),
                (false, false) => None,
            }
        }
        DirectoryChange::Removed(before) if entry_matches_rule(rule, before) => {
            Some(DirectoryChange::Removed(before.clone()))
        }
        DirectoryChange::Removed(_) => None,
    }
}

fn entry_matches_rule(rule: &AutoOpenRule, entry: &FileEntry) -> bool {
    entry.kind == FileKind::File && rule.matcher.matches(&entry.reference)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camera_toolbox_app::{
        AutoOpenActivation, AutoOpenRule, AutoOpenRuleId, CompletionRequirement, DirectoryRef,
        EntryName, FileMatcher, FileSourceId, FileVersion, SourcePath,
    };

    fn rule() -> AutoOpenRule {
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
            minimum_completion: CompletionRequirement::AllowSettledHeuristic,
            completion_contract: None,
        }
    }

    fn entry(name: &str, size: u64) -> FileEntry {
        let source = FileSourceId::new("local").unwrap();
        FileEntry {
            reference: FileRef::new(source, SourcePath::new(name).unwrap()),
            name: EntryName::new(name).unwrap(),
            kind: FileKind::File,
            version: FileVersion {
                size,
                modified_millis: Some(size),
            },
        }
    }

    #[test]
    fn filtered_changes_convert_match_boundary_transitions() {
        let rule = rule();
        let keep = entry("keep.raw", 1);
        let leave = entry("leave.txt", 2);
        let new_match = entry("become.raw", 3);
        let changes = filtered_changes(
            &rule,
            &[
                DirectoryChange::Modified {
                    before: keep.clone(),
                    after: leave.clone(),
                },
                DirectoryChange::Modified {
                    before: leave,
                    after: new_match.clone(),
                },
            ],
        );
        assert!(matches!(changes[0], DirectoryChange::Removed(ref before) if before == &keep));
        assert!(matches!(changes[1], DirectoryChange::Created(ref after) if after == &new_match));
    }

    #[test]
    fn filtered_snapshot_keeps_only_matching_files() {
        let rule = rule();
        let snapshot = DirectorySnapshot::new([
            entry("frame.raw", 1),
            entry("frame.txt", 1),
            FileEntry {
                reference: FileRef::new(
                    FileSourceId::new("local").unwrap(),
                    SourcePath::new("nested").unwrap(),
                ),
                name: EntryName::new("nested").unwrap(),
                kind: FileKind::Directory,
                version: FileVersion {
                    size: 0,
                    modified_millis: None,
                },
            },
        ]);
        let filtered = filtered_snapshot(&rule, &snapshot);
        let names: Vec<_> = filtered
            .entries()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, vec!["frame.raw"]);
    }

    #[test]
    fn apply_filtered_changes_updates_snapshot_contents() {
        let snapshot = DirectorySnapshot::new([entry("stay.raw", 1), entry("gone.raw", 2)]);
        let next = apply_filtered_changes(
            &snapshot,
            &[
                DirectoryChange::Modified {
                    before: entry("stay.raw", 1),
                    after: entry("stay.raw", 3),
                },
                DirectoryChange::Removed(entry("gone.raw", 2)),
                DirectoryChange::Created(entry("new.raw", 4)),
            ],
        );
        let names: Vec<_> = next.entries().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["new.raw", "stay.raw"]);
        assert_eq!(
            next.entries()
                .find(|entry| entry.name.as_str() == "stay.raw")
                .unwrap()
                .version
                .size,
            3
        );
    }
}
