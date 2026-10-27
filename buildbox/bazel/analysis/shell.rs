//! Splitting and quoting words as Bazel's `ShellUtils` does.

/// Split `text` into words as a Bourne shell would, as Bazel tokenizes
/// options such as `--javacopt`: quotes group, and a backslash escapes the
/// next character, but only `\` and `"` in double quotes, and nothing in
/// single quotes. Fails on an unterminated quote or a trailing backslash.
pub fn tokenize(text: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut force_word = false;
    let mut quotation = None;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match quotation {
            Some(quote) if c == quote => quotation = None,
            Some('"') if c == '\\' => {
                let escaped = chars.next().ok_or("backslash at end of string")?;
                if escaped != '\\' && escaped != '"' {
                    word.push('\\');
                }
                word.push(escaped);
            }
            Some(_) => word.push(c),
            None if c == '\'' || c == '"' => {
                quotation = Some(c);
                force_word = true;
            }
            None if c == ' ' || c == '\t' => {
                if force_word || !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                    force_word = false;
                }
            }
            None if c == '\\' => word.push(chars.next().ok_or("backslash at end of string")?),
            None => word.push(c),
        }
    }
    if quotation.is_some() {
        return Err("unterminated quotation".to_string());
    }
    if force_word || !word.is_empty() {
        words.push(word);
    }
    Ok(words)
}

/// Quote a word for a Bourne shell, leaving it as it is if it's safe.
pub fn escape(word: &str) -> String {
    let safe = !word.is_empty() && word.chars().all(|c| c.is_alphanumeric() || "@%-_+:,./".contains(c));
    if safe {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_as_bazel_does() {
        assert_eq!(tokenize(r#"-a 'b c' "d\"e" f\ g"#), Ok(vec!["-a".into(), "b c".into(), "d\"e".into(), "f g".into()]));
        assert_eq!(tokenize(r#"'a\b' "a\b" '' x"#), Ok(vec![r"a\b".into(), r"a\b".into(), String::new(), "x".into()]));
        assert!(tokenize("'open").is_err());
        assert!(tokenize("end\\").is_err());
    }

    #[test]
    fn quotes_unsafe_words() {
        assert_eq!(escape("-Xlint:all"), "-Xlint:all");
        assert_eq!(escape(""), "''");
        assert_eq!(escape("it's"), "'it'\\''s'");
    }
}
