use eframe::egui;

pub fn setup(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
    });
}

pub fn device_bar(ui: &mut egui::Ui, show_logs: &mut bool, controls: impl FnOnce(&mut egui::Ui)) {
    egui::Panel::top("device_bar").show(ui, |ui| {
        ui.add_space(5.0);
        ui.horizontal(|ui| {
            controls(ui);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.toggle_value(show_logs, "Logs");
            });
        });
        ui.add_space(5.0);
    });
}

const LOG_HEIGHT: f32 = 180.0;

pub fn script_section(ui: &mut egui::Ui, show_logs: bool, contents: impl FnOnce(&mut egui::Ui)) {
    let log_space = if show_logs { LOG_HEIGHT + 12.0 } else { 0.0 };
    let height = (ui.available_height() - log_space).clamp(120.0, 340.0);
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), height),
        egui::Layout::top_down(egui::Align::Min),
        contents,
    );
}

pub fn logs(ui: &mut egui::Ui, logs: &mut Vec<String>) {
    ui.separator();
    ui.horizontal_wrapped(|ui| {
        ui.heading("Log");
        if ui.button("Copy Logs").clicked() {
            ui.ctx().copy_text(logs.join("\n"));
            logs.push("Copied logs to clipboard".to_string());
        }
        if ui.button("Clear").clicked() {
            logs.clear();
        }
    });
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_min_height(LOG_HEIGHT);
        egui::ScrollArea::vertical()
            .id_salt("logs")
            .stick_to_bottom(true)
            .max_height(LOG_HEIGHT)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(logs.join("\n")).monospace())
                        .selectable(true)
                        .wrap(),
                );
            });
    });
}

fn app_row(ui: &mut egui::Ui, selected: bool, name: &str, bundle_id: &str) -> bool {
    let clicked = ui.selectable_label(selected, name).clicked();
    egui::Frame::new()
        .inner_margin(egui::Margin {
            left: ui.spacing().button_padding.x.round() as i8,
            ..egui::Margin::ZERO
        })
        .show(ui, |ui| ui.small(bundle_id));
    ui.add_space(4.0);
    clicked
}

pub fn list_heading(ui: &mut egui::Ui, title: &str, can_refresh: bool) -> bool {
    ui.horizontal(|ui| {
        ui.heading(title);
        ui.add_enabled(can_refresh, egui::Button::new("Refresh"))
            .clicked()
    })
    .inner
}

fn text_field(ui: &mut egui::Ui, label: &str, text: &mut String) -> bool {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.text_edit_singleline(text).changed()
    })
    .inner
}

pub fn app_list<'a>(
    ui: &mut egui::Ui,
    apps: &'a [crate::targets::AppInfo],
    selected: Option<&str>,
    filter: &mut String,
    max_height: f32,
) -> Option<&'a crate::targets::AppInfo> {
    ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
        ui.add(egui::Label::new("Only apps with get-task-allow are shown.").wrap());
    });
    text_field(ui, "Filter", filter);
    let filter = filter.trim().to_lowercase();
    egui::ScrollArea::vertical()
        .id_salt("apps")
        .max_height(max_height)
        .show(ui, |ui| {
            let mut chosen = None;
            for app in apps.iter().filter(|app| {
                app.name.to_lowercase().contains(&filter)
                    || app.bundle_id.to_lowercase().contains(&filter)
            }) {
                if app_row(
                    ui,
                    selected == Some(&app.bundle_id),
                    &app.name,
                    &app.bundle_id,
                ) {
                    chosen = Some(app);
                }
            }
            chosen
        })
        .inner
}

pub fn process_list<'a>(
    ui: &mut egui::Ui,
    processes: &'a [crate::targets::ProcessInfo],
    selected: Option<u32>,
    filter: &mut String,
    max_height: f32,
) -> Option<&'a crate::targets::ProcessInfo> {
    text_field(ui, "Filter", filter);
    let filter = filter.trim().to_lowercase();
    egui::ScrollArea::vertical()
        .id_salt("processes")
        .max_height(max_height)
        .show(ui, |ui| {
            let mut chosen = None;
            for process in processes.iter().filter(|process| {
                process.name.to_lowercase().contains(&filter)
                    || process.pid.to_string().contains(&filter)
            }) {
                if ui
                    .selectable_label(
                        selected == Some(process.pid),
                        format!("{} ({})", process.name, process.pid),
                    )
                    .clicked()
                {
                    chosen = Some(process);
                }
            }
            chosen
        })
        .inner
}

pub fn pid_field(ui: &mut egui::Ui, pid: &mut String) -> bool {
    text_field(ui, "PID", pid)
}

pub fn parse_pid(pid: &str) -> Option<u32> {
    pid.trim().parse().ok().filter(|pid| *pid != 0)
}

pub enum JitAction {
    Enable,
    RunScript,
    Stop,
}

fn jit_actions(
    ui: &mut egui::Ui,
    enable: Option<bool>,
    script: Option<bool>,
    stop: Option<bool>,
) -> Option<JitAction> {
    ui.horizontal_wrapped(|ui| {
        let enable = enable.is_some_and(|enabled| {
            ui.add_enabled(enabled, egui::Button::new("Enable JIT"))
                .clicked()
        });
        let script = script.is_some_and(|enabled| {
            ui.add_enabled(enabled, egui::Button::new("Enable JIT via Script"))
                .clicked()
        });
        let stop = stop
            .is_some_and(|enabled| ui.add_enabled(enabled, egui::Button::new("Stop")).clicked());
        if enable {
            Some(JitAction::Enable)
        } else if script {
            Some(JitAction::RunScript)
        } else if stop {
            Some(JitAction::Stop)
        } else {
            None
        }
    })
    .inner
}

