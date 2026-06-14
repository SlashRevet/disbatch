//! Embedded terminal backed by a Windows pseudo-console (ConPTY), via the
//! `portable-pty` crate, parsed with `vt100`, and drawn in egui.
//!
//! This is the same mechanism Windows Terminal uses under the hood — we attach a
//! real shell to a pseudo-console and render the screen ourselves, so a genuine
//! interactive terminal lives inside the app (colors, cursor, you can type into
//! prompts). Nothing here touches the network; it's all local OS APIs.

use eframe::egui;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use regex::Regex;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

const DEFAULT_BG: egui::Color32 = egui::Color32::from_rgb(18, 18, 18);
const DEFAULT_FG: egui::Color32 = egui::Color32::from_rgb(220, 220, 220);

/// Progress/status scraped from the live output stream (the opt-in protocol).
#[derive(Clone, Default)]
pub struct TermProgress {
    pub value: f32, // 0.0..=1.0
    pub status: String,
    pub seen: bool, // true once any @progress/@status line has appeared
}

pub struct Terminal {
    parser: Arc<Mutex<vt100::Parser>>,
    progress: Arc<Mutex<TermProgress>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    pid: Option<u32>,
    paused: bool,
    rows: u16,
    cols: u16,
    font_size: f32,
}

impl Terminal {
    /// Spawn `shell` attached to a fresh ConPTY and start streaming its output.
    pub fn new(
        ctx: &egui::Context,
        shell: &str,
        cwd: Option<std::path::PathBuf>,
    ) -> anyhow::Result<Self> {
        let (rows, cols) = (24u16, 80u16);
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(shell);
        // -ExecutionPolicy Bypass so scripts Disbatch launches actually run
        // (Windows blocks .ps1 execution by default). The risk analyzer is the
        // safety gate before the user clicks Run.
        cmd.args(["-NoLogo", "-ExecutionPolicy", "Bypass"]);
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }
        let child = pair.slave.spawn_command(cmd)?;
        let pid = child.process_id();
        drop(pair.slave); // parent doesn't need the slave side

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 5000)));
        let progress = Arc::new(Mutex::new(TermProgress::default()));
        let mut reader = pair.master.try_clone_reader()?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));

        // Reader thread: feed the VT parser, answer terminal queries, and
        // scrape the progress protocol.
        {
            let parser = parser.clone();
            let progress = progress.clone();
            let writer = writer.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let prog_re = Regex::new(r"(?i)@progress\s+(\d+(?:\.\d+)?)").unwrap();
                let stat_re = Regex::new(r"(?i)@status\s+([^\r\n]*)").unwrap();
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let cursor = {
                                let mut p = parser.lock().unwrap();
                                p.process(&buf[..n]);
                                p.screen().cursor_position()
                            };
                            let replies = query_replies(&buf[..n], cursor);
                            if !replies.is_empty() {
                                if let Ok(mut w) = writer.lock() {
                                    let _ = w.write_all(&replies);
                                    let _ = w.flush();
                                }
                            }
                            let text = String::from_utf8_lossy(&buf[..n]);
                            if let Ok(mut pr) = progress.lock() {
                                if let Some(c) = prog_re.captures_iter(&text).last() {
                                    if let Ok(v) = c[1].parse::<f32>() {
                                        pr.value = (v / 100.0).clamp(0.0, 1.0);
                                        pr.seen = true;
                                    }
                                }
                                if let Some(c) = stat_re.captures_iter(&text).last() {
                                    pr.status = c[1].trim().to_string();
                                    pr.seen = true;
                                }
                            }
                            ctx.request_repaint();
                        }
                    }
                }
            });
        }

        Ok(Self {
            parser,
            progress,
            writer,
            master: pair.master,
            child,
            pid,
            paused: false,
            rows,
            cols,
            font_size: 14.0,
        })
    }

    pub fn send(&mut self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    pub fn send_line(&mut self, s: &str) {
        self.send(s.as_bytes());
        self.send(b"\r");
    }

    pub fn progress(&self) -> TermProgress {
        self.progress.lock().map(|p| p.clone()).unwrap_or_default()
    }

    pub fn reset_progress(&self) {
        if let Ok(mut p) = self.progress.lock() {
            *p = TermProgress::default();
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Freeze (or unfreeze) the shell process exactly where it is.
    pub fn toggle_pause(&mut self) {
        if let Some(pid) = self.pid {
            let want = !self.paused;
            suspend_resume(pid, want);
            self.paused = want;
        }
    }

    /// Send Ctrl+C to interrupt the running command (resuming first if paused).
    pub fn interrupt(&mut self) {
        if self.paused {
            self.toggle_pause();
        }
        self.send(&[0x03]);
    }

    /// Clear the visible terminal.
    pub fn clear(&mut self) {
        self.send_line("Clear-Host");
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        if let Ok(mut p) = self.parser.lock() {
            p.screen_mut().set_size(rows, cols);
        }
    }

    /// Render the terminal grid and handle keyboard input when focused.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let font_id = egui::FontId::monospace(self.font_size);
        let (cell_w, cell_h) =
            ui.fonts(|f| (f.glyph_width(&font_id, 'M').max(1.0), f.row_height(&font_id).max(1.0)));

        let avail = ui.available_size();
        let cols = ((avail.x / cell_w).floor() as i32).clamp(8, 400) as u16;
        let rows = ((avail.y / cell_h).floor() as i32).clamp(2, 200) as u16;
        self.resize(rows, cols);

        let size = egui::vec2(cols as f32 * cell_w, rows as f32 * cell_h);
        let (resp, painter) = ui.allocate_painter(size, egui::Sense::click());
        let origin = resp.rect.min;
        if resp.clicked() {
            resp.request_focus();
        }
        let focused = resp.has_focus();

        painter.rect_filled(resp.rect, 0.0, DEFAULT_BG);

        // Build the screen text, grouping consecutive same-style cells into runs.
        let mut job = egui::text::LayoutJob::default();
        job.wrap.max_width = f32::INFINITY;
        let (cur_row, cur_col, cursor_hidden) = {
            let parser = self.parser.lock().unwrap();
            let screen = parser.screen();
            let (srows, scols) = screen.size();
            for row in 0..srows {
                let mut run = String::new();
                let mut run_fg = DEFAULT_FG;
                let mut run_bg = DEFAULT_BG;
                let mut started = false;
                for col in 0..scols {
                    let (ch, fg, bg): (&str, egui::Color32, egui::Color32) =
                        match screen.cell(row, col) {
                            Some(c) => {
                                let s = c.contents();
                                let s = if s.is_empty() { " " } else { s };
                                let mut fg = to_color32(c.fgcolor(), DEFAULT_FG);
                                let mut bg = to_color32(c.bgcolor(), DEFAULT_BG);
                                if c.inverse() {
                                    std::mem::swap(&mut fg, &mut bg);
                                }
                                (s, fg, bg)
                            }
                            None => (" ", DEFAULT_FG, DEFAULT_BG),
                        };
                    if started && fg == run_fg && bg == run_bg {
                        run.push_str(ch);
                    } else {
                        if started {
                            append_run(&mut job, &run, run_fg, run_bg, &font_id);
                        }
                        run.clear();
                        run.push_str(ch);
                        run_fg = fg;
                        run_bg = bg;
                        started = true;
                    }
                }
                if started {
                    append_run(&mut job, &run, run_fg, run_bg, &font_id);
                }
                job.append(
                    "\n",
                    0.0,
                    egui::TextFormat {
                        font_id: font_id.clone(),
                        color: DEFAULT_FG,
                        ..Default::default()
                    },
                );
            }
            let (cr, cc) = screen.cursor_position();
            (cr, cc, screen.hide_cursor())
        };

        let galley = ui.fonts(|f| f.layout_job(job));
        painter.galley(origin, galley, DEFAULT_FG);

        if !cursor_hidden {
            let cur = egui::Rect::from_min_size(
                origin + egui::vec2(cur_col as f32 * cell_w, cur_row as f32 * cell_h),
                egui::vec2(cell_w, cell_h),
            );
            let color = if focused {
                egui::Color32::from_rgba_unmultiplied(120, 200, 120, 150)
            } else {
                egui::Color32::from_rgba_unmultiplied(140, 140, 140, 90)
            };
            painter.rect_filled(cur, 0.0, color);
        }

        if focused {
            painter.rect_stroke(
                resp.rect,
                0.0,
                egui::Stroke::new(1.0, egui::Color32::from_rgb(80, 130, 80)),
            );
            let events = ui.input(|i| i.events.clone());
            for ev in events {
                match ev {
                    egui::Event::Text(t) => self.send(t.as_bytes()),
                    egui::Event::Paste(t) => self.send(t.as_bytes()),
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        if let Some(bytes) = key_to_bytes(key, modifiers) {
                            self.send(&bytes);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// Suspend or resume all threads of a process (an OS-level freeze) via ntdll.
#[cfg(windows)]
fn suspend_resume(pid: u32, suspend: bool) {
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::libloaderapi::{GetModuleHandleA, GetProcAddress};
    use winapi::um::processthreadsapi::OpenProcess;
    use winapi::um::winnt::{HANDLE, PROCESS_SUSPEND_RESUME};
    unsafe {
        let handle = OpenProcess(PROCESS_SUSPEND_RESUME, 0, pid);
        if handle.is_null() {
            return;
        }
        let ntdll = GetModuleHandleA(b"ntdll.dll\0".as_ptr() as *const i8);
        if !ntdll.is_null() {
            let name: &[u8] = if suspend {
                b"NtSuspendProcess\0"
            } else {
                b"NtResumeProcess\0"
            };
            let proc = GetProcAddress(ntdll, name.as_ptr() as *const i8);
            if !proc.is_null() {
                let func: unsafe extern "system" fn(HANDLE) -> i32 = std::mem::transmute(proc);
                func(handle);
            }
        }
        CloseHandle(handle);
    }
}

#[cfg(not(windows))]
fn suspend_resume(_pid: u32, _suspend: bool) {}

fn append_run(
    job: &mut egui::text::LayoutJob,
    run: &str,
    fg: egui::Color32,
    bg: egui::Color32,
    font_id: &egui::FontId,
) {
    job.append(
        run,
        0.0,
        egui::TextFormat {
            font_id: font_id.clone(),
            color: fg,
            background: bg,
            ..Default::default()
        },
    );
}

/// Translate keys that egui does NOT deliver as `Text` (control/navigation)
/// into the byte sequences a terminal expects. Plain characters arrive via
/// `Event::Text`, so we return `None` for them to avoid double input.
fn key_to_bytes(key: egui::Key, m: egui::Modifiers) -> Option<Vec<u8>> {
    use egui::Key::*;
    if m.ctrl {
        let c: u8 = match key {
            A => 1, B => 2, C => 3, D => 4, E => 5, F => 6, G => 7, H => 8,
            I => 9, J => 10, K => 11, L => 12, M => 13, N => 14, O => 15, P => 16,
            Q => 17, R => 18, S => 19, T => 20, U => 21, V => 22, W => 23, X => 24,
            Y => 25, Z => 26, _ => 0,
        };
        if c != 0 {
            return Some(vec![c]);
        }
    }
    let seq: &[u8] = match key {
        Enter => b"\r",
        Backspace => b"\x7f",
        Tab => b"\t",
        Escape => b"\x1b",
        ArrowUp => b"\x1b[A",
        ArrowDown => b"\x1b[B",
        ArrowRight => b"\x1b[C",
        ArrowLeft => b"\x1b[D",
        Home => b"\x1b[H",
        End => b"\x1b[F",
        Delete => b"\x1b[3~",
        PageUp => b"\x1b[5~",
        PageDown => b"\x1b[6~",
        _ => return None,
    };
    Some(seq.to_vec())
}

/// Build replies to terminal queries the shell/ConPTY sends (cursor-position
/// DSR `ESC[6n`, status DSR `ESC[5n`, and Device Attributes `ESC[c`). Without a
/// DSR reply, PSReadLine waits forever for a cursor report and never draws its
/// prompt.
fn query_replies(data: &[u8], cursor: (u16, u16)) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < data.len() {
        if data[i] == 0x1b && data[i + 1] == b'[' {
            let mut j = i + 2;
            while j < data.len() && !data[j].is_ascii_alphabetic() {
                j += 1;
            }
            if j < data.len() {
                let params = &data[i + 2..j];
                match data[j] {
                    b'n' if params == b"6" => out
                        .extend_from_slice(format!("\x1b[{};{}R", cursor.0 + 1, cursor.1 + 1).as_bytes()),
                    b'n' if params == b"5" => out.extend_from_slice(b"\x1b[0n"),
                    b'c' => out.extend_from_slice(b"\x1b[?1;0c"),
                    _ => {}
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn to_color32(c: vt100::Color, default: egui::Color32) -> egui::Color32 {
    match c {
        vt100::Color::Default => default,
        vt100::Color::Idx(i) => ansi_idx(i),
        vt100::Color::Rgb(r, g, b) => egui::Color32::from_rgb(r, g, b),
    }
}

/// Map an xterm 256-color index to an RGB color.
fn ansi_idx(i: u8) -> egui::Color32 {
    const BASE: [(u8, u8, u8); 16] = [
        (0, 0, 0),       (205, 0, 0),   (0, 205, 0),   (205, 205, 0),
        (0, 0, 238),     (205, 0, 205), (0, 205, 205), (229, 229, 229),
        (127, 127, 127), (255, 0, 0),   (0, 255, 0),   (255, 255, 0),
        (92, 92, 255),   (255, 0, 255), (0, 255, 255), (255, 255, 255),
    ];
    let (r, g, b) = if i < 16 {
        BASE[i as usize]
    } else if i < 232 {
        let i = i - 16;
        let conv = |c: u8| -> u8 {
            if c == 0 {
                0
            } else {
                55 + 40 * c
            }
        };
        (conv(i / 36), conv((i / 6) % 6), conv(i % 6))
    } else {
        let level = 8 + 10 * (i - 232);
        (level, level, level)
    };
    egui::Color32::from_rgb(r, g, b)
}
