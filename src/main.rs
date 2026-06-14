#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Disbatch — point it at a PowerShell script and it generates a GUI from the
//! script's `param()` block, statically analyses it for risky behaviour, and
//! runs it inside an embedded ConPTY terminal.

mod analyzer;
mod model;
mod parser;
mod terminal;

use analyzer::Severity;
use eframe::egui;
use model::{Param, ParamKind};
use std::path::PathBuf;

const RED: egui::Color32 = egui::Color32::from_rgb(230, 85, 85);
const ORANGE: egui::Color32 = egui::Color32::from_rgb(230, 160, 60);
const GRAY: egui::Color32 = egui::Color32::from_rgb(150, 150, 150);
const GREEN: egui::Color32 = egui::Color32::from_rgb(90, 180, 90);

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([920.0, 780.0])
            .with_min_inner_size([640.0, 520.0])
            .with_title("Disbatch"),
        ..Default::default()
    };
    eframe::run_native(
        "Disbatch",
        native_options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Box::new(DisbatchApp::default())
        }),
    )
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Script,
    Controls,
}

struct DisbatchApp {
    tab: Tab,
    script_path: Option<PathBuf>,
    source: String,
    editable: bool,
    params: Vec<Param>,
    note: String,
    findings: Vec<analyzer::Finding>,
    risk_ack: bool,
    terminal: Option<terminal::Terminal>,
    terminal_err: Option<String>,
}

impl Default for DisbatchApp {
    fn default() -> Self {
        Self {
            tab: Tab::Script,
            script_path: None,
            source: String::new(),
            editable: false,
            params: Vec::new(),
            note: "Open a PowerShell (.ps1) script to begin.".into(),
            findings: Vec::new(),
            risk_ack: false,
            terminal: None,
            terminal_err: None,
        }
    }
}

impl DisbatchApp {
    fn open_script(&mut self, path: PathBuf) {
        match std::fs::read_to_string(&path) {
            Ok(src) => {
                self.source = src;
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                if ext == "ps1" {
                    self.params = parser::parse_powershell(&self.source);
                    self.note = format!("Detected {} parameter(s).", self.params.len());
                } else {
                    self.params.clear();
                    self.note = format!(
                        "'.{ext}' parameters aren't auto-detected yet (mapper coming) — preview and risk analysis still work."
                    );
                }
                self.findings = analyzer::analyze(&self.source);
                self.risk_ack = false;
                self.editable = false;
                // Follow the script's folder in the terminal session.
                if let (Some(dir), Some(t)) = (path.parent(), self.terminal.as_mut()) {
                    t.send_line(&format!(
                        "Set-Location -LiteralPath {}",
                        ps_quote(&dir.display().to_string())
                    ));
                }
                self.script_path = Some(path);
            }
            Err(e) => self.note = format!("Couldn't read file: {e}"),
        }
    }

    fn reanalyze(&mut self) {
        self.params = parser::parse_powershell(&self.source);
        self.findings = analyzer::analyze(&self.source);
        self.risk_ack = false;
    }

