use anyhow::{Result, bail};

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FieldPath {
    segments: Vec<String>,
}

impl FieldPath {
    pub fn parse(input: &str) -> Result<Self> {
        if input.is_empty() {
            bail!("path must not be empty");
        }

        let mut segments = Vec::new();
        let mut current = String::new();
        let mut chars = input.chars().peekable();
        let mut in_quotes = false;
        let mut quoted_segment = false;

        while let Some(ch) = chars.next() {
            match ch {
                '"' if current.is_empty() && !in_quotes => {
                    in_quotes = true;
                    quoted_segment = true;
                }
                '"' if in_quotes => {
                    in_quotes = false;
                    if matches!(chars.peek(), Some(next) if *next != '.') {
                        bail!("quoted path segment must end before '.' in {input}");
                    }
                }
                '\\' if in_quotes => {
                    let Some(escaped) = chars.next() else {
                        bail!("dangling escape in {input}");
                    };
                    current.push(escaped);
                }
                '.' if !in_quotes => {
                    if current.is_empty() {
                        bail!("empty path segment in {input}");
                    }
                    segments.push(std::mem::take(&mut current));
                    quoted_segment = false;
                }
                _ => {
                    if quoted_segment && !in_quotes {
                        bail!("quoted path segment must end before '.' in {input}");
                    }
                    current.push(ch);
                }
            }
        }

        if in_quotes {
            bail!("unterminated quote in {input}");
        }
        if current.is_empty() {
            bail!("empty path segment in {input}");
        }

        segments.push(current);
        Ok(Self { segments })
    }

    pub fn segments(&self) -> &[String] {
        &self.segments
    }
}

#[cfg(test)]
mod tests {
    use super::FieldPath;

    #[test]
    fn parses_plain_path() {
        let path = FieldPath::parse("tui.theme").unwrap();
        assert_eq!(path.segments(), &["tui", "theme"]);
    }

    #[test]
    fn parses_quoted_segment() {
        let path = FieldPath::parse("plugins.\"github@openai-curated\".enabled").unwrap();
        assert_eq!(
            path.segments(),
            &["plugins", "github@openai-curated", "enabled"]
        );
    }

    #[test]
    fn rejects_empty_segments() {
        assert!(FieldPath::parse("tui..theme").is_err());
        assert!(FieldPath::parse("tui.").is_err());
    }

    #[test]
    fn rejects_unterminated_quotes() {
        assert!(FieldPath::parse("plugins.\"github").is_err());
    }
}
