//! Expected strings are byte-exact output of the reference DuckDB CLI (1.5.6) run with
//! `--batch --no-init` and `.highlight_errors on`, except where the baseline source differs
//! (noted at the case).

use super::*;

/// (setup, plain message as passed to `PrintDatabaseError`, highlighted `PrintError` output).
fn error_cases() -> Vec<(&'static str, &'static str, String)> {
    vec![
        (
            "",
            "Binder Error: Referenced column \"l_extendedpric\" not found in FROM clause!\nCandidate bindings: \"l_extendedprice\"\n\nLINE 1: select * from lineitem where l_extendedpric=5;\n                                     ^",
            "\x1b[1m\x1b[31mBinder Error:\n\x1b[00m\x1b[31mReferenced column \"\x1b[00m\x1b[1m\x1b[31ml_extendedpric\x1b[00m\x1b[31m\" not found in FROM clause!\nCandidate bindings: \"\x1b[00m\x1b[1m\x1b[31ml_extendedprice\x1b[00m\x1b[31m\"\n\n\x1b[00m\x1b[1m\x1b[31mLINE 1:\x1b[00m\x1b[31m \x1b[00m\x1b[1m\x1b[31mselect \x1b[00m\x1b[31m* \x1b[00m\x1b[1m\x1b[31mfrom \x1b[00m\x1b[31mlineitem \x1b[00m\x1b[1m\x1b[31mwhere \x1b[00m\x1b[1m\x1b[31ml_extendedpric\x1b[00m\x1b[31m=\x1b[00m\x1b[31m5\x1b[00m\x1b[31m;\x1b[00m\x1b[31m\n                                     ^\x1b[00m\n".to_string(),
        ),
        (
            "",
            "Binder Error: Referenced column \"I'm an error\" was not found because the FROM clause is missing\n\nLINE 1: select \"I'm an error\";\n               ^",
            "\x1b[1m\x1b[31mBinder Error:\n\x1b[00m\x1b[31mReferenced column \"\x1b[00m\x1b[1m\x1b[31mI'm an error\x1b[00m\x1b[31m\" was not found because the FROM clause is missing\n\n\x1b[00m\x1b[1m\x1b[31mLINE 1:\x1b[00m\x1b[31m \x1b[00m\x1b[1m\x1b[31mselect \x1b[00m\x1b[1m\x1b[31m\"I'm an error\"\x1b[00m\x1b[31m;\x1b[00m\x1b[31m\n               ^\x1b[00m\n".to_string(),
        ),
        (
            "",
            "Invalid Input Error: 'I\"m an error'",
            "\x1b[1m\x1b[31mInvalid Input Error:\n\x1b[00m\x1b[31m'\x1b[00m\x1b[1m\x1b[31mI\"m an error\x1b[00m\x1b[31m'\x1b[00m\n".to_string(),
        ),
        (
            "",
            "Invalid Input Error: I'm an error",
            "\x1b[1m\x1b[31mInvalid Input Error:\n\x1b[00m\x1b[31mI'm an error\x1b[00m\n".to_string(),
        ),
        (
            "",
            "Catalog Error: Table with name nope does not exist!\nDid you mean \"pg_enum\"?\n\nLINE 2:  , 42 from nope;\n                   ^",
            "\x1b[1m\x1b[31mCatalog Error:\n\x1b[00m\x1b[31mTable with name nope does not exist!\nDid you mean \"\x1b[00m\x1b[1m\x1b[31mpg_enum\x1b[00m\x1b[31m\"?\n\n\x1b[00m\x1b[1m\x1b[31mLINE 2:\x1b[00m\x1b[31m  \x1b[00m\x1b[31m, \x1b[00m\x1b[31m42 \x1b[00m\x1b[1m\x1b[31mfrom \x1b[00m\x1b[1m\x1b[31mnope\x1b[00m\x1b[31m;\x1b[00m\x1b[31m\n                   ^\x1b[00m\n".to_string(),
        ),
        (
            "error red bold",
            "Catalog Error: Table with name lineite does not exist!\nDid you mean \"pg_indexes\"?\n\nLINE 1: select * from lineite;\n                      ^",
            "\x1b[1m\x1b[31mCatalog Error:\n\x1b[00m\x1b[1m\x1b[31mTable with name lineite does not exist!\nDid you mean \"\x1b[00m\x1b[1m\x1b[31mpg_indexes\x1b[00m\x1b[1m\x1b[31m\"?\n\n\x1b[00m\x1b[1m\x1b[31mLINE 1:\x1b[00m\x1b[1m\x1b[31m \x1b[00m\x1b[1m\x1b[31mselect \x1b[00m\x1b[1m\x1b[31m* \x1b[00m\x1b[1m\x1b[31mfrom \x1b[00m\x1b[1m\x1b[31mlineite\x1b[00m\x1b[1m\x1b[31m;\x1b[00m\x1b[1m\x1b[31m\n                      ^\x1b[00m\n".to_string(),
        ),
        (
            "dark",
            "Catalog Error: Table with name lineite does not exist!\nDid you mean \"pg_indexes\"?\n\nLINE 1: select * from lineite;\n                      ^",
            "\x1b[1m\x1b[38;5;203mCatalog Error:\n\x1b[00m\x1b[38;5;203mTable with name lineite does not exist!\nDid you mean \"\x1b[00m\x1b[1m\x1b[38;5;203mpg_indexes\x1b[00m\x1b[38;5;203m\"?\n\n\x1b[00m\x1b[1m\x1b[38;5;203mLINE 1:\x1b[00m\x1b[38;5;203m \x1b[00m\x1b[1m\x1b[38;5;203mselect \x1b[00m\x1b[38;5;203m* \x1b[00m\x1b[1m\x1b[38;5;203mfrom \x1b[00m\x1b[1m\x1b[38;5;203mlineite\x1b[00m\x1b[38;5;203m;\x1b[00m\x1b[38;5;203m\n                      ^\x1b[00m\n".to_string(),
        ),
        (
            "light",
            "Catalog Error: Table with name t does not exist!\nDid you mean \"pg_tables\"?\n\nLINE 1: select 'x' + 1.5 from t where a = 1;\n                              ^",
            "\x1b[1m\x1b[38;5;124mCatalog Error:\n\x1b[00m\x1b[38;5;124mTable with name t does not exist!\nDid you mean \"\x1b[00m\x1b[1m\x1b[38;5;124mpg_tables\x1b[00m\x1b[38;5;124m\"?\n\n\x1b[00m\x1b[1m\x1b[38;5;124mLINE 1:\x1b[00m\x1b[38;5;124m \x1b[00m\x1b[1m\x1b[38;5;124mselect \x1b[00m\x1b[38;5;124m'x' \x1b[00m\x1b[38;5;124m+ \x1b[00m\x1b[38;5;124m1.5 \x1b[00m\x1b[1m\x1b[38;5;124mfrom \x1b[00m\x1b[1m\x1b[38;5;124mt \x1b[00m\x1b[1m\x1b[38;5;124mwhere \x1b[00m\x1b[38;5;124ma \x1b[00m\x1b[38;5;124m= \x1b[00m\x1b[38;5;124m1\x1b[00m\x1b[38;5;124m;\x1b[00m\x1b[38;5;124m\n                              ^\x1b[00m\n".to_string(),
        ),
        (
            "",
            "Invalid Command Error: Invalid usage of command '.highlight_mode'\n\nUsage: '.highlight_mode mixed|dark|light'",
            "\x1b[1m\x1b[31mInvalid Command Error:\n\x1b[00m\x1b[31mInvalid usage of command '\x1b[00m\x1b[1m\x1b[31m.highlight_mode\x1b[00m\x1b[31m'\n\nUsage: '\x1b[00m\x1b[1m\x1b[31m.highlight_mode mixed|dark|light\x1b[00m\x1b[31m'\x1b[00m\n".to_string(),
        ),
        (
            "",
            "Catalog Error: Table with name nope does not exist!\nDid you mean \"pg_enum\"?\n\nLINE 1: select 1 /* c */ from nope;\n                              ^",
            // 1.5.6 prints `1 /* c */ ` as one span; the baseline highlight tokenizer keeps the
            // comment as a token of its own
            "\x1b[1m\x1b[31mCatalog Error:\n\x1b[00m\x1b[31mTable with name nope does not exist!\nDid you mean \"\x1b[00m\x1b[1m\x1b[31mpg_enum\x1b[00m\x1b[31m\"?\n\n\x1b[00m\x1b[1m\x1b[31mLINE 1:\x1b[00m\x1b[31m \x1b[00m\x1b[1m\x1b[31mselect \x1b[00m\x1b[31m1 /* c */ \x1b[00m\x1b[1m\x1b[31mfrom \x1b[00m\x1b[1m\x1b[31mnope\x1b[00m\x1b[31m;\x1b[00m\x1b[31m\n                              ^\x1b[00m\n"
                .replace("\x1b[31m1 /* c */ \x1b[00m", "\x1b[31m1 \x1b[00m\x1b[31m/* c */ \x1b[00m"),
        ),
    ]
}

