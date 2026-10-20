//! Fails when a user-visible string is handed to a GTK/libadwaita setter as a bare literal
//! instead of going through `i18n::tr*()` — so new UI text can't silently skip translation.
//!
//! It is a source scan, not a compiler pass, so it is deliberately simple and only looks at the
//! setters that carry text onto the screen (see `TEXT_SETTERS`). Text that reaches the screen
//! another way (a `format!` result in a variable, a `&'static str` table) is not seen here; the
//! pseudo-locale walk in `docs/i18n.md` catches those.
//!
//! A literal that is genuinely not for translation (a placeholder glyph, a file extension, an
//! example URL) can opt out with a `// i18n: ignore` comment on the same line.
//!
//! This lives in `tests/` (a separate test binary) so it runs without compiling the app itself.

use std::fs;
use std::path::{Path, PathBuf};

/// Methods whose first argument is text shown to the user.
const TEXT_SETTERS: &[&str] = &[
    ".label(",
    ".title(",
    ".subtitle(",
    ".tooltip_text(",
    ".tooltip_markup(",
    ".placeholder_text(",
    ".description(",
    ".heading(",
    ".body(",
    ".markup(",
    ".set_label(",
    ".set_title(",
    ".set_subtitle(",
    ".set_tooltip_text(",
    ".set_tooltip_markup(",
    ".set_placeholder_text(",
    ".set_description(",
    ".set_heading(",
    ".set_body(",
    ".set_markup(",
    ".set_text(",
    ".set_text_inner(",
    "Label::new(",
    "Button::with_label(",
    "Toast::new(",
    "AccessibleProperty::Label(",
    "AccessibleProperty::Description(",
];

/// Methods whose *second* argument is the shown text (`add_response("id", "Label")`).
const SECOND_ARG_SETTERS: &[&str] = &[
    ".add_response(",
    ".append_item(",
    "a11y::name(",
    "a11y::describe(",
    "a11y::name_if_changed(",
    "a11y::describe_if_changed(",
    "a11y::landing(",
];

fn source_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            source_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The part of a file that ships: everything before a top-level `mod tests`.
fn production_part(text: &str) -> &str {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end();
        if trimmed.starts_with("mod tests") || trimmed.starts_with("pub(crate) mod tests") || trimmed.starts_with("pub mod tests") {
            // A preceding `#[cfg(test)]` line belongs to the test module too, but contains no literals.
            return &text[..offset];
        }
        offset += line.len();
    }
    text
}

/// Whether `rest` (text right after an opening paren) starts with a literal text argument:
/// `"…"`, `Some("…")`, `&format!("…`, or `format!("…`.
fn starts_with_literal(rest: &str) -> bool {
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("Some(").map(str::trim_start).unwrap_or(rest);
    let rest = rest.strip_prefix('&').unwrap_or(rest);
    rest.starts_with('"') || rest.starts_with("format!(\"") || rest.starts_with("\"")
}

/// Skips the first argument (to the first top-level comma) and returns the text after it.
fn after_first_arg(rest: &str) -> Option<&str> {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut prev = ' ';
    for (i, c) in rest.char_indices() {
        if in_str {
            if c == '"' && prev != '\\' {
                in_str = false;
            }
        } else {
            match c {
                '"' => in_str = true,
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => {
                    if depth == 0 {
                        return None;
                    }
                    depth -= 1;
                }
                ',' if depth == 0 => return Some(&rest[i + 1..]),
                _ => {}
            }
        }
        prev = c;
    }
    None
}

fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].bytes().filter(|&b| b == b'\n').count() + 1
}

fn line_text(text: &str, offset: usize) -> &str {
    let start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    let end = text[offset..].find('\n').map_or(text.len(), |i| offset + i);
    &text[start..end]
}

/// Whether the line holding the literal (the line after `rest`'s leading whitespace) opts out.
fn ignored(text: &str, call_end: usize) -> bool {
    let skip = text[call_end..].len() - text[call_end..].trim_start().len();
    let at = call_end + skip;
    line_text(text, at).contains("// i18n: ignore") || line_text(text, call_end).contains("// i18n: ignore")
}

#[test]
fn ui_text_goes_through_the_translation_helpers() {
    let mut files = Vec::new();
    source_files(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
    files.sort();

    let mut offenders = Vec::new();
    for file in &files {
        let name = file.file_name().unwrap().to_string_lossy();
        if name == "i18n.rs" || name == "test_support.rs" {
            continue;
        }
        let full = fs::read_to_string(file).unwrap();
        let text = production_part(&full);
        let rel = file.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap().display().to_string();

        for setter in TEXT_SETTERS {
            for (at, _) in text.match_indices(setter) {
                let call_end = at + setter.len();
                if starts_with_literal(&text[call_end..]) && !ignored(text, call_end) {
                    offenders.push(format!("{rel}:{}: {}", line_of(text, at), line_text(text, at).trim()));
                }
            }
        }
        for setter in SECOND_ARG_SETTERS {
            for (at, _) in text.match_indices(setter) {
                let call_end = at + setter.len();
                if let Some(second) = after_first_arg(&text[call_end..]) {
                    if starts_with_literal(second) && !ignored(text, call_end) {
                        offenders.push(format!("{rel}:{}: {}", line_of(text, at), line_text(text, at).trim()));
                    }
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "{} user-visible string(s) bypass the translation helpers (wrap them in i18n::tr/tr_args/ntr, or add \
         `// i18n: ignore` if the text is not for translation):\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}
