//! Inactive 文档的派生资源 LRU 驱逐。

use super::WorkspaceState;

pub(super) fn enforce_derived_budget(workspace: &mut WorkspaceState) {
    let mut total = workspace.total_derived_bytes();
    if total <= workspace.derived_budget_bytes {
        return;
    }

    let active = workspace.active;
    let mut candidates: Vec<_> = workspace
        .documents
        .iter()
        .filter(|document| Some(document.id) != active && document.derived_resource_bytes() > 0)
        .map(|document| (document.last_access, document.id))
        .collect();
    candidates.sort_unstable();

    for (_, id) in candidates {
        if total <= workspace.derived_budget_bytes {
            break;
        }
        if let Some(document) = workspace.document_mut(id) {
            total = total.saturating_sub(document.evict_derived_resources());
        }
    }
}
