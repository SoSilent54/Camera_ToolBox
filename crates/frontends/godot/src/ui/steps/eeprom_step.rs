//! Step 3 内的 EEPROM 写入区：双路独立 SNID 草稿 + 固定 I²C bus + 强确认写入。

use camera_toolbox_core::{YgStereoModuleCode, YgStereoSerialIdInput};
use godot::classes::{
    Button, Control, GridContainer, HBoxContainer, Label, LineEdit, OptionButton, VBoxContainer,
};
use godot::prelude::*;

use crate::ui::theme;

/// 单路 YgStereo SNID 草稿控件。
pub struct SnidDraftUi {
    pub module: Gd<OptionButton>,
    pub year: Gd<LineEdit>,
    pub month: Gd<LineEdit>,
    pub day: Gd<LineEdit>,
    pub optical_axis_class: Gd<OptionButton>,
    pub sequence: Gd<LineEdit>,
    pub preview: Gd<Label>,
}

impl SnidDraftUi {
    /// 从当前 UI 字段生成 14-byte YgStereo SNID。
    pub fn serial_number(&self) -> Result<String, String> {
        let module = match self.module.get_selected_id() {
            235 => YgStereoModuleCode::Model235,
            _ => YgStereoModuleCode::Model233,
        };
        let input = YgStereoSerialIdInput::new(
            module,
            parse_two_digit_year(&self.year.get_text().to_string())?,
            parse_decimal_field("月份", &self.month.get_text().to_string())?,
            parse_decimal_field("日期", &self.day.get_text().to_string())?,
            self.optical_axis_class
                .get_selected_id()
                .try_into()
                .unwrap_or(0),
            parse_decimal_field("序列号", &self.sequence.get_text().to_string())?,
        );
        input.serial_number().map_err(|error| error.to_string())
    }

    /// 刷新 SNID 预览标签；错误直接显示，防止自由输入非法 SN。
    pub fn refresh_preview(&mut self) -> Result<String, String> {
        match self.serial_number() {
            Ok(serial) => {
                self.preview.set_text(&format!("预览 SNID：{serial}"));
                self.preview
                    .add_theme_color_override("font_color", theme::OK);
                Ok(serial)
            }
            Err(error) => {
                self.preview.set_text(&format!("SNID 未完成：{error}"));
                self.preview
                    .add_theme_color_override("font_color", theme::WARN);
                Err(error)
            }
        }
    }

    /// Reset 只清序列号；型号、日期、光轴等级保留，适合连续生产批次。
    pub fn clear_sequence(&mut self) {
        self.sequence.clear();
        let _ = self.refresh_preview();
    }
}

/// Step 3 的 EEPROM 控件句柄。
pub struct EepromStep {
    pub panel: Gd<Control>,
    pub inspect_button: Gd<Button>,
    pub write_button: Gd<Button>,
    pub reset_button: Gd<Button>,
    pub ch0_snid: SnidDraftUi,
    pub ch3_snid: SnidDraftUi,
    pub status: Gd<Label>,
}

impl EepromStep {
    /// 构建 EEPROM 面板。
    pub fn build() -> Self {
        let mut v = VBoxContainer::new_alloc();
        v.add_theme_constant_override("separation", 10);

        let mut mapping = Label::new_alloc();
        mapping.set_text("EEPROM 写入目标：CH0 → i2c-4（左路） · CH3 → i2c-6（右路）；两路 SNID 独立生成、独立写入。");
        mapping.add_theme_font_size_override("font_size", 14);
        mapping.add_theme_color_override("font_color", theme::MUTED);
        v.add_child(&mapping);

        let ch0_snid = build_snid_editor("CH0 / i2c-4 SNID");
        let ch3_snid = build_snid_editor("CH3 / i2c-6 SNID");
        v.add_child(&ch0_snid.0);
        v.add_child(&ch3_snid.0);

        let mut row = HBoxContainer::new_alloc();
        row.add_theme_constant_override("separation", 8);
        let mut inspect_button = Button::new_alloc();
        inspect_button.set_text("读取当前状态");
        let mut write_button = Button::new_alloc();
        write_button.set_text("写入标定结果");
        write_button.set_disabled(true);
        let mut reset_button = Button::new_alloc();
        reset_button.set_text("Reset / 下一组");
        row.add_child(&inspect_button);
        row.add_child(&write_button);
        row.add_child(&reset_button);
        v.add_child(&row);

        let mut status = Label::new_alloc();
        status.set_text("等待求解完成后自动读取 EEPROM。输入两路序列号后预览 SNID，再确认写入。");
        status.add_theme_font_size_override("font_size", 13);
        status.add_theme_color_override("font_color", theme::MUTED);
        status.set_autowrap_mode(godot::classes::text_server::AutowrapMode::WORD_SMART);
        v.add_child(&status);

        let panel: Gd<Control> = v.upcast();
        let mut out = Self {
            panel,
            inspect_button,
            write_button,
            reset_button,
            ch0_snid: ch0_snid.1,
            ch3_snid: ch3_snid.1,
            status,
        };
        out.refresh_snid_previews();
        out
    }