    fn spawn_terminal(&mut self, ctx: &egui::Context) {
        let cwd = self
            .script_path
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()));
        match terminal::Terminal::new(ctx, "powershell.exe", cwd) {
            Ok(t) => {
                self.terminal = Some(t);
                self.terminal_err = None;
            }
            Err(e) => self.terminal_err = Some(format!("Terminal failed to start: {e}")),
        }
    }

    fn compose_command(&self) -> String {
        let path = self
            .script_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let mut cmd = format!("& {}", ps_quote(&path));
        for p in &self.params {
            match p.kind {
                ParamKind::Bool => {
                    if p.is_switch {
                        if p.bool_value {
                            cmd.push_str(&format!(" -{}", p.name));
                        }
                    } else {
                        cmd.push_str(&format!(" -{}:${}", p.name, p.bool_value));
                    }
                }
                _ => {
                    let v = p.value.trim();
                    if !v.is_empty() {
                        cmd.push_str(&format!(" -{} {}", p.name, ps_quote(v)));
                    }
                }
            }
        }
        cmd
    }

    fn missing_required(&self) -> Vec<String> {
        self.params
            .iter()
            .filter(|p| p.required && p.kind != ParamKind::Bool && p.value.trim().is_empty())
            .map(|p| p.label.clone())
            .collect()
    }

    fn run(&mut self, ctx: &egui::Context) {
        if self.terminal.is_none() {
            self.spawn_terminal(ctx);
        }
        let cmd = self.compose_command();
        if let Some(t) = self.terminal.as_mut() {
            t.reset_progress();
            t.send_line(&cmd);
        }
    }

    fn script_tab(&mut self, ui: &mut egui::Ui) {
        if self.source.is_empty() {
            ui.add_space(12.0);
            ui.label(&self.note);
            return;
        }

        egui::SidePanel::right("analysis")
            .default_width(310.0)
            .show_inside(ui, |ui| {
                ui.add_space(4.0);
                ui.heading("Analysis");
                let (d, w, i) = analyzer::counts(&self.findings);
                ui.horizontal(|ui| {
                    ui.colored_label(RED, format!("⛔ {d}"));
                    ui.colored_label(ORANGE, format!("⚠ {w}"));
                    ui.colored_label(GRAY, format!("ℹ {i}"));
                });
                ui.separator();

                let admin = {
                    let s = self.source.to_lowercase();
                    s.contains("runasadministrator") || s.contains("-verb runas")
                };
                let required = self.params.iter().filter(|p| p.required).count();
                ui.label(format!("Lines: {}", self.source.lines().count()));
                ui.label(format!(
                    "Parameters: {} ({} required)",
                    self.params.len(),
                    required
                ));
                ui.label(format!("Needs admin: {}", if admin { "yes" } else { "no" }));
                ui.separator();

                ui.strong("Findings");
                ui.label(
                    egui::RichText::new("Heuristic, not antivirus — informs your choice.")
                        .weak()
                        .small(),
                );
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if self.findings.is_empty() {
                            ui.add_space(4.0);
                            ui.colored_label(GREEN, "No risky patterns detected.");
                        }
                        for f in &self.findings {
                            ui.colored_label(
                                sev_color(f.severity),
                                format!(
                                    "[{}] {} · line {} · {}",
                                    f.severity.label(),
                                    f.category,
                                    f.line,
                                    f.title
                                ),
                            );
                            ui.monospace(egui::RichText::new(&f.snippet).weak().small());
                            ui.add_space(3.0);
                        }
                    });
            });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.strong("Preview");
                ui.checkbox(&mut self.editable, "Editable");
                if self.editable && ui.button("Re-analyze").clicked() {
                    self.reanalyze();
                }
            });
            ui.separator();
            egui::ScrollArea::both()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if self.editable {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.source)
                                .code_editor()
                                .desired_width(f32::INFINITY),
                        );
                    } else {
                        ui.add(
                            egui::Label::new(egui::RichText::new(&self.source).monospace())
                                .selectable(true),
                        );
                    }
                });
        });
    }

    fn controls_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if self.script_path.is_none() {
            ui.add_space(12.0);
            ui.label(&self.note);
            return;
        }
        ui.add_space(4.0);
        if self.params.is_empty() {
            ui.label("No parameters detected — you can still run the script as-is.");
            ui.add_space(6.0);
        } else {
            egui::Grid::new("controls")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .striped(true)
                .show(ui, |ui| {
                    for p in &mut self.params {
                        let label = if p.required {
                            format!("{} *", p.label)
                        } else {
                            p.label.clone()
                        };
                        ui.label(label);
                        param_widget(ui, p);
                        ui.end_row();
                    }
                });
        }

        ui.separator();
        ui.label(egui::RichText::new("Command preview").weak());
        ui.add(
            egui::Label::new(egui::RichText::new(self.compose_command()).monospace())
                .selectable(true),
        );
        ui.separator();

        let danger = analyzer::has_danger(&self.findings);
        if danger {
            ui.colored_label(
                RED,
                "⛔ Danger-level patterns detected (see the Script tab). Review before running.",
            );
            ui.checkbox(&mut self.risk_ack, "I understand the risks, run anyway");
        }

        let missing = self.missing_required();
        let can_run = missing.is_empty() && (!danger || self.risk_ack);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(can_run, egui::Button::new("▶ Run"))
                .clicked()
            {
                self.run(ctx);
            }
            if !missing.is_empty() {
                ui.colored_label(ORANGE, format!("Required: {}", missing.join(", ")));
            } else if danger && !self.risk_ack {
                ui.colored_label(GRAY, "Acknowledge the risk to enable Run.");
            }
        });
    }
}

