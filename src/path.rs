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
/// - `servers[port=8080].host` — same idea with an integer literal.
/// - `flags[primary=true].host` — boolean literal.
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
    /// `[key=<literal>]` — pin to the array item where `key` equals `<literal>`.
    Pinned { key: String, value: SelectorValue },
    /// `[key]` — fan out across every array item, using `key` as the
    /// identifier when matching items across source/target.
    Wildcard { key: String },
}

/// Typed literal for pinned selectors and wildcard identities. Comparison is
/// **strict**: `String("8080")` and `Int(8080)` are never equal. Syntax mirrors
/// the type — `[k="x"]` for strings, `[k=8080]` for ints, `[k=true]` for bools.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub enum SelectorValue {
    String(String),
    Int(i64),
    Bool(bool),
}

impl fmt::Display for SelectorValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SelectorValue::String(s) => write!(f, "\"{}\"", escape_for_quotes(s)),
            SelectorValue::Int(i) => write!(f, "{i}"),
            SelectorValue::Bool(b) => write!(f, "{b}"),
        }
    }
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
                    write!(f, "[{key}={value}]")?;
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
        let value = parse_selector_value(bytes, pos, input)?;
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

/// Parse a selector value literal at `*pos`. Accepts:
/// - `"..."` quoted string → `SelectorValue::String`
/// - bare `true` / `false` → `SelectorValue::Bool`
/// - bare digits (decimal, optional leading `-`) → `SelectorValue::Int`
///
/// The selector value's syntax determines its type — `[k=8080]` matches the
/// integer 8080 only, `[k="8080"]` matches the string `"8080"` only. Strict by
/// design so paths are unambiguous about identifier type.
fn parse_selector_value(bytes: &[char], pos: &mut usize, input: &str) -> Result<SelectorValue> {
    if *pos >= bytes.len() {
        bail!("selector value missing in {input}");
    }
    if bytes[*pos] == '"' {
        let s = parse_quoted(bytes, pos, input, '"')?;
        return Ok(SelectorValue::String(s));
    }
    let start = *pos;
    while *pos < bytes.len() && bytes[*pos] != ']' {
        *pos += 1;
    }
    let raw: String = bytes[start..*pos].iter().collect();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("selector value missing in {input}");
    }
    if trimmed != raw {
        bail!("selector value must not be padded with whitespace in {input}");
    }
    match trimmed {
        "true" => Ok(SelectorValue::Bool(true)),
        "false" => Ok(SelectorValue::Bool(false)),
        _ => trimmed.parse::<i64>().map(SelectorValue::Int).map_err(|_| {
            anyhow::anyhow!(
                "selector value {trimmed:?} is not a quoted string, integer, or boolean in {input}"
            )
        }),
    }
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
    use super::{FieldPath, ItemSelector, SelectorValue};

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
                assert_eq!(value, &SelectorValue::String("github".to_string()));
            }
            other => panic!("expected pinned selector, got {other:?}"),
        }
        assert!(path.segments()[1].select.is_none());
    }

    #[test]
    fn parses_int_selector_value() {
        let path = FieldPath::parse("servers[port=8080].host").unwrap();
        match &path.segments()[0].select {
            Some(ItemSelector::Pinned { key, value }) => {
                assert_eq!(key, "port");
                assert_eq!(value, &SelectorValue::Int(8080));
            }
            other => panic!("expected pinned int selector, got {other:?}"),
        }
    }

    #[test]
    fn parses_negative_int_selector_value() {
        let path = FieldPath::parse("a[k=-1].b").unwrap();
        match &path.segments()[0].select {
            Some(ItemSelector::Pinned {
                value: SelectorValue::Int(i),
                ..
            }) => assert_eq!(*i, -1),
            other => panic!("expected pinned int selector, got {other:?}"),
        }
    }

    #[test]
    fn parses_bool_selector_value() {
        let p_true = FieldPath::parse("a[primary=true].b").unwrap();
        let p_false = FieldPath::parse("a[primary=false].b").unwrap();
        assert!(matches!(
            &p_true.segments()[0].select,
            Some(ItemSelector::Pinned {
                value: SelectorValue::Bool(true),
                ..
            })
        ));
        assert!(matches!(
            &p_false.segments()[0].select,
            Some(ItemSelector::Pinned {
                value: SelectorValue::Bool(false),
                ..
            })
        ));
    }

    #[test]
    fn rejects_garbage_selector_value() {
        // bareword that is neither an int nor a bool
        assert!(FieldPath::parse("arr[name=github]").is_err());
        // empty
        assert!(FieldPath::parse("arr[name=]").is_err());
    }

    #[test]
    fn rejects_whitespace_padded_selector_value() {
        // Leading or trailing whitespace inside the brackets is rejected
        // — the literal must sit flush against `=` and `]`. Keeps parsing
        // unambiguous and avoids `[k= 8080]` accidentally matching `8080`
        // depending on how generously trim() is applied later.
        assert!(FieldPath::parse("arr[k= 8080]").is_err());
        assert!(FieldPath::parse("arr[k=8080 ]").is_err());
        assert!(FieldPath::parse("arr[k= true]").is_err());
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
                assert_eq!(value, &SelectorValue::String("he said \"hi\"".to_string()));
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
    fn display_round_trips_int_and_bool_selectors() {
        let pi = FieldPath::parse("servers[port=8080].host").unwrap();
        assert_eq!(pi.to_string(), "servers[port=8080].host");
        let pb = FieldPath::parse("a[primary=true].b").unwrap();
        assert_eq!(pb.to_string(), "a[primary=true].b");
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
