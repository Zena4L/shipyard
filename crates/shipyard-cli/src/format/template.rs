//! `--format`'s Go templates, as Docker's CLI runs them (docker/cli's
//! `templates` and `formatter`), through `gotmpl` (D11.3c-1).
//!
//! `json` is each object's JSON, a line each (Docker's `{{json .}}`);
//! `table` alone is the list's own default; `table TEXT` aligns TEXT's
//! tab-separated cells (Go's `text/tabwriter`, Docker's settings) under a
//! header the same template writes from each field's title; anything else
//! is written once per object, a line each. `\t` and `\n` typed as two
//! characters are a tab and a line break. Docker's functions are there
//! (`json`, `split`, `join`, `title`, `lower`, `upper`, `pad`, `truncate`),
//! and `.Label "KEY"` (a method call, which `gotmpl` can't make: the parsed
//! tree is rewritten to call a function that reads the row's labels).
//!
//! What a template writes is meant for a terminal: a value from an object
//! (a name, a label, a command) has anything a terminal would act on
//! escaped (`\u{1b}`), tabs and line breaks too, so it can't forge a row or
//! a column; `json`'s output escapes the same characters JSON's way
//! (`\u001b`), so it stays JSON.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write;
use std::sync::{Arc, Mutex, PoisonError};

use gotmpl::parse::{CommandNode, Expr, ListNode, Node, Parser, PipeNode};
use gotmpl::{FuncMap, MissingKey, TemplateError, Value};
use serde::Serialize;

/// The name the template's tree is kept under.
const NAME: &str = "format";

/// The most spaces `pad` adds on a side: a column's padding, never a way
/// to make the command allocate gigabytes.
const PAD_MAX: usize = 4096;

/// A `--format` that couldn't be used: Docker's two failures.
#[derive(Debug, thiserror::Error)]
pub(crate) enum FormatError {
    /// The template doesn't parse.
    #[error("failed to parse template: {0}")]
    Parse(TemplateError),
    /// The template asked for what an object hasn't (a field, a type), or
    /// an object couldn't be made a template's value.
    #[error("failed to execute template: {0}")]
    Execute(TemplateError),
}

/// How a list is printed.
pub(crate) enum Output {
    /// Each object's JSON, a line each (`--format json`).
    Json,
    /// A Go template.
    Template(Box<Templated>),
}

/// A Go template's parts.
pub(crate) struct Templated {
    /// The template, for each object.
    rows: gotmpl::Template,
    /// The same template, with Docker's header functions, for a table's
    /// header (`None`: no table).
    header: Option<gotmpl::Template>,
    /// The labels of the object being written, for `.Label "KEY"`.
    labels: Labels,
}

/// The labels `.Label` reads: the object being written's, if it has any.
type Labels = Arc<Mutex<Option<BTreeMap<String, String>>>>;

/// An object a list prints: Docker's row, by its fields' names.
pub(crate) trait Row: Serialize {
    /// Each field's title in a table's header, by its name (Docker's).
    fn headers() -> &'static [(&'static str, &'static str)];

    /// What `-q` prints of it: its ID (a volume's name).
    fn id(&self) -> &str;

    /// Its labels, for `.Label "KEY"` (`None`: a kind of object without
    /// them, as Docker's images).
    fn labels(&self) -> Option<&BTreeMap<String, String>> {
        None
    }

    /// What a template reads of it: its JSON's fields, unless its template
    /// names differ (Docker's events: Go's names, `.Time`, `.Scope`).
    ///
    /// # Errors
    ///
    /// It can't be made a template's value.
    fn fields(&self) -> gotmpl::Result<Value> {
        gotmpl::to_value(self)
    }
}

