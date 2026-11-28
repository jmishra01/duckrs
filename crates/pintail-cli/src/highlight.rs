// Ported from duckdb/tools/shell/shell_highlight.cpp (+ include/shell_highlight.hpp), the error
// tokenizer `Parser::TokenizeError` / `Parser::Tokenize` in src/parser/parser.cpp, the highlight
// tokenizer behavior of src/parser/peg/tokenizer/{base,highlight}_tokenizer.cpp, the statement
// highlighting of tools/shell/linenoise/highlighting.cpp, and the highlighting dot commands of
// tools/shell/shell_metadata_command.cpp @ rust-port-baseline

//! Shell highlighting: per-element colors, the dark/light/mixed color modes, error
//! highlighting (`ShellHighlight::PrintError`), SQL statement highlighting
//! (`Highlighting::Tokenize` / `HighlightText`) and `.display_colors`.
//!
//! The module is pure: every method returns the text to print and never writes to a stream.
//! The shell owns one [`Highlighter`] and routes the highlighting dot commands to it:
//!
//! | command | C++ callback | method |
//! |---|---|---|
//! | `.highlight on\|off` | `ToggleHighlighting` | [`Highlighter::set_enabled`] |
//! | `.highlight_colors COMPONENT COLOR [INTENSITY]` | `SetHighlightColors` | [`Highlighter::highlight_colors_command`] / [`Highlighter::set_color`] |
//! | `.highlight_errors on\|off` | `ToggleHighlightErrors` | [`Highlighter::set_highlight_errors`] |
//! | `.highlight_results on\|off` | `ToggleHighlightResult` | [`Highlighter::set_highlight_results`] |
//! | `.highlight_mode mixed\|dark\|light\|auto` | `ToggleHighlightMode` | [`Highlighter::toggle_mode`] |
//! | `.keyword` `.comment` `.error` `.cont` `.cont_sel` `.constant` | `SetHighlightingColor<T>` | [`Highlighter::deprecated_set_color`] |
//! | `.display_colors [bold\|underline]` | `DisplayColors` | [`Highlighter::display_colors`] |
//!
//! The shell's command table (`commands.rs`) has the rows (name, argument count, usage, help) of these
//! commands; [`usage_error`] formats the message the dispatcher prints for `PRINT_USAGE`.

use std::collections::HashSet;
use std::sync::OnceLock;

/// `ShellHighlight::ResetTerminalCode`.
pub const RESET_TERMINAL_CODE: &str = "\x1b[00m";

/// `PrintColor`: the terminal default, or one of the 256 terminal colors (0..=15 are the
/// classic colors, 16..=255 the extended ones).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrintColor {
    Standard,
    Code(u8),
}

impl PrintColor {
    const RED: Self = Self::Code(1);
    const GREEN: Self = Self::Code(2);
    const YELLOW: Self = Self::Code(3);
    const BLUE: Self = Self::Code(4);
    const MAGENTA: Self = Self::Code(5);
    const CYAN: Self = Self::Code(6);
    const GRAY: Self = Self::Code(8);
    const ORANGE4: Self = Self::Code(58);
    const DEEPSKYBLUE6: Self = Self::Code(25);
    const DODGERBLUE2: Self = Self::Code(27);
    const DODGERBLUE1: Self = Self::Code(33);
    const DEEPSKYBLUE1: Self = Self::Code(39);
    const DARKMAGENTA: Self = Self::Code(90);
    const RED3: Self = Self::Code(124);
    const DARKORANGE4: Self = Self::Code(166);
    const ORANGE3: Self = Self::Code(172);
    const INDIANRED3: Self = Self::Code(203);
    const DARKORANGE: Self = Self::Code(208);
    const ORCHID2: Self = Self::Code(212);
    const GOLD1: Self = Self::Code(220);

    /// `ShellHighlight::IsExtendedColor`: whether the color is outside the classic 16.
    pub fn is_extended(self) -> bool {
        matches!(self, Self::Code(code) if code >= 16)
    }

    /// The color's name in the `.highlight_colors` / `.display_colors` table.
    pub fn name(self) -> Option<&'static str> {
        match self {
            Self::Standard => None,
            Self::Code(code) => Some(COLORS[code as usize].0),
        }
    }

    /// `ShellHighlight::TryGetPrintColor`: looks a color up by name (case-insensitively). The
    /// error is the C++ message, which the shell prints through `PrintDatabaseError`.
    pub fn from_name(name: &str) -> Result<Self, String> {
        if let Some(code) = COLORS.iter().position(|c| c.0.eq_ignore_ascii_case(name)) {
            return Ok(Self::Code(code as u8));
        }
        let names: Vec<&str> = COLORS.iter().map(|c| c.0).collect();
        let mut error = format!("Unknown highlighting color '{name}'\n");
        error += &candidates_error_message(&names, name, "Did you mean");
        error += "\n";
        error += "Run '.display_colors' for a list of available colors.\n";
        Err(error)
    }
}

/// `PrintIntensity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrintIntensity {
    Standard,
    Bold,
    Underline,
    BoldUnderline,
    Italic,
    BoldItalic,
}

/// `HighlightMode`: `Automatic` until the terminal colors are detected or a mode is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HighlightMode {
    Automatic,
    Mixed,
    Dark,
    Light,
}

macro_rules! highlight_elements {
    ($($variant:ident = $name:literal, $color:expr, $intensity:ident;)*) => {
        /// `HighlightElementType`: the things the shell colors. [`Self::name`] is the name
        /// `.highlight_colors` accepts.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum HighlightElementType {
            $($variant,)*
        }

        impl HighlightElementType {
            /// Every element, in table order (the order of the "supported options" list).
            pub const ALL: &'static [HighlightElementType] = &[$(HighlightElementType::$variant,)*];

            /// The element's `.highlight_colors` name.
            pub fn name(self) -> &'static str {
                match self {
                    $(HighlightElementType::$variant => $name,)*
                }
            }
        }

        /// The `highlight_elements` table: the default (mixed-mode) colors.
        const DEFAULT_ELEMENTS: &[(PrintColor, PrintIntensity)] = &[$(($color, PrintIntensity::$intensity),)*];
    };
}

highlight_elements! {
    ErrorToken = "error", PrintColor::RED, Standard;
    Keyword = "keyword", PrintColor::GREEN, Standard;
    NumericConstant = "numeric_constant", PrintColor::YELLOW, Standard;
    StringConstant = "string_constant", PrintColor::YELLOW, Standard;
    LineIndicator = "line_indicator", PrintColor::Standard, Bold;
    DatabaseName = "database_name", PrintColor::ORANGE3, Standard;
    SchemaName = "schema_name", PrintColor::DEEPSKYBLUE1, Standard;
    TableName = "table_name", PrintColor::Standard, Bold;
    ColumnName = "column_name", PrintColor::Standard, Standard;
    ColumnType = "column_type", PrintColor::GRAY, Standard;
    NumericValue = "numeric_value", PrintColor::Standard, Standard;
    StringValue = "string_value", PrintColor::Standard, Standard;
    TemporalValue = "temporal_value", PrintColor::Standard, Standard;
    NullValue = "null_value", PrintColor::GRAY, Standard;
    Footer = "footer", PrintColor::GRAY, Standard;
    Layout = "layout", PrintColor::GRAY, Standard;
    StartupText = "startup_text", PrintColor::GRAY, Standard;
    StartupVersion = "startup_version", PrintColor::Standard, Standard;
    Continuation = "continuation", PrintColor::GRAY, Standard;
    ContinuationSelected = "continuation_selected", PrintColor::GREEN, Standard;
    Bracket = "bracket", PrintColor::Standard, Underline;
    Comment = "comment", PrintColor::GRAY, Standard;
    SuggestionCatalogName = "suggestion_catalog_name", PrintColor::ORANGE3, Standard;
    SuggestionSchemaName = "suggestion_schema_name", PrintColor::DEEPSKYBLUE1, Standard;
    SuggestionTableName = "suggestion_table_name", PrintColor::Standard, Standard;
    SuggestionColumnName = "suggestion_column_name", PrintColor::Standard, Standard;
    SuggestionFileName = "suggestion_file_name", PrintColor::Standard, Standard;
    SuggestionDirectoryName = "suggestion_directory_name", PrintColor::Standard, Bold;
    SuggestionFunctionName = "suggestion_function_name", PrintColor::Standard, Standard;
    SuggestionSettingName = "suggestion_setting_name", PrintColor::Standard, Standard;
    TableLayout = "table_layout", PrintColor::GRAY, Standard;
    ViewLayout = "view_layout", PrintColor::Standard, Standard;
    PrimaryKeyColumn = "primary_key_column", PrintColor::Standard, Underline;
    Prompt = "prompt", PrintColor::DARKORANGE, Bold;
    PromptConnect = "prompt_connect", PrintColor::DARKORANGE, Standard;
    ErrorEmphasis = "error_emphasis", PrintColor::RED, Bold;
    ErrorSuggestion = "error_suggestion", PrintColor::RED, Bold;
    LogTrace = "log_trace", PrintColor::BLUE, Bold;
    LogDebug = "log_debug", PrintColor::YELLOW, Bold;
    LogInfo = "log_info", PrintColor::GREEN, Bold;
    LogWarning = "log_warning", PrintColor::GRAY, Standard;
    ExplainLayout = "explain_layout", PrintColor::GRAY, Standard;
    ExplainOperator = "explain_operator", PrintColor::Standard, Bold;
    ExplainOperatorScan = "explain_operator_scan", PrintColor::GREEN, Bold;
    ExplainOperatorJoin = "explain_operator_join", PrintColor::CYAN, Bold;
    ExplainOperatorAggregate = "explain_operator_aggregate", PrintColor::MAGENTA, Bold;
    ExplainOperatorOrder = "explain_operator_order", PrintColor::YELLOW, Bold;
    ExplainRows = "explain_rows", PrintColor::Standard, Standard;
    ExplainDetailKey = "explain_detail_key", PrintColor::GRAY, Standard;
    ExplainDetailValue = "explain_detail_value", PrintColor::Standard, Standard;
    ExplainHeader = "explain_header", PrintColor::Standard, Bold;
    ExplainTimingCritical = "explain_timing_critical", PrintColor::RED, Bold;
    ExplainTimingHigh = "explain_timing_high", PrintColor::YELLOW, Standard;
    ExplainTimingModerate = "explain_timing_moderate", PrintColor::Standard, Standard;
    ExplainTimingLow = "explain_timing_low", PrintColor::GRAY, Standard;
    None = "none", PrintColor::Standard, Standard;
}