#[test]
fn print_error_matches_reference() {
    for (setup, message, expected) in error_cases() {
        let mut h = Highlighter::new();
        match setup {
            "" => {}
            "dark" => h.set_mode(HighlightMode::Dark),
            "light" => h.set_mode(HighlightMode::Light),
            _ => {
                let args: Vec<&str> = setup.split(' ').collect();
                h.highlight_colors_command(&args).unwrap();
            }
        }
        assert_eq!(h.error(message), expected, "{message}");
        assert_eq!(h.database_error(message, true), expected);
        assert_eq!(h.database_error(message, false), format!("{message}\n"));
    }
}

#[test]
fn error_without_highlighting_or_message() {
    let mut h = Highlighter::new();
    assert_eq!(h.error(""), "");
    h.set_enabled(false);
    assert_eq!(
        h.error("Binder Error: column \"x\" not found"),
        "Binder Error:\ncolumn \"x\" not found\n"
    );
}

#[test]
fn unknown_color() {
    let mut h = Highlighter::new();
    let Err(HighlightError::DatabaseError(message)) = h.set_color("column_name", "redXX", None) else {
        panic!("expected a database error");
    };
    let plain = "Unknown highlighting color 'redXX'\n\nDid you mean: \"red\", \"red1\", \"red3\", \"red4\"\nRun '.display_colors' for a list of available colors.\n";
    assert_eq!(message, plain);
    assert_eq!(h.database_error(&message, false), format!("{plain}\n"));
    assert_eq!(
        h.database_error(&message, true),
        "\x1b[31mUnknown highlighting color '\x1b[00m\x1b[1m\x1b[31mredXX\x1b[00m\x1b[31m'\n\nDid you mean: \"\x1b[00m\x1b[1m\x1b[31mred\x1b[00m\x1b[31m\", \"\x1b[00m\x1b[1m\x1b[31mred1\x1b[00m\x1b[31m\", \"\x1b[00m\x1b[1m\x1b[31mred3\x1b[00m\x1b[31m\", \"\x1b[00m\x1b[1m\x1b[31mred4\x1b[00m\x1b[31m\"\nRun '\x1b[00m\x1b[1m\x1b[31m.display_colors\x1b[00m\x1b[31m' for a list of available colors.\n\x1b[00m\n"
    );
}