    /// 写状态文本；成功绿、失败红。
    pub fn set_status(&mut self, text: &str, ok: bool) {
        self.status.set_text(text);
        self.status
            .add_theme_color_override("font_color", if ok { theme::OK } else { theme::ERR });
    }

    /// 生成双路 SNID；两路必须各自合法且不同。
    pub fn serial_pair(&mut self) -> Result<(String, String), String> {
        let ch0 = self
            .ch0_snid
            .refresh_preview()
            .map_err(|error| format!("CH0 SNID：{error}"))?;
        let ch3 = self
            .ch3_snid
            .refresh_preview()
            .map_err(|error| format!("CH3 SNID：{error}"))?;
        if ch0 == ch3 {
            return Err("CH0 与 CH3 SNID 不能相同；两颗 EEPROM 需要不同序列号".to_owned());
        }
        Ok((ch0, ch3))
    }

    pub fn refresh_snid_previews(&mut self) {
        let _ = self.ch0_snid.refresh_preview();
        let _ = self.ch3_snid.refresh_preview();
    }

    pub fn reset_for_next_unit(&mut self) {
        self.ch0_snid.clear_sequence();
        self.ch3_snid.clear_sequence();
        self.write_button.set_text("写入标定结果");
        self.write_button.set_disabled(true);
        self.set_status("已 Reset：保留设备 IP/SSH 与型号/日期/光轴等级；已清空两路序列号、dataset 与标定结果。", true);
    }
}

fn build_snid_editor(title: &str) -> (Gd<Control>, SnidDraftUi) {
    let mut box_root = VBoxContainer::new_alloc();
    box_root.add_theme_constant_override("separation", 6);

    let mut title_label = Label::new_alloc();
    title_label.set_text(title);
    title_label.add_theme_font_size_override("font_size", 14);
    title_label.add_theme_color_override("font_color", theme::ACCENT);
    box_root.add_child(&title_label);

    let mut grid = GridContainer::new_alloc();
    grid.set_columns(12);
    grid.add_theme_constant_override("h_separation", 6);
    grid.add_theme_constant_override("v_separation", 4);

    let mut module = OptionButton::new_alloc();
    module.add_item_ex("233").id(233).done();
    module.add_item_ex("235").id(235).done();
    module.select(0);

    let year = line_edit("26", 54.0);
    let month = line_edit("1-12", 54.0);
    let day = line_edit("1-31", 54.0);

    let mut axis = OptionButton::new_alloc();
    for (id, label) in [
        (0, "0 - 未分类"),
        (1, "1 - L0"),
        (2, "2 - L1"),
        (3, "3 - R0"),
        (4, "4 - R1"),
    ] {
        axis.add_item_ex(label).id(id).done();
    }
    axis.select(0);

    let mut sequence = line_edit("1-3844", 82.0);
    sequence.set_text("1");

    for (label, node) in [
        ("型号", module.clone().upcast::<Control>()),
        ("年", year.clone().upcast::<Control>()),
        ("月", month.clone().upcast::<Control>()),
        ("日", day.clone().upcast::<Control>()),
        ("光轴", axis.clone().upcast::<Control>()),
        ("序列号", sequence.clone().upcast::<Control>()),
    ] {
        let mut l = Label::new_alloc();
        l.set_text(label);
        l.add_theme_font_size_override("font_size", 12);
        l.add_theme_color_override("font_color", theme::MUTED);
        grid.add_child(&l);
        grid.add_child(&node);
    }
    box_root.add_child(&grid);

    let mut preview = Label::new_alloc();
    preview.add_theme_font_size_override("font_size", 13);
    preview.add_theme_color_override("font_color", theme::WARN);
    box_root.add_child(&preview);

    (
        box_root.upcast(),
        SnidDraftUi {
            module,
            year,
            month,
            day,
            optical_axis_class: axis,
            sequence,
            preview,
        },
    )
}

fn line_edit(placeholder: &str, width: f32) -> Gd<LineEdit> {
    let mut input = LineEdit::new_alloc();
    input.set_placeholder(placeholder);
    input.set_custom_minimum_size(Vector2::new(width, 0.0));
    input
}

fn parse_two_digit_year(text: &str) -> Result<u16, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("年份必填".to_owned());
    }
    if trimmed.len() != 2 || !trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("年份必须是两位数字，例如 26".to_owned());
    }
    trimmed
        .parse::<u16>()
        .map_err(|_| "年份必须是两位数字，例如 26".to_owned())
}

fn parse_decimal_field<T>(label: &str, text: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(format!("{label}必填"));
    }
    trimmed
        .parse::<T>()
        .map_err(|_| format!("{label}必须是十进制数字"))
}