const ELEMENT_COUNT: usize = HighlightElementType::ALL.len();

impl HighlightElementType {
    /// `ShellHighlight::TryGetHighlightElement`: looks an element up by name
    /// (case-insensitively). The error is the C++ message, printed to stderr as-is.
    pub fn from_name(name: &str) -> Result<Self, String> {
        if let Some(element) = Self::ALL.iter().find(|e| e.name().eq_ignore_ascii_case(name)) {
            return Ok(*element);
        }
        let options: Vec<&str> = Self::ALL.iter().map(|e| e.name()).collect();
        Err(format!(
            "Unknown element '{name}', supported options: {}\n",
            options.join(", ")
        ))
    }
}

/// Why a highlighting command failed, and what the shell prints for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HighlightError {
    /// Printed to stderr as-is (`state.Print(PrintOutput::STDERR, ...)`); already ends in `\n`.
    Stderr(String),
    /// Printed through `ShellState::PrintDatabaseError` ([`Highlighter::database_error`]).
    DatabaseError(String),
    /// The command returned `PRINT_USAGE`: the dispatcher prints [`usage_error`] through
    /// `PrintDatabaseError`.
    Usage,
    /// The command failed and has printed everything already.
    Failed,
}

/// The message `ShellState::DoMetaCommand` prints (through `PrintDatabaseError`) when a command
/// returns `PRINT_USAGE`. `typed` is the command as the user typed it (`args[0]`, which may be
/// an abbreviation), `command` and `usage` come from the command table.
pub fn usage_error(typed: &str, command: &str, usage: &str) -> String {
    format!("Invalid Command Error: Invalid usage of command '.{typed}'\n\nUsage: '.{command} {usage}'")
}

/// `ShellHighlight::TerminalCode`: the escape sequence that starts `color` + `intensity`
/// (empty for the terminal default).
pub fn terminal_code(color: PrintColor, intensity: PrintIntensity) -> String {
    let mut code = String::from(match intensity {
        PrintIntensity::Bold => "\x1b[1m",
        PrintIntensity::Italic => "\x1b[3m",
        PrintIntensity::BoldItalic => "\x1b[1m\x1b[3m",
        PrintIntensity::Underline => "\x1b[4m",
        PrintIntensity::BoldUnderline => "\x1b[1m\x1b[4m",
        PrintIntensity::Standard => "",
    });
    if let PrintColor::Code(c) = color {
        match c {
            // the standard colors are \033[31m through \033[37m
            1..=7 => code += &format!("\x1b[{}m", 30 + u32::from(c)),
            // the bright colors are \033[90m through \033[97m
            8..=15 => code += &format!("\x1b[{}m", 90 + u32::from(c) - 8),
            // the extended colors (and black) are \033[38;5;{code}m
            _ => code += &format!("\x1b[38;5;{c}m"),
        }
    }
    code
}

/// The shell's highlighting state: the per-element colors, which of them the user set, the
/// color mode, whether highlighting is on (`highlighting_enabled`) and the `.highlight_errors`
/// / `.highlight_results` overrides.
///
/// Defaults (C++ `ShellState`): highlighting is enabled, except that `main` turns it off when
/// `NO_COLOR` is set to a non-empty value ([`Self::apply_no_color`]); `.highlight on` turns it
/// back on. Errors are highlighted when `stderr` is a console and results when `stdout` is,
/// unless `.highlight_errors` / `.highlight_results` forced them ([`Self::highlight_errors`],
/// [`Self::highlight_results`]). The mode starts as [`HighlightMode::Automatic`] with the
/// mixed-mode colors; the shell detects the terminal background lazily
/// ([`Self::needs_mode_detection`], [`Self::apply_detected_mode`]) or takes it from
/// `-dark-mode` / `-light-mode` ([`Self::set_mode`]).
#[derive(Debug, Clone)]
pub struct Highlighter {
    elements: [(PrintColor, PrintIntensity); ELEMENT_COUNT],
    user_configured: [bool; ELEMENT_COUNT],
    enabled: bool,
    mode: HighlightMode,
    highlight_errors: Option<bool>,
    highlight_results: Option<bool>,
}

impl Default for Highlighter {
    fn default() -> Self {
        Self::new()
    }
}

impl Highlighter {
    pub fn new() -> Self {
        let mut elements = [(PrintColor::Standard, PrintIntensity::Standard); ELEMENT_COUNT];
        elements.copy_from_slice(DEFAULT_ELEMENTS);
        Self {
            elements,
            user_configured: [false; ELEMENT_COUNT],
            enabled: true,
            mode: HighlightMode::Automatic,
            highlight_errors: None,
            highlight_results: None,
        }
    }

    /// `ShellHighlight::SetHighlighting` (`.highlight on|off`).
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// `ShellHighlight::IsEnabled`.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// What `main` does with the `NO_COLOR` environment variable (pass
    /// `std::env::var_os("NO_COLOR")`): a non-empty value disables highlighting.
    pub fn apply_no_color(&mut self, value: Option<&std::ffi::OsStr>) {
        if value.is_some_and(|v| !v.is_empty()) {
            self.enabled = false;
        }
    }

    /// `.highlight_errors on|off`.
    pub fn set_highlight_errors(&mut self, on: bool) {
        self.highlight_errors = Some(on);
    }

    /// `ShellState::HighlightErrors`: forced by `.highlight_errors`, else whether stderr is a
    /// console.
    pub fn highlight_errors(&self, stderr_is_console: bool) -> bool {
        self.highlight_errors.unwrap_or(stderr_is_console)
    }

    /// `.highlight_results on|off`.
    pub fn set_highlight_results(&mut self, on: bool) {
        self.highlight_results = Some(on);
    }

    /// `ShellState::HighlightResults`: forced by `.highlight_results`, else whether stdout is
    /// a console.
    pub fn highlight_results(&self, stdout_is_console: bool) -> bool {
        self.highlight_results.unwrap_or(stdout_is_console)
    }

    pub fn mode(&self) -> HighlightMode {
        self.mode
    }

    /// `-dark-mode` / `-light-mode` / `.highlight_mode`: records the mode and applies its
    /// colors (`ToggleMode`; `Automatic` changes no colors).
    pub fn set_mode(&mut self, mode: HighlightMode) {
        self.mode = mode;
        self.apply_mode_colors(mode);
    }

    /// `ShellState::DetectDarkLightMode` preconditions: highlighting is on, the mode is still
    /// automatic and stdout is a console. Detecting the background (querying the terminal or
    /// `COLORFGBG`) is left to the shell.
    pub fn needs_mode_detection(&self, stdout_is_console: bool) -> bool {
        self.enabled && self.mode == HighlightMode::Automatic && stdout_is_console
    }

    /// The end of `DetectDarkLightMode`: a dark or light background switches to that mode's
    /// colors; anything else records the mixed mode without changing colors.
    pub fn apply_detected_mode(&mut self, detected: HighlightMode) {
        match detected {
            HighlightMode::Dark | HighlightMode::Light => self.set_mode(detected),
            _ => self.mode = HighlightMode::Mixed,
        }
    }

    /// `.highlight_mode MODE` (`ToggleHighlightMode`): `mixed`, `dark`, `light` or `auto`
    /// (case-sensitive). Anything else is [`HighlightError::Usage`].
    pub fn toggle_mode(&mut self, mode: &str) -> Result<(), HighlightError> {
        let mode = match mode {
            "mixed" => HighlightMode::Mixed,
            "dark" => HighlightMode::Dark,
            "light" => HighlightMode::Light,
            "auto" => HighlightMode::Automatic,
            _ => return Err(HighlightError::Usage),
        };
        self.set_mode(mode);
        Ok(())
    }