#[test]
fn unknown_element_and_intensity() {
    let mut h = Highlighter::new();
    // the baseline element list (1.5.6 lacks prompt_connect and the explain_* elements)
    assert_eq!(
        h.set_color("column_nameXX", "red", None),
        Err(HighlightError::Stderr(
            "Unknown element 'column_nameXX', supported options: error, keyword, numeric_constant, string_constant, \
             line_indicator, database_name, schema_name, table_name, column_name, column_type, numeric_value, \
             string_value, temporal_value, null_value, footer, layout, startup_text, startup_version, continuation, \
             continuation_selected, bracket, comment, suggestion_catalog_name, suggestion_schema_name, \
             suggestion_table_name, suggestion_column_name, suggestion_file_name, suggestion_directory_name, \
             suggestion_function_name, suggestion_setting_name, table_layout, view_layout, primary_key_column, \
             prompt, prompt_connect, error_emphasis, error_suggestion, log_trace, log_debug, log_info, log_warning, \
             explain_layout, explain_operator, explain_operator_scan, explain_operator_join, \
             explain_operator_aggregate, explain_operator_order, explain_rows, explain_detail_key, \
             explain_detail_value, explain_header, explain_timing_critical, explain_timing_high, \
             explain_timing_moderate, explain_timing_low, none\n"
                .to_string()
        ))
    );
    assert_eq!(
        h.set_color("column_name", "red", Some("boldXX")),
        Err(HighlightError::Stderr(
            "Unknown intensity 'boldXX', supported options: standard, bold, underline\n".to_string()
        ))
    );
    assert_eq!(h.highlight_colors_command(&["column_name"]), Err(HighlightError::Usage));
    assert_eq!(
        h.highlight_colors_command(&["column_name", "red", "bold", "zz"]),
        Err(HighlightError::Usage)
    );
    // names are case-insensitive
    assert_eq!(h.set_color("COLUMN_NAME", "Red", Some("BOLD_UNDERLINE")), Ok(()));
    assert_eq!(
        h.element(HighlightElementType::ColumnName),
        (PrintColor::Code(1), PrintIntensity::BoldUnderline)
    );
}