impl eframe::App for DisbatchApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Auto-spawn a terminal session on first frame.
        if self.terminal.is_none() && self.terminal_err.is_none() {
            self.spawn_terminal(ctx);
        }

        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("Disbatch");
                ui.add_space(6.0);
                if ui.button("📂 Open script").clicked() {
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("Scripts", &["ps1", "bat", "cmd"])
                        .pick_file()
                    {
                        self.open_script(p);
                    }
                }
                if let Some(p) = &self.script_path {
                    ui.monospace(p.file_name().and_then(|f| f.to_str()).unwrap_or(""));
                }
            });
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Script, "📄 Script");
                ui.selectable_value(&mut self.tab, Tab::Controls, "🎛 Controls");
            });
            ui.add_space(2.0);
        });

        egui::TopBottomPanel::bottom("terminal")
            .resizable(true)
            .default_height(320.0)
            .min_height(120.0)
            .show(ctx, |ui| {
                let progress = self.terminal.as_ref().map(|t| t.progress());
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.strong("Terminal");
                    ui.label(egui::RichText::new("ConPTY · PowerShell").weak());
                    if ui.button("New session").clicked() {
                        self.spawn_terminal(ctx);
                    }
                    if let Some(err) = &self.terminal_err {
                        ui.colored_label(RED, err);
                    }
                    if let Some(p) = &progress {
                        if p.seen && !p.status.is_empty() {
                            ui.label(&p.status);
                        }
                    }
                });
                if let Some(p) = &progress {
                    if p.seen {
                        ui.add(egui::ProgressBar::new(p.value).show_percentage());
                    }
                }
                ui.separator();
                if let Some(t) = self.terminal.as_mut() {
                    t.ui(ui);
                } else {
                    ui.label("No terminal session. Click \"New session\".");
                }
            });

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Script => self.script_tab(ui),
            Tab::Controls => self.controls_tab(ui, ctx),
        });
    }
}

fn param_widget(ui: &mut egui::Ui, p: &mut Param) {
    match p.kind {
        ParamKind::Bool => {
            ui.checkbox(&mut p.bool_value, "");
        }
        ParamKind::Choice => {
            let choices = p.choices.clone();
            egui::ComboBox::from_id_source(&p.name)
                .selected_text(p.value.clone())
                .show_ui(ui, |ui| {
                    for c in &choices {
                        ui.selectable_value(&mut p.value, c.clone(), c.clone());
                    }
                });
        }
        ParamKind::Number => {
            ui.add(egui::TextEdit::singleline(&mut p.value).desired_width(120.0));
        }
        ParamKind::FolderPath => {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut p.value).desired_width(300.0));
                if ui.button("Browse").clicked() {
                    if let Some(d) = rfd::FileDialog::new().pick_folder() {
                        p.value = d.display().to_string();
                    }
                }
            });
        }
        ParamKind::FilePath => {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut p.value).desired_width(300.0));
                if ui.button("Browse").clicked() {
                    if let Some(d) = rfd::FileDialog::new().pick_file() {
                        p.value = d.display().to_string();
                    }
                }
            });
        }
        ParamKind::Text => {
            ui.add(egui::TextEdit::singleline(&mut p.value).desired_width(300.0));
        }
    }
}

fn sev_color(s: Severity) -> egui::Color32 {
    match s {
        Severity::Danger => RED,
        Severity::Warning => ORANGE,
        Severity::Info => GRAY,
    }
}

/// Quote a value as a PowerShell single-quoted string (doubling embedded quotes).
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}