    /// `ShellHighlight::ToggleMode`: the mode's colors, skipping elements the user configured.
    fn apply_mode_colors(&mut self, mode: HighlightMode) {
        use HighlightElementType as E;
        use PrintColor as C;
        use PrintIntensity::{Bold, Standard};
        let colors: &[(E, C, PrintIntensity)] = match mode {
            HighlightMode::Mixed => &[
                (E::Keyword, C::GREEN, Standard),
                (E::StringConstant, C::YELLOW, Standard),
                (E::NumericConstant, C::YELLOW, Standard),
                (E::ContinuationSelected, C::GREEN, Standard),
                (E::Prompt, C::DARKORANGE, Bold),
                (E::PromptConnect, C::DARKORANGE, Standard),
                (E::DatabaseName, C::ORANGE3, Standard),
                (E::SchemaName, C::DEEPSKYBLUE1, Standard),
            ],
            HighlightMode::Dark => &[
                (E::Keyword, C::DODGERBLUE1, Bold),
                (E::StringConstant, C::GOLD1, Standard),
                (E::NumericConstant, C::ORCHID2, Standard),
                (E::ContinuationSelected, C::DODGERBLUE1, Standard),
                (E::ErrorToken, C::INDIANRED3, Standard),
                (E::ErrorEmphasis, C::INDIANRED3, Bold),
                (E::ErrorSuggestion, C::INDIANRED3, Bold),
            ],
            HighlightMode::Light => &[
                (E::Keyword, C::DODGERBLUE2, Bold),
                (E::StringConstant, C::ORANGE4, Standard),
                (E::NumericConstant, C::DARKMAGENTA, Standard),
                (E::ContinuationSelected, C::DODGERBLUE2, Standard),
                (E::Prompt, C::DARKORANGE4, Bold),
                (E::PromptConnect, C::DARKORANGE4, Standard),
                (E::ErrorToken, C::RED3, Standard),
                (E::ErrorEmphasis, C::RED3, Bold),
                (E::ErrorSuggestion, C::RED3, Bold),
                (E::DatabaseName, C::DARKORANGE4, Standard),
                (E::SchemaName, C::DEEPSKYBLUE6, Standard),
            ],
            HighlightMode::Automatic => &[],
        };
        for &(element, color, intensity) in colors {
            self.set_element_color(element, color, intensity, false);
        }
    }

    /// `ShellHighlight::GetHighlightElement`: the element's current color and intensity.
    pub fn element(&self, element: HighlightElementType) -> (PrintColor, PrintIntensity) {
        self.elements[element as usize]
    }

    /// `ShellHighlight::SetColor(HighlightElementType, ...)`. A user-configured color is
    /// remembered and never overridden by a mode switch (`user_configured == false`).
    pub fn set_element_color(
        &mut self,
        element: HighlightElementType,
        color: PrintColor,
        intensity: PrintIntensity,
        user_configured: bool,
    ) {
        let index = element as usize;
        if user_configured {
            self.user_configured[index] = true;
        } else if self.user_configured[index] {
            return;
        }
        self.elements[index] = (color, intensity);
    }

    /// `ShellHighlight::SetColor(const char *, const char *, const char *)`: parses the
    /// element, color and intensity names. Errors: unknown element / intensity are
    /// [`HighlightError::Stderr`], an unknown color is [`HighlightError::DatabaseError`].
    pub fn set_color(&mut self, element: &str, color: &str, intensity: Option<&str>) -> Result<(), HighlightError> {
        let element = HighlightElementType::from_name(element).map_err(HighlightError::Stderr)?;
        let color = PrintColor::from_name(color).map_err(HighlightError::DatabaseError)?;
        let intensity = match intensity {
            None => PrintIntensity::Standard,
            Some(i) if i.eq_ignore_ascii_case("standard") => PrintIntensity::Standard,
            Some(i) if i.eq_ignore_ascii_case("bold") => PrintIntensity::Bold,
            Some(i) if i.eq_ignore_ascii_case("underline") => PrintIntensity::Underline,
            Some(i) if i.eq_ignore_ascii_case("bold_underline") => PrintIntensity::BoldUnderline,
            Some(i) => {
                return Err(HighlightError::Stderr(format!(
                    "Unknown intensity '{i}', supported options: standard, bold, underline\n"
                )));
            }
        };
        self.set_element_color(element, color, intensity, true);
        Ok(())
    }

    /// `.highlight_colors COMPONENT COLOR [INTENSITY]` (`SetHighlightColors`); `args` are the
    /// words after the command name. Two or three are required, else [`HighlightError::Usage`].
    pub fn highlight_colors_command(&mut self, args: &[&str]) -> Result<(), HighlightError> {
        match args {
            [element, color] => self.set_color(element, color, None),
            [element, color, intensity] => self.set_color(element, color, Some(intensity)),
            _ => Err(HighlightError::Usage),
        }
    }

    /// The deprecated `.keyword` / `.comment` / `.error` / `.cont` / `.cont_sel` / `.constant`
    /// `COLOR` commands (`SetHighlightingColor<T>`). Returns `None` for any other command;
    /// otherwise the text to print to stderr first and the outcome of the color change (as
    /// `.highlight_colors ELEMENT COLOR`). `.constant` only prints its message and fails.
    pub fn deprecated_set_color(&mut self, command: &str, color: &str) -> Option<(String, Result<(), HighlightError>)> {
        let element = match command {
            "comment" => "comment",
            "keyword" => "keyword",
            "error" => "error",
            "cont" => "continuation",
            "cont_sel" => "continuation_selected",
            "constant" => {
                let message = format!(
                    ".constant has been split into numeric_constant and string_constant, use .highlight_colors \
                     numeric_constant {color} and .highlight_colors string_constant {color} instead\n"
                );
                return Some((message, Err(HighlightError::Failed)));
            }
            _ => return None,
        };
        let warning = format!(
            "WARNING: .{command} [COLOR] will be removed in a future release, use .highlight_colors {element} {color} instead\n"
        );
        Some((warning, self.set_color(element, color, None)))
    }

    /// `ShellHighlight::PrintText(text, color, intensity)`: wraps `text` in the terminal codes
    /// when highlighting is enabled and the style is not the terminal default.
    pub fn paint(&self, text: &str, color: PrintColor, intensity: PrintIntensity) -> String {
        let mut out = Vec::new();
        self.paint_bytes(&mut out, text.as_bytes(), color, intensity);
        bytes_to_string(out)
    }

    fn paint_bytes(&self, out: &mut Vec<u8>, text: &[u8], color: PrintColor, intensity: PrintIntensity) {
        if (color == PrintColor::Standard && intensity == PrintIntensity::Standard) || !self.enabled {
            out.extend_from_slice(text);
            return;
        }
        let code = terminal_code(color, intensity);
        if code.is_empty() {
            out.extend_from_slice(text);
            return;
        }
        out.extend_from_slice(code.as_bytes());
        out.extend_from_slice(text);
        out.extend_from_slice(RESET_TERMINAL_CODE.as_bytes());
    }

    fn paint_element(&self, out: &mut Vec<u8>, text: &[u8], element: HighlightElementType) {
        let (color, intensity) = self.element(element);
        self.paint_bytes(out, text, color, intensity);
    }

    /// `ShellHighlight::PrintText(text, element)`.
    pub fn text(&self, text: &str, element: HighlightElementType) -> String {
        let (color, intensity) = self.element(element);
        self.paint(text, color, intensity)
    }

    /// `ShellState::PrintDatabaseError` (outside agent mode): the highlighted error when
    /// `highlight_errors` ([`Self::highlight_errors`]) holds, else the message plus `\n`.
    pub fn database_error(&self, message: &str, highlight_errors: bool) -> String {
        if highlight_errors {
            self.error(message)
        } else {
            format!("{message}\n")
        }
    }

    /// `ShellHighlight::PrintError`: the error message highlighted with the error elements,
    /// including the trailing `\n` (empty for an empty message). The `XXX Error:` prefix goes
    /// on its own line.
    pub fn error(&self, message: &str) -> String {
        if message.is_empty() {
            return String::new();
        }
        let (error_type, message) = match message.find("Error: ") {
            Some(location) => (&message[..location + 6], &message[location + 7..]),
            None => ("", message),
        };
        let mut tokens = tokenize_error(message);
        if tokens.first().is_some_and(|t| t.start > 0) {
            tokens.insert(0, SimplifiedToken::new(SimplifiedTokenType::Identifier, 0));
        }
        if tokens.is_empty() && !message.is_empty() {
            tokens.push(SimplifiedToken::new(SimplifiedTokenType::Identifier, 0));
        }
        let mut out = Vec::new();
        if !error_type.is_empty() {
            self.paint_element(
                &mut out,
                format!("{error_type}\n").as_bytes(),
                HighlightElementType::ErrorEmphasis,
            );
        }
        let bytes = message.as_bytes();
        for (i, token) in tokens.iter().enumerate() {
            let element = match token.token_type {
                SimplifiedTokenType::Identifier | SimplifiedTokenType::Operator => HighlightElementType::None,
                SimplifiedTokenType::Error => HighlightElementType::ErrorToken,
                SimplifiedTokenType::ErrorEmphasis => HighlightElementType::ErrorEmphasis,
                SimplifiedTokenType::ErrorSuggestion => HighlightElementType::ErrorSuggestion,
                SimplifiedTokenType::NumericConstant => HighlightElementType::NumericConstant,
                SimplifiedTokenType::StringConstant => HighlightElementType::StringConstant,
                SimplifiedTokenType::Keyword => HighlightElementType::Keyword,
                SimplifiedTokenType::Comment => HighlightElementType::LineIndicator,
            };
            let start = token.start.min(bytes.len());
            let end = tokens.get(i + 1).map_or(bytes.len(), |t| t.start).min(bytes.len());
            if end > start {
                self.paint_element(&mut out, &bytes[start..end], element);
            }
        }
        out.push(b'\n');
        bytes_to_string(out)
    }

