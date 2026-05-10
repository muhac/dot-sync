use std::fmt;

use anyhow::{Result, bail};

/// A `.sync.yaml` field path: a sequence of object keys, optionally with array
/// item selectors. Examples:
///
/// - `tui.theme` — descend into `tui`, then `theme`. Plain object navigation.
/// - `plugins."github@openai-curated".enabled` — quoted segment so `@` and `-`
///   in the key don't confuse the parser.
/// - `mcp_servers[name="github"].enabled` — descend into the `mcp_servers`
///   array, find the item where `name` equals the literal string `"github"`,
///   then descend into its `enabled` field. Pinned key-match.
/// - `mcp_servers[name].enabled` — wildcard variant: fan out across every
///   item in `mcp_servers`, keying matches across source/target by `name`.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FieldPath {
    segments: Vec<Segment>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Segment {
    /// Key under the current container (object/table). Always present.
    pub name: String,
    /// If `Some`, `name` refers to an array; the selector picks one item or
    /// fans out across all items.
    pub select: Option<ItemSelector>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ItemSelector {
    /// `[key="value"]` — pin to the array item where `key` equals `value`.
    Pinned { key: String, value: String },
    /// `[key]` — fan out across every array item, using `key` as the
    /// identifier when matching items across source/target.
    Wildcard { key: String },
}

impl FieldPath {
    pub fn parse(input: &str) -> Result<Self> {
        if input.is_empty() {
            bail!("path must not be empty");
        }

        let bytes: Vec<char> = input.chars().collect();
        let mut pos = 0usize;
        let mut segments = Vec::new();

        loop {
            let name = parse_key_name(&bytes, &mut pos, input)?;
            let select = parse_optional_selector(&bytes, &mut pos, input)?;
            segments.push(Segment { name, select });

            if pos == bytes.len() {
                break;
            }
            if bytes[pos] != '.' {
                bail!(
                    "unexpected character {:?} at position {pos} in {input}",
                    bytes[pos]
                );
            }
            pos += 1;
            if pos == bytes.len() {
                bail!("empty path segment in {input}");
            }
        }

        Ok(Self { segments })
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// Construct a FieldPath from already-validated segments. Used by
    /// `Document::expand` to emit resolved paths after wildcards have been
    /// substituted with `Pinned` values.
    pub fn from_segments(segments: Vec<Segment>) -> Self {
        Self { segments }
    }
}

impl fmt::Display for FieldPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, seg) in self.segments.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            write!(f, "{seg}")?;
        }
        Ok(())
    }
}

impl fmt::Display for Segment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if needs_quoting(&self.name) {
            write!(f, "\"{}\"", escape_for_quotes(&self.name))?;
        } else {
            f.write_str(&self.name)?;
        }
        if let Some(sel) = &self.select {
            match sel {
                ItemSelector::Pinned { key, value } => {
                    write!(f, "[{key}=\"{}\"]", escape_for_quotes(value))?;
                }
                ItemSelector::Wildcard { key } => {
                    write!(f, "[{key}]")?;
                }
            }
        }
        Ok(())
    }
}

fn needs_quoting(name: &str) -> bool {
    name.is_empty()
        || name
            .chars()
            .any(|c| c == '.' || c == '[' || c == ']' || c == '"' || c.is_whitespace())
}

fn escape_for_quotes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn parse_key_name(bytes: &[char], pos: &mut usize, input: &str) -> Result<String> {
    if *pos >= bytes.len() {
        bail!("empty path segment in {input}");
    }
    if bytes[*pos] == '"' {
        parse_quoted(bytes, pos, input, '"')
    } else {
        let start = *pos;
        while *pos < bytes.len() && bytes[*pos] != '.' && bytes[*pos] != '[' {
            *pos += 1;
        }
        if *pos == start {
            bail!("empty path segment in {input}");
        }
        Ok(bytes[start..*pos].iter().collect())
    }
}

fn parse_optional_selector(
    bytes: &[char],
    pos: &mut usize,
    input: &str,
) -> Result<Option<ItemSelector>> {
    if *pos >= bytes.len() || bytes[*pos] != '[' {
        return Ok(None);
    }
    *pos += 1; // consume '['

    // Selector key — bare identifier; quoted not yet supported.
    let key_start = *pos;
    while *pos < bytes.len() && bytes[*pos] != ']' && bytes[*pos] != '=' {
        *pos += 1;
    }
    if *pos == key_start {
        bail!("empty selector key in {input}");
    }
    let key: String = bytes[key_start..*pos].iter().collect();

    let selector = if *pos < bytes.len() && bytes[*pos] == '=' {
        *pos += 1; // consume '='
        if *pos >= bytes.len() || bytes[*pos] != '"' {
            bail!("selector value must be a quoted string in {input}");
        }
        let value = parse_quoted(bytes, pos, input, '"')?;
        ItemSelector::Pinned { key, value }
    } else {
        ItemSelector::Wildcard { key }
    };

    if *pos >= bytes.len() || bytes[*pos] != ']' {
        bail!("unterminated selector, expected ']' in {input}");
    }
    *pos += 1; // consume ']'
    Ok(Some(selector))
}