impl Output {
    /// The output `format` asks for, Docker's way, or `default` (a list's
    /// own `table …`) without one. A field an object hasn't is an error.
    ///
    /// # Errors
    ///
    /// A template that doesn't parse ([`FormatError::Parse`]).
    pub(crate) fn parse(format: Option<&str>, default: &str) -> Result<Self, FormatError> {
        let format = match format {
            None | Some("" | "table") => default,
            Some("json") => return Ok(Self::Json),
            Some(format) => format,
        };
        let (table, text) = match format.strip_prefix("table") {
            Some(rest) => (true, rest),
            None => (false, format),
        };
        let text = text
            .trim_matches(' ')
            .replace("\\t", "\t")
            .replace("\\n", "\n");
        Self::template(&text, table, MissingKey::Error)
    }

    /// The output `format` asks for as a plain template, Docker's
    /// `events`': `json` is each object's JSON; anything else is the
    /// template as typed (no table, and `\t` is the two characters), a
    /// field an object hasn't written `<no value>` (Go's default: an event
    /// has the attributes its kind has).
    ///
    /// # Errors
    ///
    /// A template that doesn't parse ([`FormatError::Parse`]).
    pub(crate) fn lines(format: &str) -> Result<Self, FormatError> {
        if format == "json" {
            return Ok(Self::Json);
        }
        Self::template(format, false, MissingKey::Invalid)
    }

    /// The output `format` asks for over one typed object (`version`'s,
    /// `info`'s): `json` is its JSON; anything else the template as typed,
    /// a field it hasn't an error (Docker's, a Go struct's).
    ///
    /// # Errors
    ///
    /// A template that doesn't parse ([`FormatError::Parse`]).
    pub(crate) fn strict(format: &str) -> Result<Self, FormatError> {
        if format == "json" {
            return Ok(Self::Json);
        }
        Self::template(format, false, MissingKey::Error)
    }

    /// A template of `text`, a table's with a header, `missing` keys as it
    /// says.
    fn template(text: &str, table: bool, missing: MissingKey) -> Result<Self, FormatError> {
        let (tree, defines) = parsed(text)?;
        let labels: Labels = Arc::default();
        Ok(Self::Template(Box::new(Templated {
            rows: built(&tree, &defines, functions(&labels), missing),
            header: table
                .then(|| built(&tree, &defines, header_functions(), MissingKey::ZeroValue)),
            labels,
        })))
    }

    /// Writes `rows` to `out`: each one's JSON; or a table's header (with
    /// no rows too) and aligned cells; or each row's text and a line break.
    ///
    /// # Errors
    ///
    /// A template that asks for what a row hasn't ([`FormatError`]); a
    /// failed write (its `io::Error`, as it came).
    pub(crate) fn write<R: Row>(&self, out: &mut impl Write, rows: &[R]) -> anyhow::Result<()> {
        let Self::Template(templated) = self else {
            let mut lines = String::new();
            for row in rows {
                let value = gotmpl::to_value(row).map_err(FormatError::Execute)?;
                lines.push_str(&json(&value));
                lines.push('\n');
            }
            out.write_all(lines.as_bytes())?;
            return Ok(());
        };
        let Templated {
            rows: template,
            header,
            labels,
        } = templated.as_ref();
        let rendered = |template: &gotmpl::Template, value: &Value| -> anyhow::Result<String> {
            let mut text = Vec::new();
            template
                .execute_template(&mut text, NAME, value)
                .map_err(FormatError::Execute)?;
            let text = String::from_utf8_lossy(&text);
            Ok(format!(
                "{}\n",
                crate::output::printable_message(&shown(&text))
            ))
        };
        let mut lines = String::new();
        if let Some(header) = header {
            lines.push_str(&rendered(header, &headers::<R>())?);
        }
        for row in rows {
            *labels.lock().unwrap_or_else(PoisonError::into_inner) = row.labels().cloned();
            let value = row.fields().map_err(FormatError::Execute)?;
            lines.push_str(&rendered(template, &escaped(value))?);
        }
        if header.is_some() {
            // Docker's `tabwriter.NewWriter(out, 10, 1, 3, ' ', 0)`: a
            // column is `max(10, widest + 3)` wide. This port pads after
            // its minimum (`max(min, widest) + 3`), so 7 is Go's 10.
            let mut table = tabwriter::TabWriter::new(Vec::new()).minwidth(7).padding(3);
            table.write_all(lines.as_bytes())?;
            out.write_all(&table.into_inner()?)?;
        } else {
            out.write_all(lines.as_bytes())?;
        }
        Ok(())
    }
}