    /// `.display_colors [bold|underline]...` (`DisplayColors`); `args` are the words after the
    /// command name. Every color name painted in its own color, grouped and sorted by hue and
    /// luminosity, separated by spaces and ending in `\n`. Any other word is
    /// [`HighlightError::Usage`].
    pub fn display_colors(&self, args: &[&str]) -> Result<String, HighlightError> {
        let mut bold = false;
        let mut underline = false;
        for arg in args {
            match *arg {
                "bold" => bold = true,
                "underline" => underline = true,
                _ => return Err(HighlightError::Usage),
            }
        }
        let intensity = match (bold, underline) {
            (true, true) => PrintIntensity::BoldUnderline,
            (true, false) => PrintIntensity::Bold,
            (false, true) => PrintIntensity::Underline,
            (false, false) => PrintIntensity::Standard,
        };
        let mut out = Vec::new();
        for code in display_color_order() {
            self.paint_bytes(
                &mut out,
                COLORS[code as usize].0.as_bytes(),
                PrintColor::Code(code),
                intensity,
            );
            out.push(b' ');
        }
        out.push(b'\n');
        Ok(bytes_to_string(out))
    }

    /// `ShellState::HighlightSQL` without its `stdout_is_console` check (the caller makes it):
    /// the SQL with every token painted, or the SQL unchanged when highlighting is disabled.
    pub fn highlight_sql(&self, sql: &str) -> String {
        if !self.enabled {
            return sql.to_string();
        }
        let tokens = if sql.starts_with('.') {
            tokenize_dot_command(sql)
        } else {
            tokenize_sql(sql)
        };
        self.highlight_tokens(sql, 0, sql.len(), &tokens)
    }

    /// `Highlighting::HighlightText`: renders the bytes `start_pos..end_pos` of `text` with
    /// the token colors (`tokens` from [`tokenize_sql`] or [`tokenize_dot_command`]).
    pub fn highlight_tokens(&self, text: &str, start_pos: usize, end_pos: usize, tokens: &[HighlightToken]) -> String {
        let bytes = text.as_bytes();
        let len = bytes.len();
        let end_pos = end_pos.min(len);
        let mut out = Vec::new();
        let mut prev_pos = 0;
        for (i, token) in tokens.iter().enumerate() {
            let next = tokens.get(i + 1).map_or(len, |t| t.start);
            if next < start_pos {
                // this token is not rendered at all
                continue;
            }
            let start = token.start.max(start_pos);
            let end = next.min(end_pos);
            if end <= start {
                continue;
            }
            if prev_pos > start {
                // rendering out of order (C++ logs this)
                continue;
            }
            prev_pos = start;
            let element = match token.token_type {
                HighlightTokenType::Keyword => HighlightElementType::Keyword,
                HighlightTokenType::NumericConstant => HighlightElementType::NumericConstant,
                HighlightTokenType::StringConstant => HighlightElementType::StringConstant,
                HighlightTokenType::Continuation => HighlightElementType::Continuation,
                HighlightTokenType::ContinuationSelected => HighlightElementType::ContinuationSelected,
                HighlightTokenType::Bracket => HighlightElementType::Bracket,
                HighlightTokenType::Error => HighlightElementType::ErrorToken,
                HighlightTokenType::Comment => HighlightElementType::Comment,
                HighlightTokenType::Identifier | HighlightTokenType::Operator => HighlightElementType::None,
            };
            let (color, intensity) = self.element(element);
            // HighlightText always paints (it does not consult IsEnabled itself)
            let code = terminal_code(color, intensity);
            if code.is_empty() {
                out.extend_from_slice(&bytes[start..end]);
            } else {
                out.extend_from_slice(code.as_bytes());
                out.extend_from_slice(&bytes[start..end]);
                out.extend_from_slice(RESET_TERMINAL_CODE.as_bytes());
            }
        }
        bytes_to_string(out)
    }
}

