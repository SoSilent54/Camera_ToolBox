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
        .map(|document| (document.last_access, document.id, false))
        .collect();
    candidates.extend(
        workspace
            .image_documents
            .iter()
            .filter(|document| Some(document.id) != active && document.derived_resource_bytes() > 0)
            .map(|document| (document.last_access, document.id, true)),
    );
    candidates.sort_unstable();

    for (_, id, is_image) in candidates {
        if total <= workspace.derived_budget_bytes {
            break;
        }
        let freed = if is_image {
            workspace
                .image_mut(id)
                .map_or(0, super::ImageDocument::evict_derived_resources)
        } else {
            workspace
                .document_mut(id)
                .map_or(0, super::RawDocument::evict_derived_resources)
        };
        total = total.saturating_sub(freed);
    }
}