/// Parse a quoted string starting at the opening quote. Consumes through the
/// closing quote and returns the unescaped contents.
fn parse_quoted(bytes: &[char], pos: &mut usize, input: &str, quote: char) -> Result<String> {
    if *pos >= bytes.len() || bytes[*pos] != quote {
        bail!("expected opening quote {quote:?} in {input}");
    }
    *pos += 1; // consume opening quote
    let mut out = String::new();
    while *pos < bytes.len() {
        match bytes[*pos] {
            c if c == quote => {
                *pos += 1;
                return Ok(out);
            }
            '\\' => {
                *pos += 1;
                if *pos >= bytes.len() {
                    bail!("dangling escape in {input}");
                }
                out.push(bytes[*pos]);
                *pos += 1;
            }
            c => {
                out.push(c);
                *pos += 1;
            }
        }
    }
    bail!("unterminated quote in {input}");
}

#[cfg(test)]
mod tests {
    use super::{FieldPath, ItemSelector};

    fn names(path: &FieldPath) -> Vec<&str> {
        path.segments().iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn parses_plain_path() {
        let path = FieldPath::parse("tui.theme").unwrap();
        assert_eq!(names(&path), vec!["tui", "theme"]);
        assert!(path.segments().iter().all(|s| s.select.is_none()));
    }

    #[test]
    fn parses_quoted_segment() {
        let path = FieldPath::parse("plugins.\"github@openai-curated\".enabled").unwrap();
        assert_eq!(
            names(&path),
            vec!["plugins", "github@openai-curated", "enabled"]
        );
    }

    #[test]
    fn parses_pinned_key_match() {
        let path = FieldPath::parse("mcp_servers[name=\"github\"].enabled").unwrap();
        assert_eq!(names(&path), vec!["mcp_servers", "enabled"]);
        match &path.segments()[0].select {
            Some(ItemSelector::Pinned { key, value }) => {
                assert_eq!(key, "name");
                assert_eq!(value, "github");
            }
            other => panic!("expected pinned selector, got {other:?}"),
        }
        assert!(path.segments()[1].select.is_none());
    }

    #[test]
    fn parses_wildcard_key_match() {
        let path = FieldPath::parse("mcp_servers[name].enabled").unwrap();
        match &path.segments()[0].select {
            Some(ItemSelector::Wildcard { key }) => assert_eq!(key, "name"),
            other => panic!("expected wildcard selector, got {other:?}"),
        }
    }

    #[test]
    fn parses_nested_array_selectors() {
        let path = FieldPath::parse("a[k1=\"x\"].b[k2].c").unwrap();
        assert_eq!(names(&path), vec!["a", "b", "c"]);
        assert!(matches!(
            &path.segments()[0].select,
            Some(ItemSelector::Pinned { .. })
        ));
        assert!(matches!(
            &path.segments()[1].select,
            Some(ItemSelector::Wildcard { .. })
        ));
        assert!(path.segments()[2].select.is_none());
    }

    #[test]
    fn handles_escaped_quote_in_selector_value() {
        let path = FieldPath::parse(r#"a[k="he said \"hi\""].b"#).unwrap();
        match &path.segments()[0].select {
            Some(ItemSelector::Pinned { key, value }) => {
                assert_eq!(key, "k");
                assert_eq!(value, "he said \"hi\"");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_segments() {
        assert!(FieldPath::parse("tui..theme").is_err());
        assert!(FieldPath::parse("tui.").is_err());
        assert!(FieldPath::parse("").is_err());
    }

    #[test]
    fn rejects_unterminated_quotes() {
        assert!(FieldPath::parse("plugins.\"github").is_err());
    }

    #[test]
    fn rejects_unterminated_selector() {
        assert!(FieldPath::parse("arr[name").is_err());
        assert!(FieldPath::parse("arr[name=\"x\"").is_err());
    }

    #[test]
    fn rejects_unquoted_selector_value() {
        assert!(FieldPath::parse("arr[name=github]").is_err());
    }

    #[test]
    fn rejects_empty_selector_key() {
        assert!(FieldPath::parse("arr[]").is_err());
        assert!(FieldPath::parse("arr[=\"x\"]").is_err());
    }

    /// Parsing the Display output must produce the same FieldPath.
    /// The exact string may differ (we only quote segments that need it,
    /// while the input may quote even when not strictly required).
    fn assert_round_trips(input: &str) {
        let parsed = FieldPath::parse(input).unwrap();
        let displayed = parsed.to_string();
        let reparsed = FieldPath::parse(&displayed)
            .unwrap_or_else(|e| panic!("display {displayed:?} failed to reparse: {e}"));
        assert_eq!(parsed, reparsed, "{input} → {displayed}");
    }

    #[test]
    fn display_round_trips_plain_path() {
        assert_round_trips("tui.theme");
    }

    #[test]
    fn display_round_trips_quoted_segment() {
        assert_round_trips("plugins.\"github@openai-curated\".enabled");
    }

    #[test]
    fn display_round_trips_pinned_selector() {
        let path = FieldPath::parse("mcp_servers[name=\"github\"].enabled").unwrap();
        assert_eq!(path.to_string(), "mcp_servers[name=\"github\"].enabled");
    }

    #[test]
    fn display_round_trips_wildcard_selector() {
        let path = FieldPath::parse("mcp_servers[name].enabled").unwrap();
        assert_eq!(path.to_string(), "mcp_servers[name].enabled");
    }

    #[test]
    fn display_quotes_segments_that_need_it() {
        let path = FieldPath::parse("\"a.b\".c").unwrap();
        assert_eq!(path.to_string(), "\"a.b\".c");
    }
}