/// Token offsets are byte offsets; a caret position computed from a multi-byte line can split a
/// character, which C++ prints as raw bytes.
fn bytes_to_string(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

// ---------------------------------------------------------------------------------------------
// Colors
// ---------------------------------------------------------------------------------------------

/// `highlight_color_info`: name and RGB of each terminal color, indexed by color code.
const COLORS: [(&str, u8, u8, u8); 256] = [
    ("black", 0x00, 0x00, 0x00),
    ("red", 0x80, 0x00, 0x00),
    ("green", 0x00, 0x80, 0x00),
    ("yellow", 0x80, 0x80, 0x00),
    ("blue", 0x00, 0x00, 0x80),
    ("magenta", 0x80, 0x00, 0x80),
    ("cyan", 0x00, 0x80, 0x80),
    ("brightgray", 0xC0, 0xC0, 0xC0),
    ("gray", 0x80, 0x80, 0x80),
    ("brightred", 0xFF, 0x00, 0x00),
    ("brightgreen", 0x00, 0xFF, 0x00),
    ("brightyellow", 0xFF, 0xFF, 0x00),
    ("brightblue", 0x00, 0x00, 0xFF),
    ("brightmagenta", 0xFF, 0x00, 0xFF),
    ("brightcyan", 0x00, 0xFF, 0xFF),
    ("white", 0xFF, 0xFF, 0xFF),
    ("grey0", 0x00, 0x00, 0x00),
    ("navyblue", 0x00, 0x00, 0x5F),
    ("darkblue", 0x00, 0x00, 0x87),
    ("blue3", 0x00, 0x00, 0xAF),
    ("blue4", 0x00, 0x00, 0xD7),
    ("blue1", 0x00, 0x00, 0xFF),
    ("darkgreen", 0x00, 0x5F, 0x00),
    ("deepskyblue4", 0x00, 0x5F, 0x5F),
    ("deepskyblue5", 0x00, 0x5F, 0x87),
    ("deepskyblue6", 0x00, 0x5F, 0xAF),
    ("dodgerblue3", 0x00, 0x5F, 0xD7),
    ("dodgerblue2", 0x00, 0x5F, 0xFF),
    ("green4", 0x00, 0x87, 0x00),
    ("springgreen4", 0x00, 0x87, 0x5F),
    ("turquoise4", 0x00, 0x87, 0x87),
    ("deepskyblue3", 0x00, 0x87, 0xAF),
    ("deepskyblue7", 0x00, 0x87, 0xD7),
    ("dodgerblue1", 0x00, 0x87, 0xFF),
    ("green3", 0x00, 0xAF, 0x00),
    ("springgreen3", 0x00, 0xAF, 0x5F),
    ("darkcyan", 0x00, 0xAF, 0x87),
    ("lightseagreen", 0x00, 0xAF, 0xAF),
    ("deepskyblue2", 0x00, 0xAF, 0xD7),
    ("deepskyblue1", 0x00, 0xAF, 0xFF),
    ("green5", 0x00, 0xD7, 0x00),
    ("springgreen5", 0x00, 0xD7, 0x5F),
    ("springgreen2", 0x00, 0xD7, 0x87),
    ("cyan3", 0x00, 0xD7, 0xAF),
    ("darkturquoise", 0x00, 0xD7, 0xD7),
    ("turquoise2", 0x00, 0xD7, 0xFF),
    ("green1", 0x00, 0xFF, 0x00),
    ("springgreen6", 0x00, 0xFF, 0x5F),
    ("springgreen1", 0x00, 0xFF, 0x87),
    ("mediumspringgreen", 0x00, 0xFF, 0xAF),
    ("cyan2", 0x00, 0xFF, 0xD7),
    ("cyan1", 0x00, 0xFF, 0xFF),
    ("darkred1", 0x5F, 0x00, 0x00),
    ("deeppink4", 0x5F, 0x00, 0x5F),
    ("purple4", 0x5F, 0x00, 0x87),
    ("purple5", 0x5F, 0x00, 0xAF),
    ("purple3", 0x5F, 0x00, 0xD7),
    ("blueviolet", 0x5F, 0x00, 0xFF),
    ("orange4", 0x5F, 0x5F, 0x00),
    ("grey37", 0x5F, 0x5F, 0x5F),
    ("mediumpurple4", 0x5F, 0x5F, 0x87),
    ("slateblue3", 0x5F, 0x5F, 0xAF),
    ("slateblue4", 0x5F, 0x5F, 0xD7),
    ("royalblue1", 0x5F, 0x5F, 0xFF),
    ("chartreuse4", 0x5F, 0x87, 0x00),
    ("darkseagreen4", 0x5F, 0x87, 0x5F),
    ("paleturquoise4", 0x5F, 0x87, 0x87),
    ("steelblue", 0x5F, 0x87, 0xAF),
    ("steelblue3", 0x5F, 0x87, 0xD7),
    ("cornflowerblue", 0x5F, 0x87, 0xFF),
    ("chartreuse3", 0x5F, 0xAF, 0x00),
    ("darkseagreen5", 0x5F, 0xAF, 0x5F),
    ("cadetblue", 0x5F, 0xAF, 0x87),
    ("cadetblue2", 0x5F, 0xAF, 0xAF),
    ("skyblue3", 0x5F, 0xAF, 0xD7),
    ("steelblue1", 0x5F, 0xAF, 0xFF),
    ("chartreuse5", 0x5F, 0xD7, 0x00),
    ("palegreen3", 0x5F, 0xD7, 0x5F),
    ("seagreen3", 0x5F, 0xD7, 0x87),
    ("aquamarine3", 0x5F, 0xD7, 0xAF),
    ("mediumturquoise", 0x5F, 0xD7, 0xD7),
    ("steelblue2", 0x5F, 0xD7, 0xFF),
    ("chartreuse2", 0x5F, 0xFF, 0x00),
    ("seagreen2", 0x5F, 0xFF, 0x5F),
    ("seagreen1", 0x5F, 0xFF, 0x87),
    ("seagreen4", 0x5F, 0xFF, 0xAF),
    ("aquamarine1", 0x5F, 0xFF, 0xD7),
    ("darkslategray2", 0x5F, 0xFF, 0xFF),
    ("darkred2", 0x87, 0x00, 0x00),
    ("deeppink5", 0x87, 0x00, 0x5F),
    ("darkmagenta", 0x87, 0x00, 0x87),
    ("darkmagenta2", 0x87, 0x00, 0xAF),
    ("darkviolet1", 0x87, 0x00, 0xD7),
    ("purple2", 0x87, 0x00, 0xFF),
    ("orange5", 0x87, 0x5F, 0x00),
    ("lightpink4", 0x87, 0x5F, 0x5F),
    ("plum4", 0x87, 0x5F, 0x87),
    ("mediumpurple3", 0x87, 0x5F, 0xAF),
    ("mediumpurple5", 0x87, 0x5F, 0xD7),
    ("slateblue1", 0x87, 0x5F, 0xFF),
    ("yellow4", 0x87, 0x87, 0x00),
    ("wheat4", 0x87, 0x87, 0x5F),
    ("grey53", 0x87, 0x87, 0x87),
    ("lightslategrey", 0x87, 0x87, 0xAF),
    ("mediumpurple", 0x87, 0x87, 0xD7),
    ("lightslateblue", 0x87, 0x87, 0xFF),
    ("yellow5", 0x87, 0xAF, 0x00),
    ("darkolivegreen3", 0x87, 0xAF, 0x5F),
    ("darkseagreen", 0x87, 0xAF, 0x87),
    ("lightskyblue3", 0x87, 0xAF, 0xAF),
    ("lightskyblue4", 0x87, 0xAF, 0xD7),
    ("skyblue2", 0x87, 0xAF, 0xFF),
    ("chartreuse6", 0x87, 0xD7, 0x00),
    ("darkolivegreen4", 0x87, 0xD7, 0x5F),
    ("palegreen4", 0x87, 0xD7, 0x87),
    ("darkseagreen3", 0x87, 0xD7, 0xAF),
    ("darkslategray3", 0x87, 0xD7, 0xD7),
    ("skyblue1", 0x87, 0xD7, 0xFF),
    ("chartreuse1", 0x87, 0xFF, 0x00),
    ("lightgreen1", 0x87, 0xFF, 0x5F),
    ("lightgreen2", 0x87, 0xFF, 0x87),
    ("palegreen1", 0x87, 0xFF, 0xAF),
    ("aquamarine2", 0x87, 0xFF, 0xD7),
    ("darkslategray1", 0x87, 0xFF, 0xFF),
    ("red3", 0xAF, 0x00, 0x00),
    ("deeppink6", 0xAF, 0x00, 0x5F),
    ("mediumvioletred", 0xAF, 0x00, 0x87),
    ("magenta3", 0xAF, 0x00, 0xAF),
    ("darkviolet2", 0xAF, 0x00, 0xD7),
    ("purple6", 0xAF, 0x00, 0xFF),
    ("darkorange3", 0xAF, 0x5F, 0x00),
    ("indianred1", 0xAF, 0x5F, 0x5F),
    ("hotpink3", 0xAF, 0x5F, 0x87),
    ("mediumorchid3", 0xAF, 0x5F, 0xAF),
    ("mediumorchid", 0xAF, 0x5F, 0xD7),
    ("mediumpurple2", 0xAF, 0x5F, 0xFF),
    ("darkgoldenrod", 0xAF, 0x87, 0x00),
    ("lightsalmon3", 0xAF, 0x87, 0x5F),
    ("rosybrown", 0xAF, 0x87, 0x87),
    ("grey63", 0xAF, 0x87, 0xAF),
    ("mediumpurple6", 0xAF, 0x87, 0xD7),
    ("mediumpurple1", 0xAF, 0x87, 0xFF),
    ("gold3", 0xAF, 0xAF, 0x00),
    ("darkkhaki", 0xAF, 0xAF, 0x5F),
    ("navajowhite3", 0xAF, 0xAF, 0x87),
    ("grey69", 0xAF, 0xAF, 0xAF),
    ("lightsteelblue3", 0xAF, 0xAF, 0xD7),
    ("lightsteelblue", 0xAF, 0xAF, 0xFF),
    ("yellow3", 0xAF, 0xD7, 0x00),
    ("darkolivegreen5", 0xAF, 0xD7, 0x5F),
    ("darkseagreen6", 0xAF, 0xD7, 0x87),
    ("darkseagreen2", 0xAF, 0xD7, 0xAF),
    ("lightcyan3", 0xAF, 0xD7, 0xD7),
    ("lightskyblue1", 0xAF, 0xD7, 0xFF),
    ("greenyellow", 0xAF, 0xFF, 0x00),
    ("darkolivegreen2", 0xAF, 0xFF, 0x5F),
    ("palegreen2", 0xAF, 0xFF, 0x87),
    ("darkseagreen7", 0xAF, 0xFF, 0xAF),
    ("darkseagreen1", 0xAF, 0xFF, 0xD7),
    ("paleturquoise1", 0xAF, 0xFF, 0xFF),
    ("red4", 0xD7, 0x00, 0x00),
    ("deeppink3", 0xD7, 0x00, 0x5F),
    ("deeppink7", 0xD7, 0x00, 0x87),
    ("magenta4", 0xD7, 0x00, 0xAF),
    ("magenta5", 0xD7, 0x00, 0xD7),
    ("magenta2", 0xD7, 0x00, 0xFF),
    ("darkorange4", 0xD7, 0x5F, 0x00),
    ("indianred2", 0xD7, 0x5F, 0x5F),
    ("hotpink4", 0xD7, 0x5F, 0x87),
    ("hotpink2", 0xD7, 0x5F, 0xAF),
    ("orchid", 0xD7, 0x5F, 0xD7),
    ("mediumorchid1", 0xD7, 0x5F, 0xFF),
    ("orange3", 0xD7, 0x87, 0x00),
    ("lightsalmon4", 0xD7, 0x87, 0x5F),
    ("lightpink3", 0xD7, 0x87, 0x87),
    ("pink3", 0xD7, 0x87, 0xAF),
    ("plum3", 0xD7, 0x87, 0xD7),
    ("violet", 0xD7, 0x87, 0xFF),
    ("gold4", 0xD7, 0xAF, 0x00),
    ("lightgoldenrod3", 0xD7, 0xAF, 0x5F),
    ("tan", 0xD7, 0xAF, 0x87),
    ("mistyrose3", 0xD7, 0xAF, 0xAF),
    ("thistle3", 0xD7, 0xAF, 0xD7),
    ("plum2", 0xD7, 0xAF, 0xFF),
    ("yellow6", 0xD7, 0xD7, 0x00),
    ("khaki3", 0xD7, 0xD7, 0x5F),
    ("lightgoldenrod2", 0xD7, 0xD7, 0x87),
    ("lightyellow3", 0xD7, 0xD7, 0xAF),
    ("grey84", 0xD7, 0xD7, 0xD7),
    ("lightsteelblue1", 0xD7, 0xD7, 0xFF),
    ("yellow2", 0xD7, 0xFF, 0x00),
    ("darkolivegreen1", 0xD7, 0xFF, 0x5F),
    ("darkolivegreen6", 0xD7, 0xFF, 0x87),
    ("darkseagreen8", 0xD7, 0xFF, 0xAF),
    ("honeydew2", 0xD7, 0xFF, 0xD7),
    ("lightcyan1", 0xD7, 0xFF, 0xFF),
    ("red1", 0xFF, 0x00, 0x00),
    ("deeppink2", 0xFF, 0x00, 0x5F),
    ("deeppink1", 0xFF, 0x00, 0x87),
    ("deeppink8", 0xFF, 0x00, 0xAF),
    ("magenta6", 0xFF, 0x00, 0xD7),
    ("magenta1", 0xFF, 0x00, 0xFF),
    ("orangered1", 0xFF, 0x5F, 0x00),
    ("indianred3", 0xFF, 0x5F, 0x5F),
    ("indianred4", 0xFF, 0x5F, 0x87),
    ("hotpink", 0xFF, 0x5F, 0xAF),
    ("hotpink5", 0xFF, 0x5F, 0xD7),
    ("mediumorchid2", 0xFF, 0x5F, 0xFF),
    ("darkorange", 0xFF, 0x87, 0x00),
    ("salmon1", 0xFF, 0x87, 0x5F),
    ("lightcoral", 0xFF, 0x87, 0x87),
    ("palevioletred1", 0xFF, 0x87, 0xAF),
    ("orchid2", 0xFF, 0x87, 0xD7),
    ("orchid1", 0xFF, 0x87, 0xFF),
    ("orange1", 0xFF, 0xAF, 0x00),
    ("sandybrown", 0xFF, 0xAF, 0x5F),
    ("lightsalmon1", 0xFF, 0xAF, 0x87),
    ("lightpink1", 0xFF, 0xAF, 0xAF),
    ("pink1", 0xFF, 0xAF, 0xD7),
    ("plum1", 0xFF, 0xAF, 0xFF),
    ("gold1", 0xFF, 0xD7, 0x00),
    ("lightgoldenrod4", 0xFF, 0xD7, 0x5F),
    ("lightgoldenrod5", 0xFF, 0xD7, 0x87),
    ("navajowhite1", 0xFF, 0xD7, 0xAF),
    ("mistyrose1", 0xFF, 0xD7, 0xD7),
    ("thistle1", 0xFF, 0xD7, 0xFF),
    ("yellow1", 0xFF, 0xFF, 0x00),
    ("lightgoldenrod1", 0xFF, 0xFF, 0x5F),
    ("khaki1", 0xFF, 0xFF, 0x87),
    ("wheat1", 0xFF, 0xFF, 0xAF),
    ("cornsilk1", 0xFF, 0xFF, 0xD7),
    ("grey100", 0xFF, 0xFF, 0xFF),
    ("grey3", 0x08, 0x08, 0x08),
    ("grey7", 0x12, 0x12, 0x12),
    ("grey11", 0x1C, 0x1C, 0x1C),
    ("grey15", 0x26, 0x26, 0x26),
    ("grey19", 0x30, 0x30, 0x30),
    ("grey23", 0x3A, 0x3A, 0x3A),
    ("grey27", 0x44, 0x44, 0x44),
    ("grey30", 0x4E, 0x4E, 0x4E),
    ("grey35", 0x58, 0x58, 0x58),
    ("grey39", 0x62, 0x62, 0x62),
    ("grey42", 0x6C, 0x6C, 0x6C),
    ("grey46", 0x76, 0x76, 0x76),
    ("grey50", 0x80, 0x80, 0x80),
    ("grey54", 0x8A, 0x8A, 0x8A),
    ("grey58", 0x94, 0x94, 0x94),
    ("grey62", 0x9E, 0x9E, 0x9E),
    ("grey66", 0xA8, 0xA8, 0xA8),
    ("grey70", 0xB2, 0xB2, 0xB2),
    ("grey74", 0xBC, 0xBC, 0xBC),
    ("grey78", 0xC6, 0xC6, 0xC6),
    ("grey82", 0xD0, 0xD0, 0xD0),
    ("grey85", 0xDA, 0xDA, 0xDA),
    ("grey89", 0xE4, 0xE4, 0xE4),
    ("grey93", 0xEE, 0xEE, 0xEE),
];

/// `.display_colors` groups colors by the words their names contain, in this order; a name
/// belongs to the last group with a matching word (`FindColorGroup`).
const COLOR_GROUPS: &[&[&str]] = &[
    &["red", "maroon", "coral"],
    &["orange"],
    &["yellow", "gold", "khaki", "wheat"],
    &["green", "chartreuse", "lime", "honeydew", "olive"],
    &["cyan", "aqua", "turquoise"],
    &["blue", "navy"],
    &["pink", "orchid", "rose", "thistle", "salmon", "tan"],
    &["purple", "magenta", "plum", "fuchsia", "violet"],
    &["brown"],
    &["grey", "gray", "black"],
    &["white", "silver", "cornsilk"],
];

fn color_group(name: &str) -> usize {
    let mut group = None;
    for (index, words) in COLOR_GROUPS.iter().enumerate() {
        if words.iter().any(|word| name.contains(word)) {
            group = Some(index);
        }
    }
    group.unwrap_or(COLOR_GROUPS.len())
}

/// `GetHue`.
fn color_hue(code: u8) -> f64 {
    let (_, r, g, b) = COLORS[code as usize];
    let r = f64::from(r) / 255.0;
    let g = f64::from(g) / 255.0;
    let b = f64::from(b) / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    if delta == 0.0 {
        return 0.0;
    }
    let mut hue = if max == r {
        (g - b) / delta
    } else if max == g {
        2.0 + (b - r) / delta
    } else {
        4.0 + (r - g) / delta
    };
    hue *= 60.0;
    if hue < 0.0 {
        hue += 360.0;
    }
    hue
}

/// `GetLum`.
fn color_luminosity(code: u8) -> f64 {
    let (_, r, g, b) = COLORS[code as usize];
    (0.241 * f64::from(r) + 0.691 * f64::from(g) + 0.068 * f64::from(b)).sqrt()
}

/// Colors with identical RGB values tie in the C++ `std::sort`; these codes come first among
/// their twins in the reference output (the order of ties is up to the C++ sort).
const TIE_FIRST: &[u8] = &[16, 196, 46, 226, 51, 12, 201, 244];

/// The order `.display_colors` lists the color codes in.
fn display_color_order() -> Vec<u8> {
    let mut codes: Vec<u8> = (0..=255).collect();
    codes.sort_by(|&a, &b| {
        let (a_group, b_group) = (color_group(COLORS[a as usize].0), color_group(COLORS[b as usize].0));
        a_group
            .cmp(&b_group)
            .then_with(|| color_hue(a).total_cmp(&color_hue(b)))
            .then_with(|| color_luminosity(a).total_cmp(&color_luminosity(b)))
            .then_with(|| TIE_FIRST.contains(&b).cmp(&TIE_FIRST.contains(&a)))
    });
    codes
}

// ---------------------------------------------------------------------------------------------
// "Did you mean" candidates (StringUtil::CandidatesErrorMessage)
// ---------------------------------------------------------------------------------------------

/// `StringUtil::LevenshteinDistance` (case-insensitive) with a substitution penalty.
fn levenshtein(s1: &str, s2: &str, not_equal_penalty: usize) -> usize {
    let s1 = s1.to_ascii_lowercase().into_bytes();
    let s2 = s2.to_ascii_lowercase().into_bytes();
    if s1.is_empty() {
        return s2.len();
    }
    if s2.is_empty() {
        return s1.len();
    }
    let mut previous: Vec<usize> = (0..=s2.len()).collect();
    let mut current = vec![0; s2.len() + 1];
    for i in 1..=s1.len() {
        current[0] = i;
        for j in 1..=s2.len() {
            let equal = if s1[i - 1] == s2[j - 1] { 0 } else { not_equal_penalty };
            current[j] = (previous[j] + 1).min(current[j - 1] + 1).min(previous[j - 1] + equal);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[s2.len()]
}

/// `StringUtil::TopNLevenshtein` (n = 5, threshold = 5) followed by `CandidatesMessage`.
fn candidates_error_message(strings: &[&str], target: &str, prefix: &str) -> String {
    const N: usize = 5;
    const THRESHOLD: usize = 5;
    let scores: Vec<(&str, usize)> = strings
        .iter()
        .map(|s| {
            // compare only the prefix of a longer candidate (byte-wise, as in C++)
            let candidate = if target.len() < s.len() {
                &s.as_bytes()[..target.len()]
            } else {
                s.as_bytes()
            };
            (*s, levenshtein(&String::from_utf8_lossy(candidate), target, 3))
        })
        .collect();
    if scores.is_empty() {
        return String::new();
    }
    let max_score = scores.iter().map(|s| s.1).fold(THRESHOLD, usize::max);
    let normalize = |score: usize| 1.0 - score as f64 / max_score as f64;
    let mut normalized: Vec<(&str, f64)> = scores.iter().map(|&(s, score)| (s, normalize(score))).collect();
    normalized.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then_with(|| a.0.to_ascii_uppercase().cmp(&b.0.to_ascii_uppercase()))
    });
    let threshold = normalize(THRESHOLD);
    let mut result = vec![normalized[0].0];
    for &(s, score) in normalized.iter().take(N).skip(1) {
        if score < threshold {
            break;
        }
        result.push(s);
    }
    let quoted: Vec<String> = result.iter().map(|s| format!("\"{s}\"")).collect();
    format!("\n{prefix}: {}", quoted.join(", "))
}

// ---------------------------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------------------------

/// `SimplifiedTokenType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SimplifiedTokenType {
    Identifier,
    NumericConstant,
    StringConstant,
    Operator,
    Keyword,
    Comment,
    Error,
    ErrorEmphasis,
    ErrorSuggestion,
}