/// Where a template's value keeps a character a terminal would act on:
/// in Unicode's private use plane 15, at this plus the character (C0, C1
/// and the bidirectional ones are all below U+2070).
const HIDDEN: u32 = 0xF_0000;

/// The highest character [`HIDDEN`] can stand for.
const HIDDEN_LAST: u32 = HIDDEN + 0x206F;

/// `value` with every character in its strings that a terminal would act
/// on (tabs and line breaks too: a value's own would make a column or a
/// line) hidden ([`hidden`]): written as text, it's shown escaped
/// ([`shown`]); written by `json`, it's itself again ([`restored`]), so
/// JSON says the object's own data.
fn escaped(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(Arc::from(hidden(&text))),
        Value::List(items) => Value::List(items.iter().cloned().map(escaped).collect()),
        Value::Map(map) => Value::Map(Arc::new(
            map.iter()
                .map(|(key, value)| (Arc::clone(key), escaped(value.clone())))
                .collect(),
        )),
        other => other,
    }
}

/// `text` with each character a terminal would act on put in [`HIDDEN`]'s
/// place. A character of that place already in `text` is written as its
/// escape (`\u{f0041}`): never taken for a hidden one.
fn hidden(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if crate::output::acts_on_terminal(c) {
            out.push(char::from_u32(HIDDEN + u32::from(c)).unwrap_or('?'));
        } else if (HIDDEN..=HIDDEN_LAST).contains(&u32::from(c)) {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}

/// What a hidden character stands for, if `c` is one.
fn unhidden(c: char) -> Option<char> {
    (HIDDEN..=HIDDEN_LAST)
        .contains(&u32::from(c))
        .then(|| char::from_u32(u32::from(c) - HIDDEN))
        .flatten()
}

/// `text` as a terminal is shown it: each hidden character escaped
/// (`\u{1b}`).
fn shown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match unhidden(c) {
            Some(original) => out.extend(original.escape_unicode()),
            None => out.push(c),
        }
    }
    out
}

/// `text` with each hidden character itself again (for `json`, which
/// escapes it JSON's way).
fn restored(text: &str) -> String {
    text.chars().map(|c| unhidden(c).unwrap_or(c)).collect()
}

/// A template of `tree` and its `defines`, with `functions`, a key a map
/// hasn't read as `missing` says.
fn built(
    tree: &ListNode,
    defines: &[(String, ListNode)],
    functions: FuncMap,
    missing: MissingKey,
) -> gotmpl::Template {
    let mut template = gotmpl::Template::new(NAME)
        .funcs(functions)
        .missing_key(missing)
        .add_parse_tree(NAME, tree.clone());
    for (name, define) in defines {
        template = template.add_parse_tree(name, define.clone());
    }
    template
}

/// `R`'s header: each field's title, by its name.
fn headers<R: Row>() -> Value {
    Value::Map(Arc::new(
        R::headers()
            .iter()
            .map(|&(name, title)| (Arc::from(name), Value::String(Arc::from(title))))
            .collect(),
    ))
}

/// `text` parsed, with `.Label "KEY"` made a call of `label "KEY"`: the
/// tree, and each `define`d one by its name.
fn parsed(text: &str) -> Result<(ListNode, Vec<(String, ListNode)>), FormatError> {
    let (mut tree, defines) = Parser::new(text, "{{", "}}")
        .and_then(Parser::parse)
        .map_err(FormatError::Parse)?;
    rewrite_labels_in_list(&mut tree);
    let defines = defines
        .into_iter()
        .map(|mut define| {
            rewrite_labels_in_list(&mut define.body);
            (define.name.to_string(), define.body)
        })
        .collect();
    Ok((tree, defines))
}

