//! GUI 顶部通知的短生命周期状态与绘制。

use std::{
    collections::{HashSet, VecDeque},
    time::Duration,
};

use eframe::egui::{self, Align, Color32, Frame, Id, Layout, Order, RichText};

pub(crate) const WARNING_TTL_SECONDS: f64 = 8.0;
const NOTIFICATION_MARGIN: f32 = 12.0;
const NOTIFICATION_GAP: f32 = 6.0;
const NOTIFICATION_WIDTH: f32 = 560.0;

/// 通知绑定的生命周期域；清理域时连同去重 tombstone 一并回收。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum NotificationScope {
    ImageGeneration(u64),
    LoadAttempt(u64),
}

/// 强类型通知键，避免格式化字符串参与业务去重。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum NotificationKey {
    RawRange { generation: u64 },
    RawLoadFailed { attempt: u64 },
    ColorRenderFailed { generation: u64, revision: u64 },
}

impl NotificationKey {
    const fn scope(&self) -> NotificationScope {
        match *self {
            Self::RawRange { generation } | Self::ColorRenderFailed { generation, .. } => {
                NotificationScope::ImageGeneration(generation)
            }
            Self::RawLoadFailed { attempt } => NotificationScope::LoadAttempt(attempt),
        }
    }
}

/// UI 可见性策略；领域诊断和日志不依赖此状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NotificationSeverity {
    Warning,
    Error,
}

impl NotificationSeverity {
    const fn color(self) -> Color32 {
        match self {
            Self::Warning => Color32::YELLOW,
            Self::Error => Color32::from_rgb(255, 110, 110),
        }
    }

    const fn frame_fill(self) -> Color32 {
        match self {
            Self::Warning => Color32::from_rgba_premultiplied(80, 45, 0, 245),
            Self::Error => Color32::from_rgba_premultiplied(95, 20, 20, 245),
        }
    }
}

/// 一条短生命周期通知；时间使用 egui 的单调秒数。
#[derive(Clone, Debug)]
pub(crate) struct UiNotification {
    key: NotificationKey,
    severity: NotificationSeverity,
    title: String,
    detail: String,
    expires_at: Option<f64>,
}

impl UiNotification {
    pub(crate) fn raw_range(
        generation: u64,
        bit_depth: u8,
        max_code: u16,
        out_of_range_pixels: u64,
        observed_max: u16,
        first_position: (u32, u32, u16),
        now: f64,
    ) -> Self {
        Self {
            key: NotificationKey::RawRange { generation },
            severity: NotificationSeverity::Warning,
            title: "RAW range warning".to_owned(),
            detail: format!(
                "{out_of_range_pixels} pixels exceed {bit_depth}-bit max {max_code}; observed max {observed_max}; first x={} y={} raw={}. Magenta preserves original samples.",
                first_position.0, first_position.1, first_position.2
            ),
            expires_at: Some(now + WARNING_TTL_SECONDS),
        }
    }

    pub(crate) fn error(
        key: NotificationKey,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            key,
            severity: NotificationSeverity::Error,
            title: title.into(),
            detail: detail.into(),
            expires_at: None,
        }
    }
}

/// 可见通知与去重 tombstone 分离，关闭/超时不能让同一 key 重现。
#[derive(Debug, Default)]
pub(crate) struct NotificationCenter {
    visible: VecDeque<UiNotification>,
    seen: HashSet<NotificationKey>,
}

impl NotificationCenter {
    /// 仅首次接收某 key 时创建可见通知，并返回是否成功创建。
    pub(crate) fn push_once(&mut self, notification: UiNotification) -> bool {
        if !self.seen.insert(notification.key.clone()) {
            return false;
        }
        self.visible.push_back(notification);
        true
    }

    /// 只移除可见项；保留 tombstone 以避免同 key 在下一帧复现。
    pub(crate) fn dismiss(&mut self, key: &NotificationKey) {
        self.visible.retain(|notification| notification.key != *key);
    }

    /// 到期项不再绘制，但其 key 继续保留为 tombstone。
    pub(crate) fn prune_expired(&mut self, now: f64) {
        self.visible.retain(|notification| {
            notification
                .expires_at
                .is_none_or(|expires_at| now < expires_at)
        });
    }

    /// 回收已结束 scope 的可见项和 tombstone，避免长会话累积无界 key。
    pub(crate) fn clear_scope(&mut self, scope: NotificationScope) {
        self.visible
            .retain(|notification| notification.key.scope() != scope);
        self.seen.retain(|key| key.scope() != scope);
    }