/// `SimplifiedToken`: a token type and the byte offset it starts at; it runs until the next
/// token starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SimplifiedToken {
    pub token_type: SimplifiedTokenType,
    pub start: usize,
}

impl SimplifiedToken {
    fn new(token_type: SimplifiedTokenType, start: usize) -> Self {
        Self { token_type, start }
    }
}

/// linenoise's `tokenType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HighlightTokenType {
    Identifier,
    NumericConstant,
    StringConstant,
    Operator,
    Keyword,
    Comment,
    Continuation,
    ContinuationSelected,
    Bracket,
    Error,
}

/// linenoise's `highlightToken` (without the search-match `extra_highlighting`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HighlightToken {
    pub token_type: HighlightTokenType,
    pub start: usize,
}

/// `Parser::TokenizeError`: splits an error message (after its `XXX Error: ` prefix) into
/// spans. Quoted names become suggestions, and a `LINE n:` excerpt is tokenized as SQL with
/// keywords and the token under the caret emphasized.
pub fn tokenize_error(error_msg: &str) -> Vec<SimplifiedToken> {
    use SimplifiedTokenType as T;
    let bytes = error_msg.as_bytes();
    let size = bytes.len();
    let mut error_start = 0;
    let mut error_end = size;
    let mut tokens = Vec::new();
    // find "XXX Error:" - this marks the start of the error message
    if let Some(error) = error_msg.find("Error:") {
        tokens.push(SimplifiedToken::new(T::ErrorEmphasis, 0));
        tokens.push(SimplifiedToken::new(T::Error, error + 6));
        error_start = error + 6;
    } else {
        tokens.push(SimplifiedToken::new(T::Error, 0));
    }
    // find "LINE (number)" - this marks the end of the message
    let line_pos = error_msg.find("\nLINE ");
    if let Some(line_pos) = line_pos {
        error_end = line_pos;
    }
    let mut in_quotes = false;
    let mut quote_char = 0u8;
    for (i, &c) in bytes.iter().enumerate().take(error_end).skip(error_start) {
        if in_quotes {
            if c == quote_char {
                tokens.push(SimplifiedToken::new(T::Error, i));
                in_quotes = false;
            }
            if c == b'\n' || c == b'\r' {
                // found a newline in a quote, abort the quoted state entirely
                tokens.pop();
                in_quotes = false;
            }
        } else if c == b'"' || c == b'\'' {
            tokens.push(SimplifiedToken::new(T::ErrorSuggestion, i + 1));
            quote_char = c;
            in_quotes = true;
        }
    }
    if in_quotes {
        // unterminated quotes at the end of the error - pop back the quoted state
        tokens.pop();
    }
    let Some(line_pos) = line_pos else {
        return tokens;
    };
    tokens.push(SimplifiedToken::new(T::ErrorEmphasis, line_pos + 1));
    // tokenize the LINE part
    let mut query_start = line_pos + 6;
    while query_start < size && (bytes[query_start] == b':' || bytes[query_start].is_ascii_digit()) {
        query_start += 1;
    }
    if query_start >= size {
        return tokens;
    }
    tokens.push(SimplifiedToken::new(T::Error, query_start));
    let query_end = bytes[query_start..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(size, |p| query_start + p);
    // after LINE XXX: comes a caret - look for it
    let caret_start = query_end + 1;
    let caret_position = if caret_start < size {
        bytes[caret_start..].iter().position(|&c| c == b'^').and_then(|offset| {
            // the caret is relative to the start of the line ("LINE n: " included)
            offset.checked_sub(query_start - line_pos - 1)
        })
    } else {
        None
    };
    let mut place_caret = caret_position.is_some();
    let caret_position = caret_position.unwrap_or(0);
    let query = String::from_utf8_lossy(&bytes[query_start..query_end]);
    for mut token in tokenize_simplified(&query) {
        if place_caret && token.start >= caret_position {
            // highlight the token the caret points to
            tokens.push(SimplifiedToken::new(T::ErrorEmphasis, query_start + caret_position));
            place_caret = false;
            continue;
        }
        token.token_type = if token.token_type == T::Keyword {
            T::ErrorEmphasis
        } else {
            T::Error
        };
        token.start += query_start;
        tokens.push(token);
    }
    if query_end < size {
        tokens.push(SimplifiedToken::new(T::Error, query_end));
    }
    tokens
}

/// `Parser::Tokenize`: the SQL tokens for highlighting (comments included, the end of input
/// excluded). Identifiers the grammar matches literally count as keywords.
pub fn tokenize_simplified(query: &str) -> Vec<SimplifiedToken> {
    use SimplifiedTokenType as T;
    tokenize_raw(query)
        .into_iter()
        .filter(|t| t.kind != RawKind::EndOfInput)
        .map(|t| {
            let token_type = match t.kind {
                RawKind::Keyword => T::Keyword,
                RawKind::Identifier if is_grammar_word(t.text(query)) => T::Keyword,
                RawKind::Identifier | RawKind::EndOfInput => T::Identifier,
                RawKind::StringLiteral => T::StringConstant,
                RawKind::NumberLiteral => T::NumericConstant,
                RawKind::Operator | RawKind::Terminator => T::Operator,
                RawKind::Comment => T::Comment,
            };
            SimplifiedToken::new(token_type, t.start)
        })
        .collect()
}

/// `Highlighting::Tokenize` for SQL (`GetParseTokens`): unterminated strings, quoted
/// identifiers and comments are errors; the first token always starts at 0.
pub fn tokenize_sql(sql: &str) -> Vec<HighlightToken> {
    use HighlightTokenType as T;
    let mut tokens: Vec<HighlightToken> = tokenize_raw(sql)
        .into_iter()
        .map(|t| {
            let token_type = if t.unterminated {
                T::Error
            } else {
                match t.kind {
                    RawKind::Identifier if is_grammar_word(t.text(sql)) => T::Keyword,
                    RawKind::Identifier => T::Identifier,
                    RawKind::NumberLiteral => T::NumericConstant,
                    RawKind::StringLiteral => T::StringConstant,
                    RawKind::Operator | RawKind::Terminator | RawKind::EndOfInput => T::Operator,
                    RawKind::Keyword => T::Keyword,
                    RawKind::Comment => T::Comment,
                }
            };
            HighlightToken {
                token_type,
                start: t.start,
            }
        })
        .collect();
    if tokens.first().is_some_and(|t| t.start > 0) {
        tokens.insert(
            0,
            HighlightToken {
                token_type: T::Identifier,
                start: 0,
            },
        );
    }
    if tokens.is_empty() && !sql.is_empty() {
        tokens.push(HighlightToken {
            token_type: T::Identifier,
            start: 0,
        });
    }
    tokens
}

/// `Highlighting::Tokenize` for a dot command (`GetDotCommandTokens`): the command is a keyword
/// and every argument after a space a string constant.
pub fn tokenize_dot_command(line: &str) -> Vec<HighlightToken> {
    let bytes = line.as_bytes();
    let mut tokens = vec![HighlightToken {
        token_type: HighlightTokenType::Keyword,
        start: 0,
    }];
    for (i, &c) in bytes.iter().enumerate().take(bytes.len().saturating_sub(1)) {
        if matches!(c, b' ' | b'\r' | b'\n' | b'\t') {
            tokens.push(HighlightToken {
                token_type: HighlightTokenType::StringConstant,
                start: i + 1,
            });
        }
    }
    tokens
}

/// `PEGKeywordHelper::IsGrammarWord`: a word (starting with a letter or `_`) that the grammar
/// matches literally, keyword or not (e.g. `REPOSITORY`).
fn is_grammar_word(text: &str) -> bool {
    let Some(&first) = text.as_bytes().first() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    grammar_words().contains(&text.to_ascii_lowercase())
}

fn grammar_words() -> &'static HashSet<String> {
    use pintail_parser::grammar::{Grammar, Node};
    static WORDS: OnceLock<HashSet<String>> = OnceLock::new();
    WORDS.get_or_init(|| {
        fn collect(node: &Node, words: &mut HashSet<String>) {
            match node {
                Node::Literal(word) => {
                    words.insert(word.to_ascii_lowercase());
                }
                Node::LiteralSet(list, _) => words.extend(list.iter().map(|w| w.to_ascii_lowercase())),
                Node::Seq(items) | Node::Choice(items) => items.iter().for_each(|n| collect(n, words)),
                Node::Optional(n) | Node::Star(n) | Node::Plus(n) => collect(n, words),
                Node::Rule(_) | Node::Terminal(_) | Node::Unsupported => {}
            }
        }
        let mut words = HashSet::new();
        for rule in &Grammar::duckdb().rules {
            collect(&rule.node, &mut words);
        }
        words
    })
}

