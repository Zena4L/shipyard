use super::*;

/// A container as the captures below had it.
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Fake {
    #[serde(rename = "ID")]
    id: &'static str,
    names: &'static str,
    state: &'static str,
    labels: String,
    #[serde(skip)]
    label_map: BTreeMap<String, String>,
}

impl Row for Fake {
    fn headers() -> &'static [(&'static str, &'static str)] {
        &[
            ("ID", "CONTAINER ID"),
            ("Names", "NAMES"),
            ("State", "STATE"),
            ("Labels", "LABELS"),
        ]
    }

    fn id(&self) -> &str {
        self.id
    }

    fn labels(&self) -> Option<&BTreeMap<String, String>> {
        Some(&self.label_map)
    }
}

/// The three containers of Docker 29.8's captures (2026-10-20), newest
/// first.
fn fakes() -> Vec<Fake> {
    let fake = |id, names, state, labels: &[(&str, &str)]| {
        let label_map: BTreeMap<String, String> = labels
            .iter()
            .map(|&(key, value)| (key.to_owned(), value.to_owned()))
            .collect();
        Fake {
            id,
            names,
            state,
            labels: label_map
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(","),
            label_map,
        }
    };
    vec![
        fake("ddcb7aafe3f0", "shipyard-fmt-c", "exited", &[]),
        fake("28c194977890", "shipyard-fmt-b", "running", &[]),
        fake(
            "414e6478d130",
            "shipyard-fmt-a",
            "created",
            &[("app", "x"), ("tier", "web")],
        ),
    ]
}

/// What `format` writes for the captures' containers.
fn written(format: &str) -> Result<String, String> {
    let output = Output::parse(Some(format), "table {{.ID}}").map_err(|error| error.to_string())?;
    let mut out = Vec::new();
    output
        .write(&mut out, &fakes())
        .map_err(|error| error.to_string())?;
    Ok(String::from_utf8(out).unwrap())
}

/// Docker 29.8's `docker ps -a --format 'table {{.Names | upper}}\t{{json
/// .State}}\t{{truncate .ID 4}}\t{{pad .State 1 2}}|{{.Label "tier"}}'`,
/// byte for byte: the header from each field's title (the functions give
/// it back, `pad` pads it, `.Label` names the key), the cells aligned by
/// Go's tabwriter (at least 10 wide, 3 apart).
#[test]
fn a_table_is_docker_s_byte_for_byte() {
    let format = r#"table {{.Names | upper}}\t{{json .State}}\t{{truncate .ID 4}}\t{{pad .State 1 2}}|{{.Label "tier"}}"#;
    assert_eq!(
        written(format).unwrap(),
        "NAMES            STATE       CONTAINER ID    STATE  |tier\n\
         SHIPYARD-FMT-C   \"exited\"    ddcb            exited  |\n\
         SHIPYARD-FMT-B   \"running\"   28c1            running  |\n\
         SHIPYARD-FMT-A   \"created\"   414e            created  |web\n"
    );
}

/// Docker 29.8's `--format '{{join (split .Names "-") "+"}} {{title
/// .State}} {{json .Labels}} {{.Label "tier" | printf "%q"}}'`: a line per
/// container, nothing aligned.
#[test]
fn a_line_per_object_runs_docker_s_functions() {
    let format = r#"{{join (split .Names "-") "+"}} {{title .State}} {{json .Labels}} {{.Label "tier" | printf "%q"}}"#;
    assert_eq!(
        written(format).unwrap(),
        "shipyard+fmt+c Exited \"\" \"\"\n\
         shipyard+fmt+b Running \"\" \"\"\n\
         shipyard+fmt+a Created \"app=x,tier=web\" \"web\"\n"
    );
}

/// `\t` and `\n` typed as two characters are a tab and a line break, and a
/// line's own break follows (Docker's `x\ty\n`: `x`, a tab, `y`, two
/// breaks); outside a table, a tab isn't aligned.
#[test]
fn escapes_are_a_tab_and_a_line_break() {
    assert_eq!(written(r"x\ty\n").unwrap(), "x\ty\n\n".repeat(3));
}

