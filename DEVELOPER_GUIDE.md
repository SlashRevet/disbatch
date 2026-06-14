# Disbatch — Developer Guide

A map of the codebase for contributors: **where each function lives, what it does, how it works, and how the pieces connect.** Read the *Architecture at a glance* section first for the mental model, then dip into the per-module reference or the end-to-end walkthroughs as needed.

> Line references like `src/main.rs:97` are accurate as of the current commit but will drift as the code changes — the **module + function name** is the durable anchor; treat line numbers as a hint.

---

## 1. What Disbatch is

A single-binary Windows desktop app (Rust + [`egui`](https://github.com/emilk/egui)) that turns a PowerShell `.ps1` or batch `.bat`/`.cmd` script into a GUI:

1. **Parse** the script's parameters into typed controls (folder/file pickers, checkboxes, dropdowns, number/text fields).
2. **Analyse** the script text for risky capabilities and surface them.
3. **Run** it inside an embedded ConPTY terminal, streaming output and a progress bar.

It is dark-only, 100% offline, and persists per-script state (hints, control mappings, last-used values) to a `<script>.disbatch.json` sidecar.

---

## 2. Architecture at a glance

### The one data structure everything orbits

A **`Param`** (`src/model.rs`) is *one generated control*. The whole app is a pipeline that **produces** `Param`s (parser), **renders/edits** them (UI), **persists** them (sidecar), and **turns them into a command line** (runner):

```
 script text
   │  parser.rs ─────────────►  Vec<Param>  ◄──────── sidecar.rs (saved controls/values)
   │                               │
   │  analyzer.rs ──► Vec<Finding> │
   │                       │       │
   ▼                       ▼       ▼
 main.rs (egui UI):  Script tab (preview + findings)   Controls tab (the Param widgets)
                                                          │
                                       compose_command()  ▼
                                          "& 'x.ps1' -A 'v' ..."
                                                          │
                              terminal.rs: send_line() ──►│──► ConPTY ──► powershell.exe
```

### Two threads

| Thread | Owns | Job |
|---|---|---|
| **UI / main thread** | the whole `DisbatchApp` | runs `eframe`'s `update()` every frame; renders panels; mutates state on clicks |
| **Terminal reader thread** | a clone of the PTY reader | blocks on the ConPTY pipe, feeds bytes into the `vt100` parser, answers terminal queries, scrapes `@progress`/`@status`, and calls `ctx.request_repaint()` to wake the UI |

They share exactly three things, each behind an `Arc<Mutex<…>>`: the **vt100 parser** (screen state), the **`TermProgress`** (progress scrape), and the **writer** (so the reader can answer terminal queries without bouncing through the UI thread). Critical sections are tiny.

### Repository map

| File | Responsibility |
|---|---|
| `src/main.rs` | The `eframe` app: all state, the whole UI (panels, tabs, mapper), and the glue logic (open/run/compose/persist). ~960 lines, the biggest file. |
| `src/model.rs` | `Param` + `ParamKind` — the shared data model. |
| `src/parser.rs` | Turns script text → `Vec<Param>` (PowerShell `param()` block, batch `%N`). |
| `src/analyzer.rs` | Heuristic risk scanner → `Vec<Finding>`. |
| `src/terminal.rs` | The embedded ConPTY terminal (spawn, vt100 render, input, pause/stop/clear). |
| `src/sidecar.rs` | `<script>.disbatch.json` load/save (hints, control defs, remembered values). |
| `Cargo.toml` / `rust-toolchain.toml` | Dependencies, release profile, MSVC pin. |
| `.github/workflows/release.yml` | Tag-triggered CI that builds + publishes the Windows exe. |

---

## 3. Core data model — `src/model.rs`

### `ParamKind` — `model.rs:4`

```rust
enum ParamKind { Text, FilePath, FolderPath, Number, Bool, Choice }
```

Derives **`Copy`** so it can be compared, passed by value, and stored without move friction. Each variant maps to a widget (see `param_widget`):

| Variant | Widget |
|---|---|
| `Text` | single-line text field |
| `FilePath` | text field + **Browse** (rfd file picker) |
| `FolderPath` | text field + **Browse** (rfd folder picker) |
| `Number` | text field (numeric) |
| `Bool` | checkbox (uses `bool_value`, not `value`) |
| `Choice` | dropdown (populated from `choices`) |

### `Param` — `model.rs:13`

One `Param` = one control. Produced by `parser.rs`, rendered/edited by `main.rs`, serialized as `ControlDef` by `sidecar.rs`.

| Field | Type | Meaning / who uses it |
|---|---|---|
| `name` | `String` | PS param name without `$` (`"InputFolder"`), or `"argN"` for batch. Doubles as the CLI arg name (`-InputFolder`) and the sidecar map key. |
| `label` | `String` | Human label from `humanize(name)` (`"Input Folder"`). |
| `kind` | `ParamKind` | Which widget + how the value is emitted. |
| `required` | `bool` | From `[Parameter(Mandatory)]`. Blocks Run until filled. |
| `is_switch` | `bool` | `[switch]` (→ `-Name`) vs `[bool]` (→ `-Name:$true`). Both have `kind == Bool`. |
| `choices` | `Vec<String>` | From `[ValidateSet(...)]`; the dropdown items. |
| `value` | `String` | Current value for text/number/choice/path kinds. Mutated by the widget directly (it's a `&mut` into `params`, so edits persist with no save step). |
| `bool_value` | `bool` | Current value for `Bool`. |
| `position` | `Option<u32>` | Batch only: `Some(n)` = positional `%n`; `None` = PS named. Drives positional-vs-named command building. |
| `custom` | `bool` | `true` if the user added/edited it in the mapper (vs auto-detected). |
| `as_env` | `bool` | If `true`, inject as `$env:NAME = 'value';` before the call instead of as an argument. |

`parser.rs` always emits `custom=false, as_env=false`; those are set later by the mapper.

---

## 4. Parameter detection — `src/parser.rs`

Pure regex + character-scanning (no real PowerShell AST). Two public entry points; everything else is a private helper.

**Called from `main.rs`:** `open_script` (on file open), `reanalyze` (after inline edits), `redetect` (mapper "Re-detect").

### `parse_powershell(source) -> Vec<Param>` — `parser.rs:11`

A four-stage pipeline:

```
strip_block_comments → extract_param_block → split_top_level → parse_one (per decl)
```

- **`strip_block_comments`** (`:24`) — removes `<# … #>` via `(?s)<#.*?#>`, replacing with a space to keep offsets stable.
- **`extract_param_block`** (`:33`) — regex-finds `param(`, then a **character-by-character balanced scan** that tracks paren/bracket/brace depth *and* string state (`'…'`, `"…"` with backtick-escape) *and* `#` line comments, so a `)` inside a default like `= @("a","b")` doesn't close the block early. Returns the interior text.
- **`split_top_level`** (`:93`) — same string/bracket/comment-aware state machine, splitting on commas only at depth 0. Drops `#` line-comment text entirely.
- **`parse_one`** (`:152`) — per declaration: collect `[...]` attributes; strip them to find the first `$name`; extract the `= default`. Decide `kind` by precedence: `[switch]`/`[bool]`→`Bool` → `[ValidateSet]`→`Choice` → numeric type (`int`/`long`/`double`/…)→`Number` → else `kind_from_name`. Detect `Mandatory` (any attr containing `mandatory` not followed by `$false`). Pull `ValidateSet` choices via `["']([^"']*)["']`. Resolve the default into `value`/`bool_value`.
- **`kind_from_name`** (`:276`) — name heuristic when type is just `[string]`: contains `folder`/`dir` → folder picker; `file`/`path` → file picker; else text.
- **`strip_quotes`** (`:287`), **`humanize`** (`:301`) — unquote defaults; CamelCase/underscore → label (`OutFile` → `Out File`).

### `parse_batch(source) -> Vec<Param>` — `parser.rs:250`

Regex `%(?:~[a-zA-Z]*)?([1-9])` matches `%1`–`%9` and modifier forms (`%~dp1`); dedupes/sorts via a `BTreeSet`; emits one `Param { name:"argN", kind:Text, position:Some(n) }` per index.

**Limits:** regex heuristics, not a parser. Misses type aliases (`[System.IO.DirectoryInfo]`), `ValidateSet` of non-strings, unusual multi-line formatting; `%%1` for-loop vars produce spurious params. The mapper exists to fix anything it gets wrong.

---

## 5. Risk analyzer — `src/analyzer.rs`

A fully-offline static scanner. **It is an informed-consent speed-bump, NOT antivirus** — false positives and false negatives are expected; obfuscation evades it.

- **`Severity`** (`:14`) — `Warning` | `Caution` (deliberately reduced from a former Danger/Warning/Info to avoid over-alarming). `rank()` sorts Warnings first; `label()` → `"WARNING"`/`"CAUTION"`.
- **`Finding`** (`:34`) — `{ severity, category, title, line (1-based), snippet (≤160 chars) }`.
- **`Rule`** (`:43`) + **`rule()`** helper — `{ severity, category, title, Regex }`. Patterns are all `(?i)`.
- **`rules()` / `build_rules()`** (`:59`) — ~33 rules compiled **once** behind a `OnceLock`. Categories: download-&-execute, obfuscation/encoded commands, LOLBins, keylogging/native-API, persistence, destructive/ransomware, stealth, credential-theft, defense-evasion/policy. **Heuristic for severity:** patterns implying *active harm* (remote-code-from-memory, disabling Defender, keystroke capture, shadow-copy deletion) → `Warning`; suspicious-but-often-legitimate (network calls, registry writes, P/Invoke, hidden window) → `Caution`.
- **`analyze(source) -> Vec<Finding>`** (`:113`) — scans every non-blank line against every rule (one line can yield several findings); sorts by severity-desc then line-asc.
- **`has_warning(findings)`** (`:141`) — the single boolean that **gates the Run button**.
- **`counts(findings)`** (`:146`) — `(warnings, cautions)` for the filter chips.

**Flow into the UI:** `counts` → the two clickable severity chips (toggle `severity_filter`); the findings list (click → `highlight_line`/`scroll_pending`, jump to the line); a `HashMap<line, Severity>` → inline background tint in the preview; `has_warning` → the "I understand the risks" acknowledgment gate.

**To add a rule:** append a `rule(severity, "Category", "Title", r"(?i)…")` to `build_rules()`. Choose `Warning` only for high-confidence active harm.

---

## 6. The embedded terminal — `src/terminal.rs`

The most intricate module: a real interactive terminal. It attaches `powershell.exe` to a Windows pseudo-console (ConPTY) via `portable-pty`, parses the byte stream with `vt100` into a screen grid, and paints that grid by hand with egui — the same approach Windows Terminal uses.

### `TermProgress` — `:19`
`{ value: f32 (0..=1), status: String, seen: bool }`. The scrape target for the opt-in protocol; `seen` controls whether `main.rs` shows the progress bar at all. `progress()` clones it out from behind the mutex so the UI never blocks.

### `Terminal` struct — `:26`
| Field | Why |
|---|---|
| `parser: Arc<Mutex<vt100::Parser>>` | shared screen state — reader writes (`process`), UI reads (`screen()`) |
| `progress: Arc<Mutex<TermProgress>>` | reader writes scraped progress, UI reads |
| `writer: Arc<Mutex<Box<dyn Write>>>` | PTY write half; shared so the **reader thread** can send query replies |
| `master` | PTY master, used only for `resize()` |
| `child` | the powershell process handle, used in `Drop` to `kill()` |
| `pid: Option<u32>` | from `child.process_id()`; target for suspend/resume |
| `paused: bool` | UI-side shadow of suspend state |
| `rows/cols` | last size (guards redundant resizes) |
| `font_size` | monospace size → drives the cell grid |

### `Terminal::new(ctx, shell, cwd)` — `:41`
Opens a ConPTY pair (`PtySize { rows:24, cols:80 }` default), builds `powershell.exe -NoLogo -ExecutionPolicy Bypass` (see *Gotchas*), spawns it into the slave side, captures the pid, creates the shared `vt100::Parser` (5000-line scrollback), and **spawns the reader thread**. The reader loop: read ≤8 KiB → `parser.process(bytes)` → read back cursor pos → `query_replies()` and write any replies → regex-scrape `@progress`/`@status` → **`ctx.request_repaint()`** (wakes egui from outside its thread, so output appears within one frame).

### Methods
- **`send` / `send_line`** (`:133`) — lock writer, `write_all` + flush. `send_line` appends `\r` (CR = Enter). Used by Run (`send_line(cmd)`), keyboard input, `interrupt` (`send(&[0x03])`), `clear` (`send_line("Clear-Host")`).
- **`progress` / `reset_progress`** (`:145`) — clone/zero the progress; `reset_progress` is called at the start of each Run.
- **`is_paused` / `toggle_pause` / `interrupt` / `clear`** (`:155`) — `toggle_pause` calls `suspend_resume(pid, …)`; `interrupt` resumes first (a signal to a frozen process is a no-op) then sends Ctrl+C.
- **`resize`** (`:181`) — no-ops if unchanged, else `master.resize()` + `parser.screen_mut().set_size()` (both must stay in sync).
- **`ui(ui)`** (`:201`) — measure cell size from the monospace font → compute cols/rows → `resize` → `allocate_painter` → lock parser, walk cells building a `LayoutJob` (adjacent same-style cells grouped into runs via `append_run`) → draw galley → draw cursor rect → if focused, draw a green border and translate `ui.input` events through `key_to_bytes` back into the PTY.
- **`Drop`** (`:322`) — `child.kill()`.

### Module functions
- **`suspend_resume(pid, suspend)`** (`:330`) — the OS-level freeze behind **Pause**. `OpenProcess(PROCESS_SUSPEND_RESUME)` → `GetProcAddress(ntdll, "NtSuspendProcess"|"NtResumeProcess")` → transmute → call. Uses ntdll because Win32 has no single-call whole-process suspend (only per-thread). **Caveat:** freezes only `powershell.exe`, not child processes it spawned.
- **`query_replies(data, cursor)`** (`:418`) — **critical, fixed a real "frozen terminal" bug.** PSReadLine sends a cursor-position query `ESC[6n` and *blocks until it gets a reply*; with no reply the prompt never draws and the terminal looks dead. This scans raw bytes for `ESC[6n` (→ reply `ESC[<row>;<col>R`), `ESC[5n` (→ `ESC[0n`), `ESC[c` (→ `ESC[?1;0c`) and the reader writes them straight back.
- **`key_to_bytes(key, mods)`** (`:382`) — control/navigation keys → VT byte sequences (Enter→`\r`, Backspace→`\x7f`, arrows→`ESC[A…`, Ctrl+A–Z→1–26). Returns `None` for printable chars (those arrive as `Event::Text`, avoiding double input).
- **`to_color32` / `ansi_idx`** (`:445`) — vt100 color → `Color32`, including the xterm-256 palette (16 base, 6×6×6 cube, 24-step greyscale).
- **`append_run`** (`:360`) — appends one style-grouped run to the `LayoutJob`.

---

## 7. Persistence — `src/sidecar.rs`

Writes `<script>.disbatch.json` next to the script (designed to be committed so teammates inherit hints + mappings).

- **`ControlDef`** (`:13`) — the serialized form of a `Param` (`kind` as a string tag for readability/forward-compat; `#[serde(default)]`). No `value`/`bool_value` — those live in the maps below.
- **`Sidecar`** (`:27`) — `{ hints: String, controls: Vec<ControlDef>, values: HashMap<name,String>, bool_values: HashMap<name,bool> }`, all `#[serde(default)]` (a missing file is indistinguishable from an empty one).
- **`path_for` / `load` / `save`** (`:42`/`:49`/`:57`) — `load` never panics (returns `default()` on any error); `save` pretty-prints via `serde_json`.

**How `main.rs` uses it:** on open, `Sidecar::load`; if `controls` is non-empty it **replaces** the auto-detected params (via `def_to_param`); `apply_saved_values` overlays remembered `values`/`bool_values`. `save_sidecar` (snapshots current values + writes) fires on **Run**, **Save hints**, and **Save mapping**; **Re-detect** clears `controls` then re-parses.

---

## 8. The app — `src/main.rs`

### 8.1 State & lifecycle

- **`main()`** (`:24`) — `eframe::run_native`, 920×780 window, forces `Visuals::dark()`, builds `DisbatchApp::default()`. The `#![cfg_attr(not(debug_assertions), windows_subsystem="windows")]` (`:1`) suppresses the console window in release.
- **`Tab`** (`:42`) — `Script` | `Controls`.
- **`DisbatchApp`** (`:48`) — every field:

| Field | Role |
|---|---|
| `tab` | which central panel shows |
| `script_path: Option<PathBuf>` | open script; guards most logic |
| `source: String` | full script text |
| `editable: bool` | preview = editable `TextEdit` vs read-only annotated view |
| `params: Vec<Param>` | the live controls |
| `note: String` | status line when nothing's loaded / on error |
| `findings: Vec<Finding>` | analyzer output |
| `risk_ack: bool` | "run anyway" checkbox; reset on open/reanalyze |
| `terminal: Option<Terminal>` | the live ConPTY session |
| `terminal_err: Option<String>` | spawn failure (shown red; suppresses auto-retry) |
| `highlight_line: Option<usize>` | gold-tinted line from a finding click |
| `scroll_pending: bool` | one-shot: scroll the highlight into view next frame |
| `severity_filter: Option<Severity>` | filters the findings list |
| `sidecar: Sidecar` | in-memory mirror of the JSON |
| `mapping_mode: bool` | mapper edit mode (grid grows 2→4 columns) |
| `picking_for: Option<usize>` | pick mode: clicking a token binds control `idx` |

- **`open_script(path)`** (`:97`) — the reset-and-load core: read file → parse (ps1/bat) → `analyze` → reset ephemeral UI state → `cd` the terminal into the folder → load sidecar (override params with saved `controls`, restore values) → store `script_path`.
- **`reanalyze`** (`:144`) / **`redetect`** (`:286`) — re-parse + re-analyze after inline edits / discard mapper overrides.
- **`spawn_terminal(ctx)`** (`:152`) — `Terminal::new`; stores it or the error. `ctx` is handed in so the reader thread can repaint.
- **`compose_command() -> String`** (`:166`) — builds the PS line in three combinable parts: (1) **env prefix** — `as_env` params become `$env:NAME='v'; `; (2) **batch positional** — params with `position`, sorted, appended as quoted positionals; (3) **PS named** — the rest as `-Name 'v'` (or `-Switch` / `-Name:$bool`). All values via `ps_quote` (`:945`, single-quote + double-embedded-quotes).
- **`missing_required`** (`:237`), **`run(ctx)`** (`:245`) — gate check; `run` ensures a terminal, composes, `reset_progress`, `send_line`, then `save_sidecar`.
- **`save_sidecar` / `apply_saved_values`** (`:258`/`:274`) — the persist/restore halves of the value round-trip.
- **`update()`** (`:800`) — the frame loop: (1) auto-spawn the terminal on frame 1; (2) handle dropped files → `open_script`; (3) top header + tab bar; (4) bottom resizable **Terminal** dock (New session / Pause-Resume / Stop / Clear + progress bar + `terminal.ui()`); (5) `CentralPanel` → `script_tab` or `controls_tab`.

### 8.2 UI rendering

- **`script_tab(ui)`** (`:304`) — a right `SidePanel` "Analysis" (severity-filter chips, metrics, findings list with click-to-jump) + the central **Preview**. Read-only preview renders **line-by-line**: a gutter line-number + a wrapping `Label` (`horizontal_top` keeps them aligned; the number shows only on the first visual row of a wrapped line), with inline severity tinting and the gold highlight. **Pick mode** (`picking_for`) swaps each line for clickable **token chips** built from `token_spans`.
- **`controls_tab(ui, ctx)`** (`:608`) — Hints `CollapsingHeader`; the mapper toolbar (Edit controls / Add control / Save mapping / Re-detect); the controls `Grid` (2 cols normal, 4 cols in edit mode: label / `kind_combo` / required / bind+remove); the dropdown-options editor; the **Command preview** + **Copy** (`ui.output_mut(|o| o.copied_text = …)`); the **Run** button with `has_warning` + `missing_required` gating.
- **`param_widget(ui, p)`** (`:893`) — the per-`Param` value widget (checkbox / combo / number / path+Browse / text); writes straight into `p.value`/`p.bool_value`.
- Helpers: `kind_combo` (`:1135`, type picker), `kind_label`/`kind_to_str`/`kind_from_str` (`:949`+), `sev_color` (`:937`), `binding_label` (`:1125`, the `→ %1`/`→ $env:X`/`→ -Name` tag), the color constants `RED`/`WARNING`/`CAUTION`/`GRAY` (`:19`).
- **Pick-to-bind plumbing:** `Bound` enum (`:1039`), `token_spans(line, is_batch)` (`:1047`, finds non-overlapping clickable tokens: `$var`/`$env:X`/`%N`/`set VAR`), `apply_binding(p, b)` (`:1104`, writes `position`/`as_env`/`name`), `param_to_def`/`def_to_param`/`custom_param` (`:982`/`:1000`/`:1021`).

---

## 9. End-to-end walkthroughs

### Open (or drop) a script
`update` picks up the dropped path or the file-dialog result → **`open_script`**: read to `source` → `parse_powershell`/`parse_batch` → `params` → `analyze` → `findings` → reset `risk_ack`/`editable`/`highlight_line`/`severity_filter` → if a terminal exists, `Set-Location` into the folder → `Sidecar::load`; if it has saved `controls`, rebuild `params` via `def_to_param` → `apply_saved_values` → store `script_path`. Next frame renders the populated tabs.

### Click Run
`controls_tab` computes `can_run = missing_required().is_empty() && (!has_warning(findings) || risk_ack)` → button enabled → **`run`**: ensure terminal → **`compose_command`** (env prefix + positional/named) → `reset_progress` → **`terminal.send_line(cmd)`** (writes to the ConPTY as if typed) → **`save_sidecar`** (remembers the inputs). The reader thread streams output back, scraping `@progress`/`@status` into the bar.

### Pick-to-bind a control
Controls tab → click a control's binding button → `pick = Some(idx)` (applied after the grid closure) → `picking_for = Some(idx)`, `tab = Script` → next frame, `script_tab` renders the green banner and token chips → user clicks a chip → `bind_click = Some((idx, Bound))` (applied after the scroll closure) → `apply_binding(&mut params[idx], b)` → `picking_for = None`, `tab = Controls`. The control's `binding_label` now reflects the chosen token, and `compose_command` emits it accordingly.

### Terminal lifecycle
`spawn_terminal` → `Terminal::new` opens ConPTY, launches powershell, starts the reader thread → every frame `terminal.ui()` resizes + paints the vt100 screen and feeds keystrokes → Pause/Stop/Clear call `toggle_pause`/`interrupt`/`clear` → `Drop` kills the child on app exit or "New session".

---

## 10. Patterns & gotchas (read before you edit `main.rs`)

1. **Deferred-click locals.** egui closures borrow `self` for their whole body, so you can't mutate `self` inside a closure that's already reading `self.findings`/`self.params`. The codebase collects the *intent* into a local declared **before** the closure (`jump_to`, `pick`, `remove`, `bind_click`, `remove_choice`, `add_choice`) and **applies it after** the closure closes. Follow this pattern for any new click handler.
2. **`ui.push_id` for loop widgets.** In pick mode every token is a `Button`; identical token text on different lines would collide on egui's auto-generated `Id` (state/clicks bleed). Pick mode nests `push_id(line_index)` then `push_id(byte_offset)` for a unique `(line, offset)` id. `kind_combo` uses `from_id_source(("kind", idx))` for the same reason. This fixed a real "🔥 widget ID clash" bug.
3. **`-ExecutionPolicy Bypass`** is mandatory — Windows blocks unsigned local `.ps1` by default, and running scripts is the app's entire purpose. It applies only to the spawned subprocess, not the system. The **risk analyzer is the safety gate** that replaces the policy's role.
4. **The DSR/PSReadLine reply** (`query_replies`) is load-bearing. Remove it and the terminal freezes with no prompt. If you ever swap the shell or the VT pipeline, keep answering `ESC[6n`.
5. **Pause is process-level, not tree-level.** `NtSuspendProcess` freezes only `powershell.exe`; children it spawned keep running. Documented limitation.
6. **`panic = "abort"` in release** — any `unwrap()`/`panic!` is a hard crash with no unwinding. Prefer graceful handling on user-facing paths.
7. **Parsing is heuristic.** When you hit a script the parser mis-reads, the fix is usually a mapper feature, not more regex.

---

## 11. Build, dependencies & release

### Dependencies (`Cargo.toml`) and why
| Crate | Why |
|---|---|
| `eframe` 0.27 | the GUI (egui + winit + glow) |
| `rfd` 0.14 | native file/folder pickers |
| `regex` 1 | parser + analyzer |
| `portable-pty` 0.9 | ConPTY (spawn the shell, read/write its pipes) |
| `vt100` 0.16 | parse the terminal byte stream into a screen grid |
| `anyhow` 1 | error type for terminal glue |
| `serde` + `serde_json` 1 | the sidecar JSON |
| `winapi` 0.3 (`processthreadsapi`, `handleapi`, `libloaderapi`, `winnt`) | `OpenProcess` + ntdll for Pause |

### `[profile.release]`
`opt-level="z"` (size) · `lto=true` (cross-crate inlining/strip) · `strip=true` (no symbols) · `panic="abort"` (no unwinding) · `codegen-units=1` (max optimization). Net: a small (~4 MB) self-contained exe.

### Toolchain — `rust-toolchain.toml`
Pinned to `stable-x86_64-pc-windows-msvc`. MSVC is required for the Windows system libs; the pin keeps local and CI builds identical.

### Release — `.github/workflows/release.yml`
On a `v*` tag: `windows-latest` runs `cargo build --release` and `softprops/action-gh-release@v2` publishes `disbatch.exe` as a Release asset. **Why CI?** The dev machine has **Smart App Control** enforced, which blocks running the freshly-built (unsigned) cargo build-script exes locally (`os error 4551`) — so release builds go through CI. The published exe is unsigned, so the release notes tell users *More info → Run anyway*. To cut a release: `git tag vX.Y.Z && git push origin vX.Y.Z`.

---

## 12. "Where is what" — feature → code index

| Feature | Lives in |
|---|---|
| Window setup, dark theme | `main.rs::main` |
| Open a script (parse + analyze + sidecar) | `main.rs::open_script` |
| Drag-and-drop open | `main.rs::update` (dropped_files) |
| PowerShell param detection | `parser.rs::parse_powershell` + helpers |
| Batch `%N` detection | `parser.rs::parse_batch` |
| Risk rules / severities | `analyzer.rs::build_rules`, `Severity` |
| Findings list + filter + jump | `main.rs::script_tab` (Analysis panel) |
| Inline risk tinting in preview | `main.rs::script_tab` (`risky` map) |
| Run-button gating | `main.rs::controls_tab` + `analyzer::has_warning` |
| The control widgets | `main.rs::param_widget` |
| Mapper (edit type/label/required, add/remove) | `main.rs::controls_tab` (edit mode) |
| Pick-to-bind tokens | `main.rs::token_spans` / `apply_binding` + pick-mode preview |
| Dropdown options editor | `main.rs::controls_tab` |
| Command building | `main.rs::compose_command` |
| Copy command | `main.rs::controls_tab` (📋 Copy) |
| Run the script | `main.rs::run` → `terminal.rs::send_line` |
| Embedded terminal render | `terminal.rs::ui` |
| Terminal spawn / reader thread | `terminal.rs::Terminal::new` |
| Progress bar protocol | `terminal.rs` reader scrape + `TermProgress` |
| Pause / Resume | `terminal.rs::toggle_pause` / `suspend_resume` |
| Stop (Ctrl+C) / Clear | `terminal.rs::interrupt` / `clear` |
| Keyboard → terminal | `terminal.rs::ui` + `key_to_bytes` |
| Terminal query replies (prompt fix) | `terminal.rs::query_replies` |
| Hints / remembered values / mappings | `sidecar.rs` + `main.rs::save_sidecar`/`apply_saved_values` |

---

## 13. Extending it — quick recipes

- **New analyzer rule:** add a `rule(Severity::…, "Category", "Title", r"(?i)…")` to `build_rules()` in `analyzer.rs`.
- **New control type:** add a `ParamKind` variant (`model.rs`) → handle it in `param_widget`, `kind_combo`, `kind_label`/`kind_to_str`/`kind_from_str`, and `compose_command`.
- **New terminal toolbar action:** add a method on `Terminal` (`terminal.rs`) and a button in the bottom dock in `update`.
- **Persist something new:** add a field to `Sidecar` (`#[serde(default)]`) and wire it in `save_sidecar`/`open_script`.
- **A new release:** `git tag vX.Y.Z && git push origin vX.Y.Z` — CI builds and publishes.