// ---------------------------------------------------------------------------------------------
// The highlight tokenizer (Tokenizer::TokenizeInput with HighlightTokenizerBehavior)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawKind {
    Keyword,
    Identifier,
    StringLiteral,
    NumberLiteral,
    Operator,
    Terminator,
    Comment,
    EndOfInput,
}

#[derive(Debug, Clone, Copy)]
struct RawToken {
    kind: RawKind,
    start: usize,
    end: usize,
    unterminated: bool,
}

impl RawToken {
    fn text(self, sql: &str) -> &str {
        sql.get(self.start..self.end).unwrap_or("")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Standard,
    SingleLineComment,
    MultiLineComment,
    QuotedIdentifier,
    StringLiteral,
    Keyword,
    Numeric,
    Operator,
    DollarQuotedString,
}

/// `Tokenizer::IsSingleByteOperator`.
fn is_single_byte_operator(c: u8) -> bool {
    matches!(
        c,
        b'(' | b')' | b'{' | b'}' | b'[' | b']' | b',' | b':' | b'?' | b'$' | b'#'
    )
}

/// `Tokenizer::CharacterIsInitialNumber`.
fn is_initial_number(c: u8) -> bool {
    c.is_ascii_digit() || c == b'.'
}

fn is_scientific(c: u8) -> bool {
    c == b'e' || c == b'E'
}

/// `StringUtil::CharacterIsSpace`.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `StringUtil::CharacterIsOperator`: ASCII punctuation except `_`.
fn is_punctuation(c: u8) -> bool {
    c != b'_' && matches!(c, b'!'..=b'/' | b':'..=b'@' | b'['..=b'`' | b'{'..=b'~')
}

/// `Tokenizer::CharacterIsKeyword`.
fn is_keyword_char(c: u8) -> bool {
    !is_single_byte_operator(c) && !is_punctuation(c) && !is_space(c) && !matches!(c, b'\'' | b';' | b'"' | b'.')
}

/// `Tokenizer::CharacterIsOperator`.
fn is_operator_char(c: u8) -> bool {
    matches!(
        c,
        b'+' | b'-' | b'*' | b'/' | b'%' | b'^' | b'<' | b'>' | b'=' | b'~' | b'!' | b'@' | b'&' | b'|'
    )
}

/// `Tokenizer::IsValidDollarTagCharacter`.
fn is_dollar_tag_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

/// `SkipTokenizerBytes`: the position of the next `first`/`second` byte, or of the last byte.
fn skip_bytes(bytes: &[u8], mut pos: usize, first: u8, second: u8) -> usize {
    let end = bytes.len() - 1;
    while pos < end && bytes[pos] != first && bytes[pos] != second {
        pos += 1;
    }
    pos
}

struct RawTokens {
    tokens: Vec<RawToken>,
}

impl RawTokens {
    /// `HighlightTokenizerBehavior::PushToken`: comments are kept, empty spans dropped.
    fn push(&mut self, start: usize, end: usize, kind: RawKind, unterminated: bool) {
        if start < end {
            self.tokens.push(RawToken {
                kind,
                start,
                end,
                unterminated,
            });
        }
    }

