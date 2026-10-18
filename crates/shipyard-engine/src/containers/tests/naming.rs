//! Names and serials, made up or given, renames, and finding a container
//! by a reference.

use std::fs;

use super::*;
use crate::ErrorKind;

#[test]
fn names_are_unique_and_made_up_when_not_given() {
    let (engine, _, dir) = engine("names");
    let web = create_record(&engine, new(Some("web"))).unwrap();
    let err = create_record(&engine, new(Some("web"))).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Exists);
    assert!(err.to_string().contains(web.id.short()), "{err}");
    let made: Vec<Record> = (0..20)
        .map(|_| create_record(&engine, new(None)).unwrap())
        .collect();
    let names: std::collections::BTreeSet<_> = made.iter().map(|r| r.name.clone()).collect();
    assert_eq!(names.len(), 20);
    for record in &made {
        let (adjective, noun) = record.name.as_str().split_once('_').unwrap();
        assert!(!adjective.is_empty() && !noun.is_empty(), "{}", record.name);
    }
    fs::remove_dir_all(dir).unwrap();
}

/// A record renamed: found by its new name, not its old; another
/// container's name refused (its record unchanged); its own name (checked
/// apart from itself) fine; a name free again once its container has
/// another.
#[test]
fn a_record_is_renamed_to_a_free_name() {
    let (engine, _, dir) = engine("rename");
    let web = create_record(&engine, new(Some("web"))).unwrap();
    let db = create_record(&engine, new(Some("db"))).unwrap();
    let name = |text: &str| Name::parse(text).unwrap();
    let err = rename(&engine, &web.id, &name("db")).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Exists);
    assert!(err.to_string().contains(db.id.short()), "{err}");
    assert_eq!(find(&engine, "web").unwrap().id, web.id, "unchanged");
    check_name(&engine, &web.id, &name("web")).unwrap();
    let renamed = rename(&engine, &web.id, &name("site")).unwrap();
    assert_eq!(renamed.name, name("site"));
    assert_eq!(find(&engine, "site").unwrap().id, web.id);
    assert_eq!(
        find(&engine, "web").unwrap_err().kind(),
        ErrorKind::NotFound
    );
    rename(&engine, &db.id, &name("web")).unwrap();
    fs::remove_dir_all(dir).unwrap();
}

/// A serial drawn that's taken is drawn again; none free in the draws is
/// `Busy`.
#[test]
fn serials_are_unique() {
    let (engine, _, dir) = engine("serials");
    let first = Serial::new(42).unwrap();
    let mut always = || Ok(first);
    let one = create_drawing(&engine, new(None), &[], &mut always, &mut random_words).unwrap();
    assert_eq!(one.serial, first);
    let mut draws = [first, first, Serial::new(43).unwrap()].into_iter();
    let mut drawn = || Ok(draws.next().unwrap());
    let two = create_drawing(&engine, new(None), &[], &mut drawn, &mut random_words).unwrap();
    assert_eq!(two.serial.number(), 43);
    let err = create_drawing(&engine, new(None), &[], &mut always, &mut random_words).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Busy);
    fs::remove_dir_all(dir).unwrap();
}