/// [`rewrite_labels_in_pipe`] through every pipeline in `list`.
fn rewrite_labels_in_list(list: &mut ListNode) {
    for node in &mut list.nodes {
        match node {
            Node::List(list) => rewrite_labels_in_list(list),
            Node::Action(action) => rewrite_labels_in_pipe(&mut action.pipe),
            Node::If(branch) | Node::Range(branch) | Node::With(branch) => {
                rewrite_labels_in_pipe(&mut branch.pipe);
                rewrite_labels_in_list(&mut branch.body);
                if let Some(otherwise) = &mut branch.else_body {
                    rewrite_labels_in_list(otherwise);
                }
            }
            Node::Template(call) => {
                if let Some(pipe) = &mut call.pipe {
                    rewrite_labels_in_pipe(pipe);
                }
            }
            Node::Define(define) => rewrite_labels_in_list(&mut define.body),
            Node::Text(_) | Node::Break(_) | Node::Continue(_) => {}
        }
    }
}

/// `pipe`'s `.Label` given an argument (`.Label "KEY"`, or `"KEY" |
/// .Label`) made the function `label`: Go calls a method there, and
/// `gotmpl` has none.
fn rewrite_labels_in_pipe(pipe: &mut PipeNode) {
    for (index, command) in pipe.commands.iter_mut().enumerate() {
        rewrite_labels_in_command(command, index > 0);
    }
}

/// [`rewrite_labels_in_pipe`] for one command (`piped`: it's given the one before's
/// value).
fn rewrite_labels_in_command(command: &mut CommandNode, piped: bool) {
    for arg in &mut command.args {
        rewrite_labels_in_expr(arg);
    }
    let given_more = piped || command.args.len() > 1;
    if let Some(first) = command.args.first_mut()
        && let Expr::Field(pos, fields) = first
        && given_more
        && fields.len() == 1
        && fields[0] == "Label"
    {
        *first = Expr::Identifier(*pos, "label".into());
    }
}

/// [`rewrite_labels_in_pipe`] inside an argument (a parenthesised pipeline).
fn rewrite_labels_in_expr(expr: &mut Expr) {
    match expr {
        Expr::Pipe(_, pipe) => rewrite_labels_in_pipe(pipe),
        Expr::Chain(_, inner, _) => rewrite_labels_in_expr(inner),
        _ => {}
    }
}

/// An error a function gives.
fn failed(message: impl Into<String>) -> TemplateError {
    TemplateError::Exec(message.into())
}

/// `args[index]` as a string.
fn string_arg<'a>(name: &str, args: &'a [Value], index: usize) -> gotmpl::Result<&'a str> {
    args.get(index)
        .and_then(Value::as_str)
        .ok_or_else(|| failed(format!("{name}: argument {} isn't a string", index + 1)))
}

/// `args[index]` as a count.
fn count_arg(name: &str, args: &[Value], index: usize) -> gotmpl::Result<usize> {
    args.get(index)
        .and_then(Value::as_int)
        .and_then(|count| usize::try_from(count).ok())
        .ok_or_else(|| failed(format!("{name}: argument {} isn't a count", index + 1)))
}

/// A string as a template's value.
fn string(text: impl AsRef<str>) -> Value {
    Value::String(Arc::from(text.as_ref()))
}