#[test]
fn usage_error_text() {
    assert_eq!(
        usage_error("highlight_mode", "highlight_mode", "mixed|dark|light"),
        "Invalid Command Error: Invalid usage of command '.highlight_mode'\n\nUsage: '.highlight_mode mixed|dark|light'"
    );
    let mut h = Highlighter::new();
    assert_eq!(h.toggle_mode("foo"), Err(HighlightError::Usage));
    assert_eq!(h.toggle_mode("Dark"), Err(HighlightError::Usage));
    assert_eq!(h.display_colors(&["foo"]), Err(HighlightError::Usage));
}

#[test]
fn text_matches_shell_tests() {
    // test_highlighting.py: test_highlight_column_header / test_custom_highlight
    let mut h = Highlighter::new();
    assert_eq!(h.text("NULL", HighlightElementType::NullValue), "\x1b[90mNULL\x1b[00m");
    h.highlight_colors_command(&["column_name", "red", "bold"]).unwrap();
    h.highlight_colors_command(&["column_type", "yellow"]).unwrap();
    assert_eq!(
        h.text("l_comment", HighlightElementType::ColumnName),
        "\x1b[1m\x1b[31ml_comment\x1b[00m"
    );
    assert_eq!(
        h.text("varchar", HighlightElementType::ColumnType),
        "\x1b[33mvarchar\x1b[00m"
    );
    // the terminal default prints the text as-is
    assert_eq!(h.text("x", HighlightElementType::None), "x");
    h.set_enabled(false);
    assert_eq!(h.text("NULL", HighlightElementType::NullValue), "NULL");
}

#[test]
fn terminal_codes() {
    use PrintIntensity::*;
    assert_eq!(terminal_code(PrintColor::Standard, Standard), "");
    assert_eq!(terminal_code(PrintColor::Code(0), Standard), "\x1b[38;5;0m");
    assert_eq!(terminal_code(PrintColor::Code(1), Bold), "\x1b[1m\x1b[31m");
    assert_eq!(terminal_code(PrintColor::Code(7), Standard), "\x1b[37m");
    assert_eq!(terminal_code(PrintColor::Code(8), Underline), "\x1b[4m\x1b[90m");
    assert_eq!(
        terminal_code(PrintColor::Code(15), BoldUnderline),
        "\x1b[1m\x1b[4m\x1b[97m"
    );
    assert_eq!(terminal_code(PrintColor::Code(16), Italic), "\x1b[3m\x1b[38;5;16m");
    assert_eq!(terminal_code(PrintColor::Standard, BoldItalic), "\x1b[1m\x1b[3m");
    assert!(!PrintColor::Code(15).is_extended());
    assert!(PrintColor::Code(16).is_extended());
    assert!(!PrintColor::Standard.is_extended());
}

#[test]
fn deprecated_commands() {
    let mut h = Highlighter::new();
    for (command, element) in [
        ("keyword", HighlightElementType::Keyword),
        ("comment", HighlightElementType::Comment),
        ("error", HighlightElementType::ErrorToken),
        ("cont", HighlightElementType::Continuation),
        ("cont_sel", HighlightElementType::ContinuationSelected),
    ] {
        let (warning, result) = h.deprecated_set_color(command, "brightred").unwrap();
        let hint = element.name();
        assert_eq!(
            warning,
            format!(
                "WARNING: .{command} [COLOR] will be removed in a future release, use .highlight_colors {hint} brightred instead\n"
            )
        );
        assert_eq!(result, Ok(()));
        assert_eq!(h.element(element), (PrintColor::Code(9), PrintIntensity::Standard));
    }
    let (message, result) = h.deprecated_set_color("constant", "brightred").unwrap();
    assert_eq!(
        message,
        ".constant has been split into numeric_constant and string_constant, use .highlight_colors numeric_constant \
         brightred and .highlight_colors string_constant brightred instead\n"
    );
    assert_eq!(result, Err(HighlightError::Failed));
    assert!(h.deprecated_set_color("highlight", "red").is_none());
    let (_, result) = h.deprecated_set_color("keyword", "nocolor").unwrap();
    assert!(matches!(result, Err(HighlightError::DatabaseError(_))));
}

