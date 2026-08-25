//! 目录快照差异和启发式发布就绪状态机。

use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};

use super::{DirectoryRef, FileEntry, FileKind, FileRef, FileVersion, PublicationReady};
use crate::filesystem::CompletionEvidence;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryDeltaCause {
    NativeHint,
    Reconciliation,
    RemotePoll,
    FileOperation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectoryChange {
    Created(FileEntry),
    Modified { before: FileEntry, after: FileEntry },
    Removed(FileEntry),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryDelta {
    pub directory: DirectoryRef,
    pub sequence: u64,
    pub cause: DirectoryDeltaCause,
    pub changes: Vec<DirectoryChange>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirectorySnapshot {
    entries: BTreeMap<String, FileEntry>,
}

impl DirectorySnapshot {
    #[must_use]
    pub fn new(entries: impl IntoIterator<Item = FileEntry>) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(|entry| (entry.name.as_str().to_owned(), entry))
                .collect(),
        }
    }

    #[must_use]
    pub fn entries(&self) -> impl Iterator<Item = &FileEntry> {
        self.entries.values()
    }

    #[must_use]
    pub fn diff(&self, next: &Self) -> Vec<DirectoryChange> {
        let mut changes = Vec::new();
        for (name, before) in &self.entries {
            match next.entries.get(name) {
                None => changes.push(DirectoryChange::Removed(before.clone())),
                Some(after) if before != after => changes.push(DirectoryChange::Modified {
                    before: before.clone(),
                    after: after.clone(),
                }),
                Some(_) => {}
            }
        }
        for (name, after) in &next.entries {
            if !self.entries.contains_key(name) {
                changes.push(DirectoryChange::Created(after.clone()));
            }
        }
        changes
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicationTransition {
    Ready(PublicationReady),
    Superseded {
        file: FileRef,
        previous: FileVersion,
        current: FileVersion,
    },
    Disappeared {
        file: FileRef,
        previous: FileVersion,
    },
}

#[derive(Debug, Clone, Copy)]
struct Observation {
    version: FileVersion,
    stable_count: u8,
    emitted: bool,
}

/// 普通 stat 轮询只能产生 `SettledHeuristic`，绝不升级为生产者确认。
#[derive(Debug)]
pub struct PublicationTracker {
    required_observations: u8,
    interval: Duration,
    max_pending: usize,
    observations: BTreeMap<FileRef, Observation>,
    order: VecDeque<FileRef>,
}

impl PublicationTracker {
    #[must_use]
    pub fn new(required_observations: u8, interval: Duration, max_pending: usize) -> Self {
        Self {
            required_observations: required_observations.max(2),
            interval,
            max_pending: max_pending.max(1),
            observations: BTreeMap::new(),
            order: VecDeque::new(),
        }
    }

    /// 初次连接只建立 baseline；既有文件不会自动打开。baseline 不占 pending 配额。
    pub fn install_baseline(&mut self, snapshot: &DirectorySnapshot) {
        self.observations.clear();
        self.order.clear();
        for entry in snapshot
            .entries()
            .filter(|entry| entry.kind == FileKind::File)
        {
            self.observations.insert(
                entry.reference.clone(),
                Observation {
                    version: entry.version,
                    stable_count: self.required_observations,
                    emitted: true,
                },
            );
        }
    }

    /// 输入一次完整目录快照，输出本次可见的就绪/失效转换。
    #[must_use]
    pub fn observe_snapshot(&mut self, snapshot: &DirectorySnapshot) -> Vec<PublicationTransition> {
        let current: BTreeMap<_, _> = snapshot
            .entries()
            .filter(|entry| entry.kind == FileKind::File)
            .map(|entry| (entry.reference.clone(), entry.version))
            .collect();
        let mut transitions = Vec::new();

        let removed: Vec<_> = self
            .observations
            .keys()
            .filter(|file| !current.contains_key(*file))
            .cloned()
            .collect();
        for file in removed {
            if let Some(previous) = self.observations.remove(&file) {
                self.order.retain(|candidate| candidate != &file);
                if previous.emitted {
                    transitions.push(PublicationTransition::Disappeared {
                        file,
                        previous: previous.version,
                    });
                }
            }
        }

        for (file, version) in current {
            let mut became_pending = false;
            let mut became_ready = false;
            if let Some(observation) = self.observations.get_mut(&file) {
                if observation.version == version {
                    observation.stable_count = observation.stable_count.saturating_add(1);
                } else {
                    if observation.emitted {
                        transitions.push(PublicationTransition::Superseded {
                            file: file.clone(),
                            previous: observation.version,
                            current: version,
                        });
                    }
                    *observation = Observation {
                        version,
                        stable_count: 1,
                        emitted: false,
                    };
                    became_pending = true;
                }
                if !observation.emitted && observation.stable_count >= self.required_observations {
                    observation.emitted = true;
                    became_ready = true;
                }
            } else {
                self.observations.insert(
                    file.clone(),
                    Observation {
                        version,
                        stable_count: 1,
                        emitted: false,
                    },
                );
                became_pending = true;
            }

            if became_pending {
                self.order.retain(|candidate| candidate != &file);
                self.order.push_back(file.clone());
                self.enforce_pending_bound();
            }
            if became_ready {
                self.order.retain(|candidate| candidate != &file);
                transitions.push(PublicationTransition::Ready(PublicationReady {
                    file,
                    version,
                    evidence: CompletionEvidence::SettledHeuristic {
                        observations: self.required_observations,
                        interval: self.interval,
                    },
                }));
            }
        }
        transitions
    }

    fn enforce_pending_bound(&mut self) {
        while self.order.len() > self.max_pending {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if self
                .observations
                .get(&oldest)
                .is_some_and(|observation| !observation.emitted)
            {
                self.observations.remove(&oldest);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::{EntryName, FileSourceId, SourcePath};

    fn entry(name: &str, size: u64) -> FileEntry {
        let source_id = FileSourceId::new("remote").unwrap();
        FileEntry {
            reference: FileRef::new(source_id, SourcePath::new(name).unwrap()),
            name: EntryName::new(name).unwrap(),
            kind: FileKind::File,
            version: FileVersion {
                size,
                modified_millis: Some(size),
            },
        }
    }

    #[test]
    fn snapshot_diff_reports_create_modify_remove() {
        let before = DirectorySnapshot::new([entry("old.raw", 2), entry("same.raw", 4)]);
        let after = DirectorySnapshot::new([entry("new.raw", 2), entry("same.raw", 8)]);
        let changes = before.diff(&after);
        assert_eq!(changes.len(), 3);
        assert!(
            changes
                .iter()
                .any(|change| matches!(change, DirectoryChange::Created(_)))
        );
        assert!(
            changes
                .iter()
                .any(|change| matches!(change, DirectoryChange::Modified { .. }))
        );
        assert!(
            changes
                .iter()
                .any(|change| matches!(change, DirectoryChange::Removed(_)))
        );
    }

    #[test]
    fn baseline_is_ignored_and_later_growth_is_superseded() {
        let mut tracker = PublicationTracker::new(2, Duration::from_secs(1), 8);
        tracker.install_baseline(&DirectorySnapshot::new([entry("frame.raw", 2)]));
        assert!(
            tracker
                .observe_snapshot(&DirectorySnapshot::new([entry("frame.raw", 2)]))
                .is_empty()
        );
        let transitions =
            tracker.observe_snapshot(&DirectorySnapshot::new([entry("frame.raw", 4)]));
        assert!(matches!(
            transitions.as_slice(),
            [PublicationTransition::Superseded { .. }]
        ));
        let transitions =
            tracker.observe_snapshot(&DirectorySnapshot::new([entry("frame.raw", 4)]));
        assert!(matches!(
            transitions.as_slice(),
            [PublicationTransition::Ready(PublicationReady {
                evidence: CompletionEvidence::SettledHeuristic { .. },
                ..
            })]
        ));
    }

    #[test]
    fn baseline_larger_than_pending_limit_never_reopens_existing_files() {
        let snapshot =
            DirectorySnapshot::new([entry("a.raw", 1), entry("b.raw", 1), entry("c.raw", 1)]);
        let mut tracker = PublicationTracker::new(2, Duration::from_secs(1), 1);
        tracker.install_baseline(&snapshot);
        assert!(tracker.observe_snapshot(&snapshot).is_empty());
        assert!(tracker.observe_snapshot(&snapshot).is_empty());
        assert_eq!(tracker.observations.len(), 3);
    }

    #[test]
    fn pending_candidates_are_bounded() {
        let mut tracker = PublicationTracker::new(2, Duration::from_secs(1), 2);
        tracker.observe_snapshot(&DirectorySnapshot::new([
            entry("a.raw", 1),
            entry("b.raw", 1),
            entry("c.raw", 1),
        ]));
        assert_eq!(tracker.observations.len(), 2);
    }
}
