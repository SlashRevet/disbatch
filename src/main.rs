#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Disbatch — point it at a PowerShell script and it generates a GUI from the
//! script's `param()` block, statically analyses it for risky behaviour, and
//! runs it inside an embedded ConPTY terminal.

mod analyzer;
mod model;
mod parser;
mod sidecar;
mod terminal;

use analyzer::Severity;
use eframe::egui;
use model::{Param, ParamKind};
use std::collections::HashMap;
use std::path::PathBuf;

const RED: egui::Color32 = egui::Color32::from_rgb(225, 90, 90); // hard errors
const WARNING: egui::Color32 = egui::Color32::from_rgb(240, 150, 55); // top risk tier
const CAUTION: egui::Color32 = egui::Color32::from_rgb(205, 195, 120); // lower risk tier
const GRAY: egui::Color32 = egui::Color32::from_rgb(150, 150, 150);

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
    /// Line (1-based) to highlight in the preview, set by clicking a finding.
    highlight_line: Option<usize>,
    /// One-shot flag: scroll the highlighted line into view next frame.
    scroll_pending: bool,
    /// When set, only findings of this severity are shown.
    severity_filter: Option<Severity>,
    /// Per-script hints/notes, persisted to `<script>.disbatch.json`.
    sidecar: sidecar::Sidecar,
    /// Mapper "edit controls" mode toggle.
    mapping_mode: bool,
    /// When set, clicking a preview line binds that control (by index) to it.
    picking_for: Option<usize>,
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
            highlight_line: None,
            scroll_pending: false,
            severity_filter: None,
            sidecar: sidecar::Sidecar::default(),
            mapping_mode: false,
            picking_for: None,
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
                } else if ext == "bat" || ext == "cmd" {
                    self.params = parser::parse_batch(&self.source);
                    self.note = format!(
                        "Detected {} positional argument(s) (%1, %2, …), passed in order.",
                        self.params.len()
                    );
                } else {
                    self.params.clear();
                    self.note =
                        format!("'.{ext}' isn't auto-detected — preview and risk analysis still work.");
                }
                self.findings = analyzer::analyze(&self.source);
                self.risk_ack = false;
                self.editable = false;
                self.highlight_line = None;
                self.scroll_pending = false;
                self.severity_filter = None;
                // Follow the script's folder in the terminal session.
                if let (Some(dir), Some(t)) = (path.parent(), self.terminal.as_mut()) {
                    t.send_line(&format!(
                        "Set-Location -LiteralPath {}",
                        ps_quote(&dir.display().to_string())
                    ));
                }
                self.sidecar = sidecar::Sidecar::load(&path);
                if !self.sidecar.controls.is_empty() {
                    self.params = self.sidecar.controls.iter().map(def_to_param).collect();
                }
                self.apply_saved_values();
                self.script_path = Some(path);
            }
            Err(e) => self.note = format!("Couldn't read file: {e}"),
        }
    }

    fn reanalyze(&mut self) {
        self.params = parser::parse_powershell(&self.source);
        self.findings = analyzer::analyze(&self.source);
        self.risk_ack = false;
        self.highlight_line = None;
        self.severity_filter = None;
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
        let is_batch = self
            .script_path
            .as_ref()
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .map(|e| {
                let e = e.to_lowercase();
                e == "bat" || e == "cmd"
            })
            .unwrap_or(false);

        // Environment-variable bindings are set before the call (both ps1 and bat).
        let mut prefix = String::new();
        for p in &self.params {
            if p.as_env {
                let v = if p.kind == ParamKind::Bool {
                    p.bool_value.to_string()
                } else {
                    p.value.trim().to_string()
                };
                if !v.is_empty() {
                    prefix.push_str(&format!("$env:{} = {}; ", p.name, ps_quote(&v)));
                }
            }
        }

        let mut cmd = format!("{}& {}", prefix, ps_quote(&path));
        if is_batch {
            // Batch parameters are positional — emit values in argument order.
            let mut positional: Vec<&Param> = self
                .params
                .iter()
                .filter(|p| p.position.is_some() && !p.as_env)
                .collect();
            positional.sort_by_key(|p| p.position);
            for p in positional {
                cmd.push_str(&format!(" {}", ps_quote(p.value.trim())));
            }
        } else {
            for p in &self.params {
                if p.as_env || p.position.is_some() {
                    continue;
                }
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
        self.save_sidecar();
    }

    /// Persist hints, mappings, and the currently-typed input values to the sidecar.
    fn save_sidecar(&mut self) {
        let snapshot: Vec<(String, String, bool)> = self
            .params
            .iter()
            .map(|p| (p.name.clone(), p.value.clone(), p.bool_value))
            .collect();
        for (name, value, b) in snapshot {
            self.sidecar.values.insert(name.clone(), value);
            self.sidecar.bool_values.insert(name, b);
        }
        if let Some(path) = self.script_path.clone() {
            let _ = self.sidecar.save(&path);
        }
    }

    /// Restore remembered input values from the sidecar into the current controls.
    fn apply_saved_values(&mut self) {
        for p in &mut self.params {
            if let Some(v) = self.sidecar.values.get(&p.name) {
                p.value = v.clone();
            }
            if let Some(b) = self.sidecar.bool_values.get(&p.name) {
                p.bool_value = *b;
            }
        }
    }

    /// Re-run auto-detection from the current script, discarding mapper overrides.
    fn redetect(&mut self) {
        let ext = self
            .script_path
            .as_ref()
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        self.params = if ext == "ps1" {
            parser::parse_powershell(&self.source)
        } else if ext == "bat" || ext == "cmd" {
            parser::parse_batch(&self.source)
        } else {
            Vec::new()
        };
        self.apply_saved_values();
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
                let (w, c) = analyzer::counts(&self.findings);
                ui.horizontal(|ui| {
                    let warn_sel = self.severity_filter == Some(Severity::Warning);
                    if ui
                        .selectable_label(
                            warn_sel,
                            egui::RichText::new(format!("⚠ {w} Warning")).color(WARNING),
                        )
                        .on_hover_text("Show only warnings (click again to clear)")
                        .clicked()
                    {
                        self.severity_filter =
                            if warn_sel { None } else { Some(Severity::Warning) };
                    }
                    let caut_sel = self.severity_filter == Some(Severity::Caution);
                    if ui
                        .selectable_label(
                            caut_sel,
                            egui::RichText::new(format!("• {c} Caution")).color(CAUTION),
                        )
                        .on_hover_text("Show only cautions (click again to clear)")
                        .clicked()
                    {
                        self.severity_filter =
                            if caut_sel { None } else { Some(Severity::Caution) };
                    }
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
                    egui::RichText::new("Not an antivirus — heuristic only")
                        .color(egui::Color32::from_gray(235))
                        .small(),
                )
                .on_hover_text(
                    "This flags potentially risky patterns in the script text. It is NOT \
                     antivirus and gives no guarantees — expect both false positives and \
                     false negatives. Treat it as a prompt to read the script, not proof of \
                     safety.",
                );
                let mut jump_to: Option<usize> = None;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for f in &self.findings {
                            if let Some(filter) = self.severity_filter {
                                if f.severity != filter {
                                    continue;
                                }
                            }
                            let selected = self.highlight_line == Some(f.line);
                            let title = egui::RichText::new(format!(
                                "[{}] {} · line {} · {}",
                                f.severity.label(),
                                f.category,
                                f.line,
                                f.title
                            ))
                            .color(sev_color(f.severity));
                            let resp = ui
                                .selectable_label(selected, title)
                                .on_hover_text("Jump to this line in the preview");
                            if resp.clicked() {
                                jump_to = Some(f.line);
                            }
                            ui.monospace(egui::RichText::new(&f.snippet).weak().small());
                            ui.add_space(3.0);
                        }
                    });
                if let Some(line) = jump_to {
                    self.highlight_line = Some(line);
                    self.scroll_pending = true;
                    self.editable = false; // show the highlighted read-only view
                }
            });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.add_space(4.0);
            if let Some(pick_idx) = self.picking_for {
                let label = self
                    .params
                    .get(pick_idx)
                    .map(|p| p.label.clone())
                    .unwrap_or_default();
                egui::Frame::none()
                    .fill(egui::Color32::from_rgb(38, 48, 30))
                    .inner_margin(6.0)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.colored_label(
                                egui::Color32::from_rgb(150, 210, 150),
                                format!(
                                    "🎯 Click a line to bind \"{label}\" to its variable/argument"
                                ),
                            );
                            if ui.button("Cancel").clicked() {
                                self.picking_for = None;
                            }
                        });
                    });
                ui.add_space(4.0);
            }
            ui.horizontal(|ui| {
                ui.strong("Preview");
                ui.checkbox(&mut self.editable, "Editable");
                if self.editable && ui.button("Re-analyze").clicked() {
                    self.reanalyze();
                }
            });
            ui.separator();

            let is_batch = self
                .script_path
                .as_ref()
                .and_then(|p| p.extension())
                .and_then(|e| e.to_str())
                .map(|e| {
                    let e = e.to_lowercase();
                    e == "bat" || e == "cmd"
                })
                .unwrap_or(false);
            let picking = self.picking_for;
            let mut bind_click: Option<(usize, Bound)> = None;

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if self.editable {
                        // Wrap long lines (no horizontal scroll).
                        ui.add(
                            egui::TextEdit::multiline(&mut self.source)
                                .code_editor()
                                .desired_width(ui.available_width()),
                        );
                    } else {
                        // Map each line to its highest-severity finding for inline tinting.
                        let mut risky: HashMap<usize, Severity> = HashMap::new();
                        for f in &self.findings {
                            risky
                                .entry(f.line)
                                .and_modify(|s| {
                                    if f.severity.rank() > s.rank() {
                                        *s = f.severity;
                                    }
                                })
                                .or_insert(f.severity);
                        }

                        ui.spacing_mut().item_spacing.y = 1.0;
                        let highlight = self.highlight_line;
                        let mut did_scroll = false;
                        for (i, line) in self.source.lines().enumerate() {
                            let lineno = i + 1;
                            let is_hl = highlight == Some(lineno);
                            if let Some(pick_idx) = picking {
                                // Pick mode: each recognised token is its own clickable chip,
                                // each with a unique id (i, byte-offset) to avoid id clashes.
                                let spans = token_spans(line, is_batch);
                                ui.push_id(i, |ui| {
                                    ui.horizontal_top(|ui| {
                                        ui.spacing_mut().item_spacing.x = 6.0;
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(format!("{lineno:>4}"))
                                                    .weak()
                                                    .monospace(),
                                            )
                                            .selectable(false),
                                        );
                                        ui.horizontal_wrapped(|ui| {
                                            ui.spacing_mut().item_spacing.x = 0.0;
                                            let mut pos = 0usize;
                                            for (s, e, bound) in &spans {
                                                if *s > pos {
                                                    ui.add(
                                                        egui::Label::new(
                                                            egui::RichText::new(&line[pos..*s])
                                                                .monospace(),
                                                        )
                                                        .selectable(false),
                                                    );
                                                }
                                                let clicked = ui
                                                    .push_id(*s, |ui| {
                                                        ui.add(
                                                            egui::Button::new(
                                                                egui::RichText::new(&line[*s..*e])
                                                                    .monospace()
                                                                    .color(egui::Color32::from_rgb(
                                                                        150, 210, 150,
                                                                    )),
                                                            )
                                                            .small(),
                                                        )
                                                    })
                                                    .inner
                                                    .clicked();
                                                if clicked {
                                                    bind_click = Some((pick_idx, bound.clone()));
                                                }
                                                pos = *e;
                                            }
                                            if pos < line.len() {
                                                ui.add(
                                                    egui::Label::new(
                                                        egui::RichText::new(&line[pos..])
                                                            .monospace(),
                                                    )
                                                    .selectable(false),
                                                );
                                            }
                                        });
                                    });
                                });
                            } else {
                                let row = |ui: &mut egui::Ui| {
                                    ui.horizontal_top(|ui| {
                                        ui.spacing_mut().item_spacing.x = 6.0;
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(format!("{lineno:>4}"))
                                                    .weak()
                                                    .monospace(),
                                            )
                                            .selectable(false),
                                        );
                                        ui.add(
                                            egui::Label::new(egui::RichText::new(line).monospace())
                                                .selectable(true)
                                                .wrap(true),
                                        );
                                    })
                                    .response
                                };
                                let bg = if is_hl {
                                    Some(egui::Color32::from_rgb(64, 60, 28))
                                } else {
                                    risky.get(&lineno).map(|s| match s {
                                        Severity::Warning => {
                                            egui::Color32::from_rgba_unmultiplied(240, 150, 55, 32)
                                        }
                                        Severity::Caution => {
                                            egui::Color32::from_rgba_unmultiplied(205, 195, 120, 26)
                                        }
                                    })
                                };
                                let resp = if let Some(fill) = bg {
                                    egui::Frame::none()
                                        .fill(fill)
                                        .inner_margin(egui::Margin::symmetric(2.0, 0.0))
                                        .show(ui, row)
                                        .response
                                } else {
                                    row(ui)
                                };
                                if is_hl && self.scroll_pending {
                                    resp.scroll_to_me(Some(egui::Align::Center));
                                    did_scroll = true;
                                }
                            }
                        }
                        if did_scroll {
                            self.scroll_pending = false;
                        }
                    }
                });

            if let Some((idx, b)) = bind_click {
                if let Some(p) = self.params.get_mut(idx) {
                    apply_binding(p, b);
                }
                self.picking_for = None;
                self.tab = Tab::Controls;
            }
        });
    }

    fn controls_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if self.script_path.is_none() {
            ui.add_space(12.0);
            ui.label(&self.note);
            return;
        }
        ui.add_space(4.0);

        egui::CollapsingHeader::new("📝 Hints / usage notes")
            .default_open(!self.sidecar.hints.trim().is_empty())
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.sidecar.hints)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .hint_text(
                            "How to use this script. Saved to <script>.disbatch.json next to it — commit that file to share hints with your team.",
                        ),
                );
                if ui.button("💾 Save hints").clicked() {
                    self.save_sidecar();
                }
            });
        ui.separator();

        ui.horizontal(|ui| {
            ui.checkbox(&mut self.mapping_mode, "✏ Edit controls");
            if self.mapping_mode {
                if ui.button("➕ Add control").clicked() {
                    let n = self.params.len() + 1;
                    self.params.push(custom_param(n));
                    self.picking_for = Some(self.params.len() - 1);
                    self.tab = Tab::Script;
                }
                if ui.button("💾 Save mapping").clicked() {
                    self.sidecar.controls = self.params.iter().map(param_to_def).collect();
                    self.save_sidecar();
                }
                if ui.button("↺ Re-detect").clicked() {
                    self.sidecar.controls.clear();
                    self.redetect();
                }
            }
        });
        if self.mapping_mode {
            ui.label(
                egui::RichText::new(
                    "Change a control's type/label, mark it required, or add custom ones — then Save mapping to persist (and share via the sidecar).",
                )
                .weak()
                .small(),
            );
        }
        ui.separator();

        let editing = self.mapping_mode;
        let mut remove: Option<usize> = None;
        let mut pick: Option<usize> = None;
        if self.params.is_empty() && !editing {
            ui.label("No parameters detected — you can still run the script as-is.");
            ui.add_space(6.0);
        } else {
            egui::Grid::new("controls")
                .num_columns(if editing { 4 } else { 2 })
                .spacing([10.0, 8.0])
                .striped(true)
                .show(ui, |ui| {
                    for (idx, p) in self.params.iter_mut().enumerate() {
                        if editing {
                            ui.add(
                                egui::TextEdit::singleline(&mut p.label)
                                    .desired_width(150.0)
                                    .hint_text("label"),
                            );
                            kind_combo(ui, idx, p);
                            ui.checkbox(&mut p.required, "required");
                            ui.horizontal(|ui| {
                                if ui
                                    .button(binding_label(p))
                                    .on_hover_text(
                                        "Pick the script variable/argument this control sets",
                                    )
                                    .clicked()
                                {
                                    pick = Some(idx);
                                }
                                if p.custom && ui.small_button("✕").clicked() {
                                    remove = Some(idx);
                                }
                            });
                        } else {
                            let label = if p.required {
                                format!("{} *", p.label)
                            } else {
                                p.label.clone()
                            };
                            ui.label(label);
                            param_widget(ui, p);
                        }
                        ui.end_row();
                    }
                });
        }
        if let Some(i) = remove {
            self.params.remove(i);
        }
        if let Some(i) = pick {
            self.picking_for = Some(i);
            self.tab = Tab::Script;
        }

        if editing {
            let mut remove_choice: Option<(usize, usize)> = None;
            let mut add_choice: Option<usize> = None;
            for (pi, p) in self.params.iter_mut().enumerate() {
                if p.kind == ParamKind::Choice {
                    ui.group(|ui| {
                        ui.label(format!("Dropdown options for \"{}\":", p.label));
                        for (ci, choice) in p.choices.iter_mut().enumerate() {
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(choice)
                                        .desired_width(180.0)
                                        .hint_text("option"),
                                );
                                if ui.small_button("✕").clicked() {
                                    remove_choice = Some((pi, ci));
                                }
                            });
                        }
                        if ui.button("➕ option").clicked() {
                            add_choice = Some(pi);
                        }
                    });
                }
            }
            if let Some((pi, ci)) = remove_choice {
                if let Some(p) = self.params.get_mut(pi) {
                    if ci < p.choices.len() {
                        p.choices.remove(ci);
                    }
                }
            }
            if let Some(pi) = add_choice {
                if let Some(p) = self.params.get_mut(pi) {
                    p.choices.push(String::new());
                }
            }
        }

        ui.separator();
        ui.label(egui::RichText::new("Command preview").weak());
        ui.add(
            egui::Label::new(egui::RichText::new(self.compose_command()).monospace())
                .selectable(true),
        );
        ui.separator();

        let gated = analyzer::has_warning(&self.findings);
        if gated {
            ui.colored_label(
                WARNING,
                "⚠ Warning-level patterns detected (see the Script tab). Review before running.",
            );
            ui.checkbox(&mut self.risk_ack, "I understand the risks, run anyway");
        }

        let missing = self.missing_required();
        let can_run = missing.is_empty() && (!gated || self.risk_ack);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(can_run, egui::Button::new("▶ Run"))
                .clicked()
            {
                self.run(ctx);
            }
            if !missing.is_empty() {
                ui.colored_label(WARNING, format!("Required: {}", missing.join(", ")));
            } else if gated && !self.risk_ack {
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

        // Drag-and-drop a script onto the window to open it.
        let dropped = ctx.input(|i| i.raw.dropped_files.iter().find_map(|f| f.path.clone()));
        if let Some(path) = dropped {
            self.open_script(path);
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
        Severity::Warning => WARNING,
        Severity::Caution => CAUTION,
    }
}