/// Docker's functions (`templates.basicFunctions`), and `label` for
/// `.Label "KEY"`, reading `labels`.
fn functions(labels: &Labels) -> FuncMap {
    let mut functions = FuncMap::new();
    let mut add = |name: &str, function: gotmpl::ValueFunc| {
        functions.insert(name.to_owned(), function);
    };
    add(
        "json",
        Arc::new(|args| {
            let value = args.first().ok_or_else(|| failed("json: no argument"))?;
            Ok(string(json(value)))
        }),
    );
    add(
        "split",
        Arc::new(|args| {
            let text = string_arg("split", args, 0)?;
            let separator = string_arg("split", args, 1)?;
            let parts: Vec<Value> = if separator.is_empty() {
                text.chars().map(|c| string(c.to_string())).collect()
            } else {
                text.split(separator).map(string).collect()
            };
            Ok(Value::List(Arc::from(parts)))
        }),
    );
    add(
        "join",
        Arc::new(|args| {
            let separator = string_arg("join", args, 1)?;
            let words: Vec<String> = match args.first() {
                None | Some(Value::Nil) => Vec::new(),
                Some(Value::List(items)) => items.iter().map(ToString::to_string).collect(),
                // A map's values, sorted (Docker's).
                Some(Value::Map(map)) => {
                    let mut values: Vec<String> = map.values().map(ToString::to_string).collect();
                    values.sort();
                    values
                }
                Some(_) => return Err(failed("join: the first argument isn't a list")),
            };
            Ok(string(words.join(separator)))
        }),
    );
    add(
        "title",
        Arc::new(|args| Ok(string(title(string_arg("title", args, 0)?)))),
    );
    add(
        "lower",
        Arc::new(|args| Ok(string(string_arg("lower", args, 0)?.to_lowercase()))),
    );
    add(
        "upper",
        Arc::new(|args| Ok(string(string_arg("upper", args, 0)?.to_uppercase()))),
    );
    add("pad", Arc::new(pad));
    add(
        "truncate",
        Arc::new(|args| {
            let text = string_arg("truncate", args, 0)?;
            let length = count_arg("truncate", args, 1)?;
            Ok(string(text.chars().take(length).collect::<String>()))
        }),
    );
    let labels = Arc::clone(labels);
    add(
        "label",
        Arc::new(move |args| {
            let key = string_arg("Label", args, 0)?;
            let labels = labels.lock().unwrap_or_else(PoisonError::into_inner);
            let labels = labels
                .as_ref()
                .ok_or_else(|| failed("can't evaluate field Label: this object has no labels"))?;
            // Hidden as every value is ([`escaped`]).
            Ok(string(hidden(labels.get(key).map_or("", String::as_str))))
        }),
    );
    functions
}

/// Docker's header functions: each gives its first argument back, as a
/// field's title is (`pad` excepted, so a padded column's header is
/// padded too); `label` gives the label's key.
fn header_functions() -> FuncMap {
    let mut functions = FuncMap::new();
    for name in [
        "json", "split", "join", "title", "lower", "upper", "truncate", "label",
    ] {
        functions.insert(
            name.to_owned(),
            Arc::new(|args: &[Value]| Ok(args.first().cloned().unwrap_or(Value::Nil))),
        );
    }
    functions.insert("pad".to_owned(), Arc::new(pad));
    functions
}

/// `pad TEXT BEFORE AFTER`: TEXT with that many spaces before and after
/// it, if it isn't empty.
fn pad(args: &[Value]) -> gotmpl::Result<Value> {
    let text = string_arg("pad", args, 0)?;
    let before = count_arg("pad", args, 1)?;
    let after = count_arg("pad", args, 2)?;
    if before.max(after) > PAD_MAX {
        return Err(failed(format!("pad: at most {PAD_MAX} spaces a side")));
    }
    if text.is_empty() {
        return Ok(string(""));
    }
    Ok(string(format!(
        "{}{text}{}",
        " ".repeat(before),
        " ".repeat(after)
    )))
}

/// Docker's `title` (`cases.Title(language.Und, cases.NoLower)`): each
/// word's first letter upper case, the rest as it is (`o'neil` stays one
/// word).
fn title(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at_start = true;
    for c in text.chars() {
        if at_start {
            out.extend(c.to_uppercase());
        } else {
            out.push(c);
        }
        at_start = !(c.is_alphanumeric() || c == '_' || c == '\'');
    }
    out
}