/// A name is looked up before an ID prefix: a container named like the
/// start of another's ID is the one found by it.
#[test]
fn lookup_goes_id_name_serial_prefix() {
    let (engine, _, dir) = engine("lookup");
    let first = create_record(&engine, new(None)).unwrap();
    let named = create_record(&engine, new(Some(first.id.short()))).unwrap();
    assert_eq!(find(&engine, first.id.short()).unwrap(), named);
    assert_eq!(find(&engine, first.id.as_str()).unwrap(), first);
    assert_eq!(find(&engine, &first.id.as_str()[..13]).unwrap(), first);
    // A serial with a wrong check digit is a mistake, not a miss.
    let check = first.serial.check_digit();
    let wrong = format!("SHPU {:06} {}", first.serial.number(), (check + 1) % 10);
    assert_eq!(
        find(&engine, &wrong).unwrap_err().kind(),
        ErrorKind::InvalidRequest
    );
    // A serial no container has, and words no container is.
    let free = (0..1_000_000)
        .map(|n| Serial::new(n).unwrap())
        .find(|s| *s != first.serial && *s != named.serial)
        .unwrap();
    assert_eq!(
        find(&engine, &free.to_string()).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert_eq!(
        find(&engine, "nothing").unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert_eq!(find(&engine, "").unwrap_err().kind(), ErrorKind::NotFound);
    fs::remove_dir_all(dir).unwrap();
}

/// A prefix more than one ID starts with finds none, and says which.
#[test]
fn an_ambiguous_prefix_names_the_matches() {
    let (engine, _, dir) = engine("ambiguous");
    // Seventeen IDs: two start with the same hex digit.
    let made: Vec<Record> = (0..17)
        .map(|_| create_record(&engine, new(None)).unwrap())
        .collect();
    let digit = (b'0'..=b'f')
        .map(char::from)
        .find(|digit| {
            made.iter()
                .filter(|r| r.id.as_str().starts_with(*digit))
                .count()
                > 1
        })
        .unwrap();
    let err = find(&engine, &digit.to_string()).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidRequest);
    for record in made.iter().filter(|r| r.id.as_str().starts_with(digit)) {
        assert!(err.to_string().contains(record.id.short()), "{err}");
    }
    fs::remove_dir_all(dir).unwrap();
}

/// Once the draws of made-up names are all taken, they're numbered.
#[test]
fn made_up_names_are_numbered_once_the_draws_are_taken() {
    let (engine, _, dir) = engine("suffix");
    let mut same = || Ok([0, 0]);
    let names: Vec<String> = (0..3)
        .map(|_| {
            create_drawing(&engine, new(None), &[], &mut Serial::random, &mut same)
                .unwrap()
                .name
                .to_string()
        })
        .collect();
    assert_eq!(names, ["able_anchor", "able_anchor_2", "able_anchor_3"]);
    fs::remove_dir_all(dir).unwrap();
}

/// A made-up name avoids the names it's told to (F63: those its networks
/// hold, a container of another store's), whatever their case; the
/// control, the same draw without them, is the name avoided.
#[test]
fn made_up_names_avoid_the_names_held_elsewhere() {
    // Two stores alike, each with `able_anchor` taken: the second draw is
    // `able_anchor_2` in one (the control), past it in the other.
    let second = |label: &str, avoid: &[String]| {
        let (engine, _, dir) = engine(label);
        let mut same = || Ok([0, 0]);
        create_drawing(&engine, new(None), &[], &mut Serial::random, &mut same).unwrap();
        let name = create_drawing(&engine, new(None), avoid, &mut Serial::random, &mut same)
            .unwrap()
            .name;
        fs::remove_dir_all(dir).unwrap();
        name
    };
    assert_eq!(
        second("avoid-none", &[]).as_str(),
        "able_anchor_2",
        "the control"
    );
    assert_eq!(
        second("avoid", &["ABLE_ANCHOR_2".to_owned()]).as_str(),
        "able_anchor_3",
        "past the held one, any case"
    );
}

/// Two records with one name or serial (made by hand, or by two stores
/// merged): a reference to it finds neither.
#[test]
fn a_name_or_serial_two_records_have_is_ambiguous() {
    let (engine, _, dir) = engine("twice");
    let kept = create_record(&engine, new(Some("web"))).unwrap();
    let mut twin = kept.clone();
    twin.id = ContainerId::parse(&"6".repeat(64)).unwrap();
    let store = dir.join("containers");
    fs::create_dir(store.join(twin.id.as_str())).unwrap();
    fs::write(record_path(&store, &twin.id), twin.to_json_new().unwrap()).unwrap();
    for reference in ["web".to_owned(), kept.serial.to_string()] {
        let err = find(&engine, &reference).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidRequest, "{reference}");
        assert!(err.to_string().contains(twin.id.short()), "{err}");
    }
    assert_eq!(
        find(&engine, kept.id.as_str()).unwrap(),
        kept,
        "the ID is still one"
    );
    fs::remove_dir_all(dir).unwrap();
}