pub fn script_picker<'a>(
    ui: &mut egui::Ui,
    enabled: bool,
    selected: Option<usize>,
    names: impl IntoIterator<Item = &'a str>,
) -> Option<usize> {
    let names: Vec<_> = names.into_iter().collect();
    let height = (ui.ctx().content_rect().bottom() - ui.next_widget_position().y - 16.0).max(120.0);
    ui.add_enabled_ui(enabled, |ui| {
        egui::ComboBox::from_label("Selected Script")
            .height(height)
            .selected_text(
                selected
                    .and_then(|index| names.get(index).copied())
                    .unwrap_or("No script selected"),
            )
            .show_ui(ui, |ui| {
                let mut chosen = None;
                for (index, name) in names.into_iter().enumerate() {
                    if ui.selectable_label(selected == Some(index), name).clicked() {
                        chosen = Some(index);
                    }
                }
                chosen
            })
            .inner
            .flatten()
    })
    .inner
}

#[cfg(not(target_arch = "wasm32"))]
pub fn workspace(ui: &mut egui::Ui, min_width: f32, contents: impl FnOnce(&mut [egui::Ui])) {
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_min_width(min_width);
            ui.columns(2, contents);
        });
}

pub fn debug_indicators(ui: &mut egui::Ui, attached: bool, running: bool) {
    for (active, color, label) in [
        (attached, egui::Color32::from_rgb(80, 200, 120), "Attached"),
        (
            running,
            egui::Color32::from_rgb(255, 196, 64),
            "Script Running",
        ),
    ] {
        let color = if active {
            color
        } else {
            egui::Color32::from_gray(140)
        };
        let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, color);
        ui.label(label);
    }
}

pub fn status_text(text: impl Into<String>, enabled: bool) -> egui::RichText {
    let color = if enabled {
        egui::Color32::from_rgb(80, 200, 80)
    } else {
        egui::Color32::RED
    };
    egui::RichText::new(text).color(color)
}

pub fn mounted_ddi_label(ui: &mut egui::Ui) -> bool {
    ui.add(egui::Label::new(device_success("Mounted")).sense(egui::Sense::click()))
        .clicked()
        && ui.input(|input| input.modifiers.shift)
}

pub fn device_identity(
    ui: &mut egui::Ui,
    model: Option<&str>,
    version: Option<&str>,
    build: Option<&str>,
    udid: Option<&str>,
) {
    let os = match model.unwrap_or_default() {
        model if model.starts_with("iPhone") || model.starts_with("iPod") => "iOS",
        model if model.starts_with("iPad") => "iPadOS",
        model if model.starts_with("AppleTV") => "tvOS",
        model if model.starts_with("Watch") => "watchOS",
        model if model.starts_with("RealityDevice") => "visionOS",
        model if model.starts_with("AudioAccessory") => "HomePod Software",
        _ => "OS",
    };
    let version = version.map(|version| match build {
        Some(build) => format!("{version} ({build})"),
        None => version.to_owned(),
    });
    egui::Grid::new("device_identity")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .max_col_width((ui.available_width() - 62.0).max(48.0))
        .show(ui, |ui| {
            for (label, value) in [("Model", model), (os, version.as_deref()), ("UDID", udid)] {
                ui.weak(label);
                ui.add(egui::Label::new(value.unwrap_or("—")).selectable(true).wrap());
                ui.end_row();
            }
        });
    ui.add_space(8.0);
}

pub fn device_check(ui: &mut egui::Ui, label: &str, value: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal_wrapped(|ui| {
        ui.weak(label);
        value(ui);
    });
}

fn device_success(text: &str) -> egui::RichText {
    egui::RichText::new(text).color(egui::Color32::from_rgb(76, 175, 80))
}

pub fn device_check_value(ui: &mut egui::Ui, enabled: Option<bool>, disabled_text: &str) {
    match enabled {
        Some(true) => ui.label(device_success("Enabled")),
        Some(false) => ui.label(egui::RichText::new(disabled_text).color(ui.visuals().warn_fg_color)),
        None => ui.weak("Unknown"),
    };
}

pub fn device_version_warning(ui: &mut egui::Ui, version: &str) {
    if crate::device_services::unsupported_ios_version(version) {
        ui.label(status_text(
            format!("OS {version} is unsupported. JITserver requires iOS or iPadOS 17.4 or later."),
            false,
        ));
    }
}

pub fn requires_scripts_status(
    ui: &mut egui::Ui,
    status: &crate::device_services::RequiresScriptsStatus,
) {
    use crate::device_services::RequiresScriptsStatus;

    device_check(ui, "Requires scripts", |ui| {
        match status {
            RequiresScriptsStatus::Checking => ui.label("Checking"),
            RequiresScriptsStatus::Yes => {
                ui.label(device_success("Yes (TXM device on iOS 26+)"))
            }
            RequiresScriptsStatus::No(reason) => ui.label(format!("No ({reason})")),
            RequiresScriptsStatus::Unknown(error) => ui.label("Unknown").on_hover_text(error),
        };
    });
}

pub fn device_jit_actions(
    ui: &mut egui::Ui,
    requires_scripts: bool,
    can_enable: bool,
    has_script: bool,
    can_stop: bool,
    stopping: bool,
) -> Option<JitAction> {
    jit_actions(
        ui,
        (!requires_scripts).then_some(can_enable && !can_stop && !stopping),
        requires_scripts.then_some(can_enable && has_script && !can_stop && !stopping),
        (requires_scripts || can_stop || stopping).then_some(can_stop && !stopping),
    )
}