/// `json` is each object, compact, its keys sorted; `table` alone is the
/// list's own table, with only its header for no objects.
#[test]
fn json_and_table_are_docker_s_words() {
    let first = written("json").unwrap().lines().next().unwrap().to_owned();
    assert_eq!(
        first,
        r#"{"ID":"ddcb7aafe3f0","Labels":"","Names":"shipyard-fmt-c","State":"exited"}"#
    );
    let output = Output::parse(Some("table"), "table {{.Names}}\t{{.State}}").unwrap();
    let mut out = Vec::new();
    output.write::<Fake>(&mut out, &[]).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "NAMES     STATE\n");
    let output = Output::parse(None, "table {{.Names}}").unwrap();
    let mut out = Vec::new();
    output.write(&mut out, &fakes()[..1]).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "NAMES\nshipyard-fmt-c\n");
}

/// A field an object hasn't, a template that doesn't parse, a function
/// given the wrong type: Docker's two failures, by kind.
#[test]
fn what_can_t_run_fails_by_kind() {
    assert!(matches!(
        Output::parse(Some("{{.Names"), ""),
        Err(FormatError::Parse(_))
    ));
    let nope = Output::parse(Some("{{.Nope}}"), "").unwrap();
    let error = nope.write(&mut Vec::new(), &fakes()).unwrap_err();
    assert!(
        matches!(error.downcast_ref(), Some(FormatError::Execute(_))),
        "{error:#}"
    );
    let typed = Output::parse(Some("{{truncate .ID .ID}}"), "").unwrap();
    let error = typed.write(&mut Vec::new(), &fakes()).unwrap_err();
    assert!(
        matches!(error.downcast_ref(), Some(FormatError::Execute(_))),
        "{error:#}"
    );
}

/// An object without labels, as Docker's images.
#[derive(Serialize)]
struct Unlabelled {
    #[serde(rename = "ID")]
    id: &'static str,
}
impl Row for Unlabelled {
    fn headers() -> &'static [(&'static str, &'static str)] {
        &[("ID", "IMAGE ID")]
    }

    fn id(&self) -> &str {
        self.id
    }
}