/// Quote a value as a PowerShell single-quoted string (doubling embedded quotes).
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn kind_label(k: ParamKind) -> &'static str {
    match k {
        ParamKind::Text => "Text",
        ParamKind::FilePath => "File picker",
        ParamKind::FolderPath => "Folder picker",
        ParamKind::Number => "Number",
        ParamKind::Bool => "Checkbox",
        ParamKind::Choice => "Dropdown",
    }
}

fn kind_to_str(k: ParamKind) -> &'static str {
    match k {
        ParamKind::Text => "text",
        ParamKind::FilePath => "file",
        ParamKind::FolderPath => "folder",
        ParamKind::Number => "number",
        ParamKind::Bool => "bool",
        ParamKind::Choice => "choice",
    }
}

fn kind_from_str(s: &str) -> ParamKind {
    match s {
        "file" => ParamKind::FilePath,
        "folder" => ParamKind::FolderPath,
        "number" => ParamKind::Number,
        "bool" => ParamKind::Bool,
        "choice" => ParamKind::Choice,
        _ => ParamKind::Text,
    }
}

fn param_to_def(p: &Param) -> sidecar::ControlDef {
    sidecar::ControlDef {
        name: p.name.clone(),
        label: p.label.clone(),
        kind: kind_to_str(p.kind).to_string(),
        required: p.required,
        default: if p.kind == ParamKind::Bool {
            p.bool_value.to_string()
        } else {
            p.value.clone()
        },
        choices: p.choices.clone(),
        position: p.position,
        custom: p.custom,
        as_env: p.as_env,
    }
}