/// An object `inspect` shows: its JSON.
#[derive(Serialize)]
#[serde(transparent)]
struct Object<'a>(&'a serde_json::Value);

impl Row for Object<'_> {
    fn headers() -> &'static [(&'static str, &'static str)] {
        &[]
    }

    fn id(&self) -> &'static str {
        ""
    }
}

/// Writes `objects` (what `inspect` found) as `format` asks, Docker's
/// way: none, a JSON array indented 4 spaces; `json`, one compact array;
/// a template, a line per object, any key an object hasn't an error
/// (D11.3c2-2). What a terminal would act on is escaped, as everywhere.
///
/// # Errors
///
/// A template that doesn't parse or asks for what an object hasn't
/// ([`FormatError`]); a failed write.
pub(crate) fn objects(
    out: &mut impl Write,
    format: Option<&str>,
    objects: &[serde_json::Value],
) -> anyhow::Result<()> {
    // An empty format is none, as Docker's.
    match format.filter(|format| !format.is_empty()) {
        None => {
            let mut text = Vec::new();
            let pretty = serde_json::ser::PrettyFormatter::with_indent(b"    ");
            let mut serializer = serde_json::Serializer::with_formatter(&mut text, pretty);
            objects.serialize(&mut serializer)?;
            let text = String::from_utf8(text)?;
            writeln!(out, "{}", terminal_safe(text))?;
        }
        Some("json") => {
            let text = serde_json::to_string(objects)?;
            writeln!(out, "{}", terminal_safe(text))?;
        }
        Some(text) => {
            let rows: Vec<Object<'_>> = objects.iter().map(Object).collect();
            Output::template(text, false, MissingKey::Error)?.write(out, &rows)?;
        }
    }
    Ok(())
}

/// `value` as Docker's `json` writes it: compact, keys sorted; and what a
/// terminal would act on escaped JSON's way. `<`, `>` and `&` are written
/// as they are (Docker's `{{json .Field}}`; its whole rows escape them,
/// `\u003e`: the same JSON, other bytes).
pub(crate) fn json(value: &Value) -> String {
    terminal_safe(serde_json::to_string(&json_value(value)).unwrap_or_default())
}

/// JSON `text` with what a terminal would act on escaped JSON's way
/// (`\u001b`): only ever inside a string, where it means the same (serde
/// escapes every control character there, so a line break left is the
/// JSON's own layout, kept).
pub(crate) fn terminal_safe(text: String) -> String {
    let escaped_here = |c: char| c != '\n' && crate::output::acts_on_terminal(c);
    if !text.chars().any(escaped_here) {
        return text;
    }
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if escaped_here(c) {
            // Only ever inside a string: JSON's own text is plain ASCII.
            let mut units = [0_u16; 2];
            for unit in c.encode_utf16(&mut units) {
                let _ = write!(escaped, "\\u{unit:04x}");
            }
        } else {
            escaped.push(c);
        }
    }
    escaped
}

/// A template's value as JSON.
fn json_value(value: &Value) -> serde_json::Value {
    use serde_json::Value as Json;
    match value {
        Value::Nil | Value::Function(_) => Json::Null,
        Value::Bool(on) => Json::Bool(*on),
        Value::Int(number) => Json::from(*number),
        Value::Uint(number) => Json::from(*number),
        Value::Float(number) => {
            serde_json::Number::from_f64(*number).map_or(Json::Null, Json::Number)
        }
        Value::String(text) => Json::String(restored(text)),
        Value::List(items) => Json::Array(items.iter().map(json_value).collect()),
        Value::Map(map) => Json::Object(
            map.iter()
                .map(|(key, value)| (key.to_string(), json_value(value)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests;