#[test]
fn display_colors_matches_reference() {
    let h = Highlighter::new();
    assert_eq!(h.display_colors(&[]).unwrap(), include_str!("display_colors.txt"));
    assert_eq!(
        h.display_colors(&["underline"]).unwrap(),
        include_str!("display_colors_underline.txt")
    );
    assert_eq!(
        h.display_colors(&["bold", "underline"]).unwrap(),
        include_str!("display_colors_bold_underline.txt")
    );
    let mut plain = Highlighter::new();
    plain.set_enabled(false);
    assert!(plain.display_colors(&[]).unwrap().starts_with("darkred1 red darkred2 "));
}

#[test]
fn modes_keep_user_colors() {
    let mut h = Highlighter::new();
    assert_eq!(h.mode(), HighlightMode::Automatic);
    h.highlight_colors_command(&["keyword", "red"]).unwrap();
    h.toggle_mode("dark").unwrap();
    assert_eq!(h.mode(), HighlightMode::Dark);
    assert_eq!(
        h.element(HighlightElementType::Keyword),
        (PrintColor::Code(1), PrintIntensity::Standard)
    );
    assert_eq!(
        h.element(HighlightElementType::StringConstant),
        (PrintColor::Code(220), PrintIntensity::Standard)
    );
    assert_eq!(h.element(HighlightElementType::ErrorToken).0, PrintColor::Code(203));
    // mixed mode does not reset the error colors
    h.toggle_mode("mixed").unwrap();
    assert_eq!(h.element(HighlightElementType::StringConstant).0, PrintColor::Code(3));
    assert_eq!(h.element(HighlightElementType::ErrorToken).0, PrintColor::Code(203));
    // auto records the mode and changes no colors
    h.toggle_mode("light").unwrap();
    h.toggle_mode("auto").unwrap();
    assert_eq!(h.mode(), HighlightMode::Automatic);
    assert_eq!(h.element(HighlightElementType::Prompt).0, PrintColor::Code(166));
    assert!(h.needs_mode_detection(true));
    assert!(!h.needs_mode_detection(false));
    h.apply_detected_mode(HighlightMode::Automatic);
    assert_eq!(h.mode(), HighlightMode::Mixed);
    assert_eq!(h.element(HighlightElementType::Prompt).0, PrintColor::Code(166));
    assert!(!h.needs_mode_detection(true));
}

#[test]
fn defaults_and_overrides() {
    let mut h = Highlighter::new();
    assert!(h.is_enabled());
    assert!(h.highlight_errors(true));
    assert!(!h.highlight_errors(false));
    assert!(!h.highlight_results(false));
    h.set_highlight_errors(true);
    h.set_highlight_results(false);
    assert!(h.highlight_errors(false));
    assert!(!h.highlight_results(true));
    h.apply_no_color(Some(std::ffi::OsStr::new("")));
    assert!(h.is_enabled());
    h.apply_no_color(None);
    assert!(h.is_enabled());
    h.apply_no_color(Some(std::ffi::OsStr::new("1")));
    assert!(!h.is_enabled());
    assert!(!h.needs_mode_detection(true));
}

#[test]
fn sql_highlighting() {
    let h = Highlighter::new();
    assert_eq!(
        h.highlight_sql("SELECT 42, 'x' -- c\nFROM t;"),
        "\x1b[32mSELECT \x1b[00m\x1b[33m42\x1b[00m, \x1b[33m'x' \x1b[00m\x1b[90m-- c\n\x1b[00m\x1b[32mFROM \x1b[00mt;"
    );
    // unterminated tokens are errors; leading whitespace is an identifier span
    assert_eq!(
        h.highlight_sql("  select 'abc"),
        "  \x1b[32mselect \x1b[00m\x1b[31m'abc\x1b[00m"
    );
    assert_eq!(h.highlight_sql("/* x"), "\x1b[31m/* x\x1b[00m");
    // a grammar word that is not a keyword
    assert_eq!(h.highlight_sql("repository"), "\x1b[32mrepository\x1b[00m");
    assert_eq!(h.highlight_sql(""), "");
    assert_eq!(h.highlight_sql("   "), "   ");
    let mut off = Highlighter::new();
    off.set_enabled(false);
    assert_eq!(off.highlight_sql("SELECT 1"), "SELECT 1");
    // a partial render
    let sql = "SELECT 1";
    assert_eq!(
        h.highlight_tokens(sql, 3, 8, &tokenize_sql(sql)),
        "\x1b[32mECT \x1b[00m\x1b[33m1\x1b[00m"
    );
}