/// `.Label` reads the object's own labels, in a pipeline too; an object
/// without labels (Docker's images) can't be asked.
#[test]
fn a_label_is_read_by_its_key() {
    assert_eq!(
        written(r#"{{"tier" | .Label}}|{{if .Label "app"}}app{{end}}"#).unwrap(),
        "|\n|\nweb|app\n"
    );
    let output = Output::parse(Some(r#"{{.Label "x"}}"#), "").unwrap();
    assert!(
        output
            .write(&mut Vec::new(), &[Unlabelled { id: "a" }])
            .is_err()
    );
}

/// What a terminal would act on is escaped: in text as `\u{1b}`, in
/// `json` as JSON's `\u001b` (still JSON); tabs and breaks are kept.
#[test]
fn what_a_terminal_acts_on_is_escaped() {
    let hostile = Fake {
        id: "a",
        names: "evil\u{1b}[2J\u{202e}",
        state: "exited",
        labels: String::new(),
        label_map: BTreeMap::new(),
    };
    let write = |format| {
        let mut out = Vec::new();
        Output::parse(Some(format), "")
            .unwrap()
            .write(&mut out, std::slice::from_ref(&hostile))
            .unwrap();
        String::from_utf8(out).unwrap()
    };
    assert_eq!(write(r"{{.Names}}\t."), "evil\\u{1b}[2J\\u{202e}\t.\n");
    // `json` inside a template writes the object's own text, escaped
    // JSON's way (`\u001b`, `\u202e`): nothing raw, and the same data read
    // back (an `inspect -f '{{json .Config.Cmd}}' | jq` gets what's there).
    let json = write("{{json .Names}}");
    assert_eq!(json, "\"evil\\u001b[2J\\u202e\"\n");
    let parsed: String = serde_json::from_str(json.trim_end()).unwrap();
    assert_eq!(parsed, hostile.names);
    // `--format json` writes the object's own text, escaped JSON's way:
    // the same value read back.
    let json = write("json");
    assert!(json.contains("evil\\u001b[2J\\u202e"), "{json}");
    let row: serde_json::Value = serde_json::from_str(json.trim_end()).unwrap();
    assert_eq!(row["Names"], hostile.names, "still JSON, and the same text");
}

/// Docker's `title`, `pad` on nothing, `split` by nothing.
#[test]
fn docker_s_functions_are_go_s() {
    assert_eq!(title("up 3 minutes, o'neil_x"), "Up 3 Minutes, O'neil_x");
    let value = |format: &str| {
        let output = Output::parse(Some(format), "").unwrap();
        let mut out = Vec::new();
        output.write(&mut out, &fakes()[..1]).unwrap();
        String::from_utf8(out).unwrap()
    };
    assert_eq!(value(r#"[{{pad "" 2 2}}]"#), "[]\n");
    assert_eq!(value(r#"{{join (split "abc" "") ","}}"#), "a,b,c\n");
    assert_eq!(value(r#"{{truncate "héllo" 2}}"#), "hé\n");
    assert_eq!(value(r#"{{lower "AbC"}}{{upper "aBc"}}"#), "abcABC\n");
}

/// `events`' template is taken as typed, as Docker's (2026-10-20's
/// capture: `table {{.Type}}` printed `table container`; `\t` stayed two
/// characters); `json` is still `{{json .}}`.
#[test]
fn a_plain_template_is_as_typed() {
    let write = |format: &str| {
        let mut out = Vec::new();
        Output::lines(format)
            .unwrap()
            .write(&mut out, &fakes()[..1])
            .unwrap();
        String::from_utf8(out).unwrap()
    };
    assert_eq!(write("table {{.State}}"), "table exited\n");
    assert_eq!(write(r"{{.State}}\t."), "exited\\t.\n");
    assert!(write("json").starts_with(r#"{"ID":"ddcb7aafe3f0""#));
}

/// A value can't make a row or a column of its own: its tabs and line
/// breaks are escaped (a label's too), the template's own kept.
#[test]
fn a_value_can_t_forge_a_row() {
    let forged = Fake {
        id: "a",
        names: "web\tlocal\nevil",
        state: "exited",
        labels: String::new(),
        label_map: BTreeMap::from([("note".to_owned(), "x\ny".to_owned())]),
    };
    let mut out = Vec::new();
    Output::parse(Some(r#"table {{.Names}}\t{{.Label "note"}}"#), "")
        .unwrap()
        .write(&mut out, std::slice::from_ref(&forged))
        .unwrap();
    let out = String::from_utf8(out).unwrap();
    assert_eq!(out.lines().count(), 2, "a header and one row: {out}");
    assert!(out.contains("web\\u{9}local\\u{a}evil   x\\u{a}y"), "{out}");
    // `json` writes the object's own text, escaped JSON's way: still the
    // same value read back.
    let mut out = Vec::new();
    Output::parse(Some("json"), "")
        .unwrap()
        .write(&mut out, std::slice::from_ref(&forged))
        .unwrap();
    let row: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(row["Names"], forged.names);
}

/// `pad` pads a column, at most 4096 spaces a side: never a way to make
/// the command allocate gigabytes.
#[test]
fn pad_is_bounded() {
    let write = |format: &str| {
        Output::parse(Some(format), "")
            .unwrap()
            .write(&mut Vec::new(), &fakes()[..1])
    };
    assert!(write("{{pad .Names 4096 0}}").is_ok());
    let error = write("{{pad .Names 10000000000 0}}").unwrap_err();
    assert!(
        matches!(error.downcast_ref(), Some(FormatError::Execute(_))),
        "{error:#}"
    );
}

/// A plain template (`events`') writes Go's `<no value>` for a key an
/// object hasn't, as Docker's: an event has the attributes its kind has.
#[test]
fn a_plain_template_s_missing_key_is_no_value() {
    let mut out = Vec::new();
    Output::lines("{{.State}} {{.Nope}}")
        .unwrap()
        .write(&mut out, &fakes()[..1])
        .unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "exited <no value>\n");
}

/// `inspect`'s output, Docker's (2026-10-20's captures): an array indented
/// 4 spaces; `json` one compact array; a template a line per object, any
/// key an object hasn't an error (D11.3c2-2); what a terminal acts on
/// escaped, the JSON's own line breaks kept.
#[test]
fn objects_are_shown_as_docker_s_inspect() {
    let found = [
        serde_json::json!({"Name": "data", "Labels": {"k": "v"}}),
        serde_json::json!({"Name": "evil\u{1b}[2J", "Labels": {}}),
    ];
    let shown = |format: Option<&str>| {
        let mut out = Vec::new();
        objects(&mut out, format, &found).map(|()| String::from_utf8(out).unwrap())
    };
    let pretty = shown(None).unwrap();
    assert!(
        pretty.starts_with("[\n    {\n        \"Labels\": {\n            \"k\": \"v\""),
        "{pretty}"
    );
    assert!(pretty.contains("evil\\u001b[2J"), "{pretty}");
    let parsed: serde_json::Value = serde_json::from_str(&pretty).unwrap();
    assert_eq!(parsed[1]["Name"], "evil\u{1b}[2J");
    assert_eq!(
        shown(Some("json")).unwrap(),
        "[{\"Labels\":{\"k\":\"v\"},\"Name\":\"data\"},{\"Labels\":{},\"Name\":\"evil\\u001b[2J\"}]\n"
    );
    assert_eq!(
        shown(Some("{{.Name}} {{json .Labels}}")).unwrap(),
        "data {\"k\":\"v\"}\nevil\\u{1b}[2J {}\n"
    );
    for missing in ["{{.Nope}}", "{{.Labels.nope}}"] {
        let error = shown(Some(missing)).unwrap_err();
        assert!(
            matches!(error.downcast_ref(), Some(FormatError::Execute(_))),
            "{missing}: {error:#}"
        );
    }
}

/// What serde leaves raw (C1's `\u{9b}`, bidi's `\u{202e}`) is escaped in
/// `inspect`'s JSON too; a value's line break is the value's own to
/// `json` (read back the same) and escaped as text; a character of the
/// private use place hidden ones take is never taken for one.
#[test]
fn inspect_s_json_says_the_object_s_own_data() {
    let found = [serde_json::json!({
        "Name": "a\u{9b}b\u{202e}",
        "Cmd": ["sh", "-c", "a\nb"],
        "Odd": "\u{f001b}"
    })];
    let shown = |format: Option<&str>| {
        let mut out = Vec::new();
        objects(&mut out, format, &found).unwrap();
        String::from_utf8(out).unwrap()
    };
    for format in [None, Some("json")] {
        let text = shown(format);
        assert!(text.contains("a\\u009bb\\u202e"), "{text}");
        assert!(
            !text.contains('\u{9b}') && !text.contains('\u{202e}'),
            "{text}"
        );
        let read: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(read[0], found[0], "the same data");
    }
    let cmd = shown(Some("{{json .Cmd}}"));
    let read: serde_json::Value = serde_json::from_str(&cmd).unwrap();
    assert_eq!(read, found[0]["Cmd"], "{cmd}");
    assert_eq!(shown(Some("{{index .Cmd 2}}")), "a\\u{a}b\n");
    assert_eq!(shown(Some("{{.Odd}}")), "\\u{f001b}\n", "not an escape");
}

/// An empty format is none, as Docker's: `inspect -f ""` prints the array.
#[test]
fn an_empty_format_is_none() {
    let found = [serde_json::json!({"Name": "data"})];
    let mut empty = Vec::new();
    objects(&mut empty, Some(""), &found).unwrap();
    let mut none = Vec::new();
    objects(&mut none, None, &found).unwrap();
    assert_eq!(empty, none);
    assert!(String::from_utf8(none).unwrap().starts_with("[\n    {"));
}