    pub(crate) fn next_expiry(&self, now: f64) -> Option<Duration> {
        self.visible
            .iter()
            .filter_map(|notification| notification.expires_at)
            .map(|expires_at| Duration::from_secs_f64((expires_at - now).max(0.0)))
            .min()
    }

    /// 渲染通知并调度下一个到期帧，保证无输入时也能按时消失。
    pub(crate) fn render(&mut self, context: &egui::Context, viewer_rect: egui::Rect, now: f64) {
        self.prune_expired(now);
        if self.visible.is_empty() {
            return;
        }
        if let Some(remaining) = self.next_expiry(now) {
            context.request_repaint_after(remaining);
        }

        let mut dismissed = Vec::new();
        egui::Area::new(Id::new("notifications"))
            .movable(false)
            .sense(egui::Sense::hover())
            .order(Order::Foreground)
            .fixed_pos(viewer_rect.min + egui::vec2(NOTIFICATION_MARGIN, NOTIFICATION_MARGIN))
            .show(context, |ui| {
                ui.set_width(
                    (viewer_rect.width() - 2.0 * NOTIFICATION_MARGIN)
                        .clamp(1.0, NOTIFICATION_WIDTH),
                );
                ui.vertical(|ui| {
                    for notification in &self.visible {
                        let key = notification.key.clone();
                        let close_requested = Frame::popup(ui.style())
                            .fill(notification.severity.frame_fill())
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.vertical(|ui| {
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(&notification.title)
                                                    .strong()
                                                    .color(notification.severity.color()),
                                            )
                                            .selectable(false),
                                        );
                                        ui.add(
                                            egui::Label::new(&notification.detail)
                                                .wrap_mode(egui::TextWrapMode::Wrap)
                                                .selectable(false),
                                        );
                                    });
                                    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                                        ui.button("×").on_hover_text("Dismiss notification")
                                    })
                                    .inner
                                })
                                .inner
                                .clicked()
                            })
                            .inner;
                        if close_requested {
                            dismissed.push(key);
                        }
                        ui.add_space(NOTIFICATION_GAP);
                    }
                });
            });
        for key in dismissed {
            self.dismiss(&key);
        }
    }

    #[cfg(test)]
    fn visible_count(&self) -> usize {
        self.visible.len()
    }

    #[cfg(test)]
    fn seen_count(&self) -> usize {
        self.seen.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warning(generation: u64, now: f64) -> UiNotification {
        UiNotification::raw_range(generation, 10, 1023, 1, 1024, (0, 0, 1024), now)
    }

    #[test]
    fn warning_expires_without_removing_tombstone() {
        let mut center = NotificationCenter::default();
        assert!(center.push_once(warning(7, 10.0)));
        center.prune_expired(10.0 + WARNING_TTL_SECONDS);

        assert_eq!(center.visible_count(), 0);
        assert_eq!(center.seen_count(), 1);
        assert!(!center.push_once(warning(7, 100.0)));
    }

    #[test]
    fn dismissal_keeps_tombstone() {
        let mut center = NotificationCenter::default();
        let notification = warning(7, 0.0);
        let key = notification.key.clone();
        assert!(center.push_once(notification));
        center.dismiss(&key);

        assert_eq!(center.visible_count(), 0);
        assert_eq!(center.seen_count(), 1);
        assert!(!center.push_once(warning(7, 0.0)));
    }

    #[test]
    fn replacing_image_scope_allows_new_generation() {
        let mut center = NotificationCenter::default();
        assert!(center.push_once(warning(7, 0.0)));
        center.clear_scope(NotificationScope::ImageGeneration(7));

        assert_eq!(center.visible_count(), 0);
        assert_eq!(center.seen_count(), 0);
        assert!(center.push_once(warning(8, 0.0)));
    }

    #[test]
    fn error_stays_visible_until_dismissed() {
        let mut center = NotificationCenter::default();
        let key = NotificationKey::RawLoadFailed { attempt: 3 };
        assert!(center.push_once(UiNotification::error(
            key.clone(),
            "Load failed",
            "missing.raw",
        )));
        center.prune_expired(1_000_000.0);

        assert_eq!(center.visible_count(), 1);
        center.dismiss(&key);
        assert_eq!(center.visible_count(), 0);
    }

    #[test]
    fn next_expiry_uses_earliest_visible_warning() {
        let mut center = NotificationCenter::default();
        assert!(center.push_once(warning(1, 10.0)));
        assert!(center.push_once(warning(2, 12.0)));

        assert_eq!(center.next_expiry(14.0), Some(Duration::from_secs(4)));
    }
}
