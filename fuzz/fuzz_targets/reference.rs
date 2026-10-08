//! An image reference as typed: either refused, or one whose canonical form
//! reads back as the same reference.
#![no_main]

use libfuzzer_sys::fuzz_target;
use shipyard_image::ImageReference;

fuzz_target!(|data: &[u8]| {
    let Ok(typed) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(reference) = ImageReference::parse(typed) else {
        return;
    };
    let canonical = reference.to_string();
    let again = ImageReference::parse(&canonical)
        .unwrap_or_else(|err| panic!("{typed:?} gave {canonical:?}, which is refused: {err}"));
    assert_eq!(again, reference, "{typed:?} gave {canonical:?}");
    let _ = reference.manifest_path();
});