    /// A token added directly to the token list (not through `PushToken`).
    fn emplace(&mut self, start: usize, len: usize, kind: RawKind) {
        self.tokens.push(RawToken {
            kind,
            start,
            end: start + len,
            unterminated: false,
        });
    }

    /// `Tokenizer::PushOperatorToken`: an operator cannot end in `+`/`-` unless it contains one
    /// of `~ ! @ # % ^ & | \` ?`; the trimmed signs become their own tokens.
    fn push_operator(&mut self, bytes: &[u8], start: usize, end: usize) {
        let special = bytes[start..end]
            .iter()
            .any(|c| matches!(c, b'~' | b'!' | b'@' | b'#' | b'%' | b'^' | b'&' | b'|' | b'`' | b'?'));
        let mut end_pos = end;
        if !special {
            while end_pos > start + 1 && matches!(bytes[end_pos - 1], b'+' | b'-') {
                end_pos -= 1;
            }
        }
        self.push(start, end_pos, RawKind::Operator, false);
        for pos in end_pos..end {
            self.emplace(pos, 1, RawKind::Operator);
        }
    }
}

/// `Tokenizer::TokenizeInput` with `HighlightTokenizerBehavior`: every token with its byte
/// span, comments and `;` terminators included, ending with an end-of-input token.
fn tokenize_raw(sql: &str) -> Vec<RawToken> {
    let bytes = sql.as_bytes();
    let n = bytes.len();
    let mut t = RawTokens { tokens: Vec::new() };
    let mut state = State::Standard;
    let mut last_pos = 0;
    let mut escape_string = false;
    let mut quote = b'"';
    let mut dollar_marker: &[u8] = &[];
    let mut comment_depth = 0usize;
    let mut i = 0;
    while i < n {
        let c = bytes[i];
        match state {
            State::Standard => {
                if c == b'\'' {
                    state = State::StringLiteral;
                    last_pos = i;
                    escape_string = false;
                } else if c == b'"' || c == b'`' {
                    state = State::QuotedIdentifier;
                    quote = c;
                    last_pos = i;
                } else if c == b';' {
                    // end of statement
                    t.emplace(i, 1, RawKind::Terminator);
                    last_pos = i + 1;
                } else if c == b'$' {
                    if i + 1 >= n {
                        // a single dollar at the end
                    } else if bytes[i + 1].is_ascii_digit() {
                        // $[numeric] is a parameter
                        t.emplace(i, 1, RawKind::Operator);
                    } else {
                        last_pos = i;
                        let next_dollar = bytes[i + 1..]
                            .iter()
                            .position(|&b| b == b'$' || !is_dollar_tag_char(b))
                            .map(|p| i + 1 + p)
                            .filter(|&p| bytes[p] == b'$');
                        match next_dollar {
                            // a collabel parameter ($collabel)
                            None => t.emplace(i, 1, RawKind::Operator),
                            Some(next_dollar) => {
                                state = State::DollarQuotedString;
                                dollar_marker = &bytes[i + 1..next_dollar];
                                i = next_dollar;
                            }
                        }
                    }
                } else if c == b'-' && i + 1 < n && bytes[i + 1] == b'-' {
                    i += 1;
                    state = State::SingleLineComment;
                } else if c == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
                    i += 1;
                    state = State::MultiLineComment;
                    comment_depth = 1;
                } else if is_space(c) {
                    last_pos = i + 1;
                } else if c == b':' && i + 1 < n && matches!(bytes[i + 1], b':' | b'=') {
                    if i + 2 < n && is_operator_char(bytes[i + 2]) {
                        state = State::Operator;
                        last_pos = i;
                    } else {
                        // the compound colon token (offset as in C++: last_pos)
                        t.emplace(last_pos, 2, RawKind::Operator);
                        i += 1;
                        last_pos = i + 1;
                    }
                } else if is_single_byte_operator(c) {
                    t.emplace(last_pos, 1, RawKind::Operator);
                    last_pos = i + 1;
                } else if is_initial_number(c) {
                    state = State::Numeric;
                    last_pos = i;
                } else if matches!(c, b'N' | b'n' | b'X' | b'x' | b'E' | b'e' | b'B' | b'b')
                    && i + 1 < n
                    && bytes[i + 1] == b'\''
                {
                    state = State::StringLiteral;
                    last_pos = i;
                    escape_string = c == b'E' || c == b'e';
                    i += 1;
                } else if is_punctuation(c) {
                    state = State::Operator;
                    last_pos = i;
                } else {
                    state = State::Keyword;
                    last_pos = i;
                }
            }
            State::Numeric => {
                let continues = is_initial_number(c)
                    || (c == b'_' && bytes[i - 1].is_ascii_digit() && i + 1 < n && bytes[i + 1].is_ascii_digit())
                    || (is_scientific(c)
                        && !is_scientific(bytes[i - 1])
                        && (bytes[last_pos].is_ascii_digit() || bytes[i - 1].is_ascii_digit()))
                    || ((c == b'+' || c == b'-') && is_scientific(bytes[i - 1]));
                if !continues {
                    while !is_initial_number(bytes[i - 1]) {
                        i -= 1;
                    }
                    t.push(last_pos, i, RawKind::NumberLiteral, false);
                    state = State::Standard;
                    last_pos = i;
                    continue;
                }
            }
            State::Operator => {
                let comment_start = (c == b'-' && i + 1 < n && bytes[i + 1] == b'-')
                    || (c == b'/' && i + 1 < n && bytes[i + 1] == b'*');
                if comment_start || !is_operator_char(c) {
                    t.push_operator(bytes, last_pos, i);
                    state = State::Standard;
                    last_pos = i;
                    continue;
                }
            }
            State::Keyword => {
                // '$' is valid as a non-initial identifier character in PostgreSQL
                if c != b'$' && !is_keyword_char(c) {
                    let word = &sql[last_pos..i];
                    let kind = if pintail_parser::keywords::is_keyword(word) {
                        RawKind::Keyword
                    } else {
                        RawKind::Identifier
                    };
                    t.push(last_pos, i, kind, false);
                    state = State::Standard;
                    last_pos = i;
                    continue;
                }
            }
            State::StringLiteral => {
                i = skip_bytes(bytes, i, b'\'', b'\\');
                let c = bytes[i];
                if c == b'\\' && i + 1 < n && escape_string {
                    i += 1;
                } else if c == b'\'' {
                    if i + 1 < n && bytes[i + 1] == b'\'' {
                        // escaped quote
                        i += 1;
                    } else {
                        t.push(last_pos, i + 1, RawKind::StringLiteral, false);
                        last_pos = i + 1;
                        escape_string = false;
                        state = State::Standard;
                    }
                }
            }
            State::QuotedIdentifier => {
                i = skip_bytes(bytes, i, quote, quote);
                if bytes[i] == quote {
                    if i + 1 < n && bytes[i + 1] == quote {
                        // escaped delimiter
                        i += 1;
                    } else {
                        t.push(last_pos, i + 1, RawKind::Identifier, false);
                        last_pos = i + 1;
                        state = State::Standard;
                    }
                }
            }
            State::SingleLineComment => {
                i = skip_bytes(bytes, i, b'\n', b'\r');
                if matches!(bytes[i], b'\n' | b'\r') {
                    t.push(last_pos, i + 1, RawKind::Comment, false);
                    last_pos = i + 1;
                    state = State::Standard;
                }
            }
            State::MultiLineComment => {
                i = skip_bytes(bytes, i, b'/', b'*');
                let c = bytes[i];
                if c == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
                    i += 1;
                    comment_depth += 1;
                } else if c == b'*' && i + 1 < n && bytes[i + 1] == b'/' {
                    i += 1;
                    comment_depth -= 1;
                    if comment_depth == 0 {
                        t.push(last_pos, i + 1, RawKind::Comment, false);
                        last_pos = i + 1;
                        state = State::Standard;
                    }
                }
            }
            State::DollarQuotedString => {
                i = skip_bytes(bytes, i, b'$', b'$');
                // only $[marker]$ ends a dollar-quoted string
                if bytes[i] == b'$' && i + 1 < n {
                    let start = i + 1;
                    if let Some(end) = bytes[start..].iter().position(|&b| b == b'$').map(|p| start + p)
                        && &bytes[start..end] == dollar_marker
                    {
                        t.push(last_pos, end + 1, RawKind::StringLiteral, false);
                        dollar_marker = &[];
                        state = State::Standard;
                        i = end;
                        last_pos = i + 1;
                    }
                }
            }
        }
        i += 1;
    }
    match state {
        State::SingleLineComment => t.push(last_pos, n, RawKind::Comment, false),
        State::MultiLineComment => t.push(last_pos, n, RawKind::Comment, true),
        State::Operator => t.push_operator(bytes, last_pos, n),
        State::DollarQuotedString => t.push(last_pos, n, RawKind::StringLiteral, true),
        _ if last_pos < n => {
            // TokenizerBehavior::OnLastToken
            let word = &sql[last_pos..];
            let (kind, unterminated) = match state {
                State::Keyword if pintail_parser::keywords::is_keyword(word) => (RawKind::Keyword, false),
                State::Numeric => (RawKind::NumberLiteral, false),
                State::StringLiteral => (RawKind::StringLiteral, true),
                State::QuotedIdentifier => (RawKind::Identifier, true),
                _ => (RawKind::Identifier, false),
            };
            t.tokens.push(RawToken {
                kind,
                start: last_pos,
                end: n,
                unterminated,
            });
        }
        _ => {}
    }
    t.emplace(n, 0, RawKind::EndOfInput);
    t.tokens
}

#[cfg(test)]
#[path = "highlight/tests.rs"]
mod tests;
