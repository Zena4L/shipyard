//! Docker's `filters` as the API reads them: the engine's
//! ([`shipyard_engine::docker::filters`], shared with the CLI's `-f`), and
//! `GET /events`' flattened for the events' own reader.

use shipyard_engine::docker::filters::Given;

use crate::errors::ApiError;

/// A list's `filters` query: Docker's JSON, either form.
///
/// # Errors
///
/// Text that isn't Docker's JSON (400).
pub(crate) fn given(text: Option<&str>) -> Result<Given, ApiError> {
    Ok(Given::from_json(text)?)
}

/// Docker's `filters` JSON (either form) as `KEY=VALUE`s, in order, for a
/// filter the engine reads itself (the events'): none if it's absent or
/// empty.
///
/// # Errors
///
/// Text that isn't Docker's JSON (400).
pub(crate) fn flattened(text: Option<&str>) -> Result<Vec<String>, ApiError> {
    Ok(given(text)?.flattened())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both forms flattened to the engine's `KEY=VALUE`s; a bad one is a
    /// 400.
    #[test]
    fn filters_are_flattened_for_the_engine() {
        assert_eq!(
            flattened(Some(
                r#"{"type":["container"],"event":{"die":true,"start":false}}"#
            ))
            .unwrap(),
            vec!["event=die".to_owned(), "type=container".to_owned()]
        );
        assert!(flattened(None).unwrap().is_empty());
        assert_eq!(
            flattened(Some("[1]")).unwrap_err().status,
            hyper::StatusCode::BAD_REQUEST
        );
    }
}
