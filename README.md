# Disbatch

**Give your PowerShell scripts an instant GUI.** Point Disbatch at a `.ps1` and it
reads the script's `param()` block, generates the matching controls — folder
pickers, checkboxes, dropdowns, number and text fields — statically analyses the
script for risky behaviour, and runs it inside a live embedded terminal with a
progress bar. One self-contained Windows `.exe`, **100% offline**, no runtime to
install.

> ⚠️ **"Vibecoded" — use with caution.** Disbatch was built quickly and iteratively
> (largely with an AI coding assistant) as a personal project. It works, but it has
> **not** been hardened, audited, or extensively tested. Run it at your own risk —
> especially when pointing it at scripts you don't fully trust. The built-in risk
> analyzer is a heuristic aid, **not** a security guarantee.

## Features

- **Auto-generated UI** — a PowerShell `param()` block becomes a form, no config required.
- **Two tabs**
  - **Script** — read-only preview + a risk-analysis panel + metrics.
  - **Controls** — the generated form, a live command preview, and Run.
- **Embedded ConPTY terminal** — a real, interactive PowerShell terminal (the same
  pseudo-console mechanism Windows Terminal uses), rendered inside the app. Run sends
  the composed command into it; you can also type in it directly.
- **Static risk analyzer** — flags risky capabilities (download-and-run, encoded
  commands, keyboard hooks, persistence, shadow-copy deletion, …). Danger-level
  findings **gate the Run button** until you acknowledge them. *Heuristic, not
  antivirus* — it surfaces what a script can do so you can make an informed choice
  before running it.
- **Progress bar** — driven by an opt-in `@progress` / `@status` protocol.
- **Dark, offline, single exe** — no telemetry, no network calls, nothing to install.

## How parameters map to controls

| PowerShell                         | Control          |
| ---------------------------------- | ---------------- |
| `[switch]$Recurse`                 | checkbox         |
| `[ValidateSet("A","B")][string]$X` | dropdown         |
| `[int]$Threads = 4`                | number field (4) |
| `[string]$InputFolder`             | folder picker    |
| `[string]$LogFile` / `...Path`     | file picker      |
| `[string]$Name`                    | text field       |
| `[Parameter(Mandatory)]`           | required *       |

Defaults pre-fill the controls; mandatory parameters must be set before Run.

## Progress + status protocol (opt-in)

Print these from your script and Disbatch drives the bar (the lines also appear in
the terminal):

```powershell
Write-Host "@progress 42"           # 0-100 -> progress bar
Write-Host "@status Copying files"  # -> status label
```

## Build & run

Requires the Rust toolchain (the repo pins the MSVC toolchain via
`rust-toolchain.toml`).

```powershell
cargo run --release      # build + launch
cargo build --release    # -> target\release\disbatch.exe (single file)
```

Open `examples\demo.ps1` to see every control type generated, then hit **Run**.

## Safety note

Disbatch makes running scripts frictionless — which is exactly when it's easy to run
something you shouldn't. The risk analyzer is a speed-bump and an informed-consent
layer; it is **not** a replacement for antivirus, and obfuscated code can evade it.
Read what a script does before you run it.

## Roadmap

- Visual **mapper** to define/override controls auto-detection misses, with
  per-script sidecar persistence.
- **`.bat` / `.cmd`** parameter detection (`%1`, `set /p`, `set VAR=`).
- Inline risk highlighting in the preview.

## License

[MIT](LICENSE) © 2026 SlashRevet
