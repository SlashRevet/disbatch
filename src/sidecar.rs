//! Per-script sidecar (`<script>.disbatch.json`) stored next to the script.
//!
//! Holds team-shareable extras and remembered state: usage hints, mapper
//! control definitions, and the last-used input values. Commit the sidecar
//! alongside the `.ps1`/`.bat` and your team gets the hints + custom controls
//! when they open it.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A control as defined/overridden by the user in the mapper.
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct ControlDef {
    pub name: String,
    pub label: String,
    pub kind: String, // text | file | folder | number | bool | choice
    pub required: bool,
    pub default: String,
    pub choices: Vec<String>,
    pub position: Option<u32>,
    pub custom: bool,
    pub as_env: bool,
}

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct Sidecar {
    /// Free-text notes on how to use the script.
    pub hints: String,
    /// Mapper controls. If non-empty, they replace auto-detection on open.
    pub controls: Vec<ControlDef>,
    /// Remembered text/number/choice/path values, keyed by control name.
    pub values: HashMap<String, String>,
    /// Remembered checkbox values, keyed by control name.
    pub bool_values: HashMap<String, bool>,
}

impl Sidecar {
    /// `<script>.disbatch.json` next to the script.
    pub fn path_for(script: &Path) -> PathBuf {
        let mut s = script.as_os_str().to_owned();
        s.push(".disbatch.json");
        PathBuf::from(s)
    }

    /// Load the sidecar for `script`, or a default if missing/unreadable.
    pub fn load(script: &Path) -> Sidecar {
        std::fs::read_to_string(Self::path_for(script))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Write the sidecar next to `script`.
    pub fn save(&self, script: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self).unwrap_or_default();
        std::fs::write(Self::path_for(script), json)
    }
}
