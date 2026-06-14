//! Data model: a parsed script parameter and its current UI value.

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParamKind {
    Text,
    FilePath,
    FolderPath,
    Number,
    Bool,
    Choice,
}

#[derive(Clone, Debug)]
pub struct Param {
    /// PowerShell parameter name without the leading `$` (e.g. "InputFolder").
    pub name: String,
    /// Human-friendly label derived from the name (e.g. "Input Folder").
    pub label: String,
    pub kind: ParamKind,
    pub required: bool,
    /// `[switch]` (presence = true) vs `[bool]` (needs `-Name:$true`).
    pub is_switch: bool,
    /// Allowed values for a `[ValidateSet(...)]` parameter.
    pub choices: Vec<String>,
    /// Current value for Text / FilePath / FolderPath / Number / Choice.
    pub value: String,
    /// Current value for Bool.
    pub bool_value: bool,
    /// Batch positional argument index (None for PowerShell named params).
    pub position: Option<u32>,
    /// True if added/edited by the user via the mapper (not auto-detected).
    pub custom: bool,
    /// Inject the value as an environment variable (`$env:name`) before running.
    pub as_env: bool,
}