fn def_to_param(d: &sidecar::ControlDef) -> Param {
    let kind = kind_from_str(&d.kind);
    Param {
        name: d.name.clone(),
        label: d.label.clone(),
        kind,
        required: d.required,
        is_switch: kind == ParamKind::Bool,
        choices: d.choices.clone(),
        value: if kind == ParamKind::Bool {
            String::new()
        } else {
            d.default.clone()
        },
        bool_value: d.default.eq_ignore_ascii_case("true"),
        position: d.position,
        custom: d.custom,
        as_env: d.as_env,
    }
}

fn custom_param(n: usize) -> Param {
    Param {
        name: format!("Custom{n}"),
        label: format!("Custom {n}"),
        kind: ParamKind::Text,
        required: false,
        is_switch: false,
        choices: Vec::new(),
        value: String::new(),
        bool_value: false,
        position: None,
        custom: true,
        as_env: false,
    }
}

/// How a control injects its value into the run, picked from a script token.
#[derive(Clone)]
enum Bound {
    Positional(u32),
    Named(String),
    Env(String),
}

/// Find clickable binding tokens in a line: (start_byte, end_byte, target),
/// non-overlapping and left-to-right.
fn token_spans(line: &str, is_batch: bool) -> Vec<(usize, usize, Bound)> {
    let mut spans: Vec<(usize, usize, Bound)> = Vec::new();
    if is_batch {
        for c in regex::Regex::new(r"%~?[a-zA-Z]*([1-9])")
            .unwrap()
            .captures_iter(line)
        {
            let m = c.get(0).unwrap();
            if let Ok(n) = c[1].parse::<u32>() {
                spans.push((m.start(), m.end(), Bound::Positional(n)));
            }
        }
        for c in regex::Regex::new(r"%([A-Za-z_]\w*)%")
            .unwrap()
            .captures_iter(line)
        {
            let m = c.get(0).unwrap();
            spans.push((m.start(), m.end(), Bound::Env(c[1].to_string())));
        }
        for c in regex::Regex::new(r#"(?i)\bset\s+(?:/p\s+)?"?([A-Za-z_]\w*)\s*="#)
            .unwrap()
            .captures_iter(line)
        {
            let g = c.get(1).unwrap();
            spans.push((g.start(), g.end(), Bound::Env(g.as_str().to_string())));
        }
    } else {
        for c in regex::Regex::new(r"(?i)\$env:([A-Za-z_]\w*)")
            .unwrap()
            .captures_iter(line)
        {
            let m = c.get(0).unwrap();
            spans.push((m.start(), m.end(), Bound::Env(c[1].to_string())));
        }
        for c in regex::Regex::new(r"\$([A-Za-z_]\w*)")
            .unwrap()
            .captures_iter(line)
        {
            let m = c.get(0).unwrap();
            let name = c[1].to_string();
            if !matches!(name.to_lowercase().as_str(), "true" | "false" | "null") {
                spans.push((m.start(), m.end(), Bound::Named(name)));
            }
        }
    }
    spans.sort_by_key(|s| (s.0, std::cmp::Reverse(s.1)));
    let mut result: Vec<(usize, usize, Bound)> = Vec::new();
    let mut last_end = 0usize;
    for (s, e, b) in spans {
        if s >= last_end {
            result.push((s, e, b));
            last_end = e;
        }
    }
    result
}

fn apply_binding(p: &mut Param, b: Bound) {
    match b {
        Bound::Positional(n) => {
            p.position = Some(n);
            p.as_env = false;
            p.name = format!("arg{n}");
        }
        Bound::Named(name) => {
            p.position = None;
            p.as_env = false;
            p.name = name;
        }
        Bound::Env(name) => {
            p.position = None;
            p.as_env = true;
            p.name = name;
        }
    }
}

/// Short label for a control's current binding (shown on the mapper row).
fn binding_label(p: &Param) -> String {
    if let Some(n) = p.position {
        format!("→ %{n}")
    } else if p.as_env {
        format!("→ $env:{}", p.name)
    } else {
        format!("→ -{}", p.name)
    }
}

fn kind_combo(ui: &mut egui::Ui, idx: usize, p: &mut Param) {
    egui::ComboBox::from_id_source(("kind", idx))
        .selected_text(kind_label(p.kind))
        .show_ui(ui, |ui| {
            for k in [
                ParamKind::Text,
                ParamKind::FolderPath,
                ParamKind::FilePath,
                ParamKind::Number,
                ParamKind::Bool,
                ParamKind::Choice,
            ] {
                if ui.selectable_label(p.kind == k, kind_label(k)).clicked() {
                    p.kind = k;
                    p.is_switch = k == ParamKind::Bool;
                }
            }
        });
}