#[test]
fn dot_command_highlighting() {
    let h = Highlighter::new();
    let line = ".mode box";
    assert_eq!(
        h.highlight_tokens(line, 0, line.len(), &tokenize_dot_command(line)),
        "\x1b[32m.mode \x1b[00m\x1b[33mbox\x1b[00m"
    );
}

#[test]
fn highlight_tokenizer() {
    fn kinds(sql: &str) -> Vec<(RawKind, &str, bool)> {
        tokenize_raw(sql)
            .into_iter()
            .map(|t| (t.kind, &sql[t.start..t.end], t.unterminated))
            .collect()
    }
    use RawKind::*;
    assert_eq!(
        kinds("select a::int, $1, $$x$$ /* c */ 1e-5+2;"),
        vec![
            (Keyword, "select", false),
            (Identifier, "a", false),
            (Operator, "::", false),
            (Keyword, "int", false),
            (Operator, ",", false),
            (Operator, "$", false),
            (NumberLiteral, "1", false),
            (Operator, ",", false),
            (StringLiteral, "$$x$$", false),
            (Comment, "/* c */", false),
            (NumberLiteral, "1e-5", false),
            (Operator, "+", false),
            (NumberLiteral, "2", false),
            (Terminator, ";", false),
            (EndOfInput, "", false),
        ]
    );
    assert_eq!(
        kinds("x >=- 1 \"q"),
        vec![
            (Identifier, "x", false),
            (Operator, ">=", false),
            (Operator, "-", false),
            (NumberLiteral, "1", false),
            (Identifier, "\"q", true),
            (EndOfInput, "", false),
        ]
    );
    assert_eq!(
        kinds("E'a\\'b' -- end"),
        vec![
            (StringLiteral, "E'a\\'b'", false),
            (Comment, "-- end", false),
            (EndOfInput, "", false),
        ]
    );
}

#[test]
fn error_tokens() {
    use SimplifiedTokenType::*;
    let tokens = tokenize_error("Table \"x\" not found\nLINE 1: select x;\n               ^");
    let spans: Vec<(SimplifiedTokenType, usize)> = tokens.iter().map(|t| (t.token_type, t.start)).collect();
    assert_eq!(
        spans,
        vec![
            (Error, 0),
            (ErrorSuggestion, 7),
            (Error, 8),
            (ErrorEmphasis, 20),
            (Error, 27),
            (ErrorEmphasis, 28),
            (ErrorEmphasis, 35),
            (Error, 36),
            (Error, 37),
        ]
    );
    // a caret before the query cannot be placed
    let tokens = tokenize_error("x\nLINE 1: a\n^");
    assert!(tokens.iter().all(|t| t.start != 0 || t.token_type == Error));
}

#[test]
fn candidates() {
    // the third line of the reference `.highlight_colors keyword COLOR` error
    for (color, expected) in [
        (
            "bleu",
            "Did you mean: \"blue\", \"blue1\", \"blue3\", \"blue4\", \"blueviolet\"",
        ),
        (
            "grey",
            "Did you mean: \"grey0\", \"grey100\", \"grey11\", \"grey15\", \"grey19\"",
        ),
        (
            "Gren",
            "Did you mean: \"green\", \"green1\", \"green3\", \"green4\", \"green5\"",
        ),
        ("zzzzzzzz", "Did you mean: \"red\""),
        (
            "darkorang",
            "Did you mean: \"darkorange\", \"darkorange3\", \"darkorange4\", \"darkcyan\"",
        ),
    ] {
        let message = PrintColor::from_name(color).unwrap_err();
        assert_eq!(message.lines().nth(2), Some(expected), "{color}");
    }
    assert_eq!(levenshtein("abc", "ABD", 3), 2);
    assert_eq!(levenshtein("", "ab", 1), 2);
}
