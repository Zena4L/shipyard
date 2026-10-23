//! A request's query string (`?all=1&t=10`), its values decoded and read
//! as Docker reads them.

use std::str::FromStr;

use percent_encoding::percent_decode_str;

use crate::errors::ApiError;

/// A query's `key=value` pairs, decoded, in order.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Query(Vec<(String, String)>);

impl Query {
    /// The pairs of `query` (none if there's none): `+` is a space, `%XX`
    /// a byte, and a key without `=` has an empty value.
    ///
    /// # Errors
    ///
    /// A key or value that isn't UTF-8 once decoded (400).
    pub(crate) fn parse(query: Option<&str>) -> Result<Self, ApiError> {
        let decode = |text: &str| {
            let spaced = text.replace('+', " ");
            percent_decode_str(&spaced)
                .decode_utf8()
                .map(std::borrow::Cow::into_owned)
                .map_err(|_| ApiError::bad(format!("the query's {text:?} isn't UTF-8")))
        };
        let pairs = query
            .unwrap_or_default()
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                Ok((decode(key)?, decode(value)?))
            })
            .collect::<Result<_, ApiError>>()?;
        Ok(Self(pairs))
    }

    /// `key`'s value: its first, if it's given more than once (Go's
    /// `Form.Get`).
    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    /// `key` as Docker's flags are read (moby's `BoolValue`): false if
    /// it's absent, empty, `0`, `no`, `false` or `none` (any case), true
    /// otherwise.
    pub(crate) fn flag(&self, key: &str) -> bool {
        let value = self
            .get(key)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        !matches!(value.as_str(), "" | "0" | "no" | "false" | "none")
    }

    /// `key` as a `T`, if it's given and not empty.
    ///
    /// # Errors
    ///
    /// A value that isn't one (400).
    pub(crate) fn parsed<T: FromStr>(&self, key: &str) -> Result<Option<T>, ApiError> {
        match self.get(key).filter(|value| !value.is_empty()) {
            None => Ok(None),
            Some(value) => value
                .parse()
                .map(Some)
                .map_err(|_| ApiError::bad(format!("invalid value for {key}: {value:?}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_decoded() {
        let query = Query::parse(Some("name=my%2Dweb&label=a+b&all&t=10&t=20")).unwrap();
        assert_eq!(query.get("name"), Some("my-web"));
        assert_eq!(query.get("label"), Some("a b"));
        assert_eq!(query.get("all"), Some(""));
        assert_eq!(query.get("t"), Some("10"));
        assert_eq!(query.get("missing"), None);
        assert_eq!(Query::parse(None).unwrap(), Query::default());
        assert_eq!(
            Query::parse(Some("name=%ff")).unwrap_err().status,
            hyper::StatusCode::BAD_REQUEST
        );
    }

    /// moby's `BoolValue`.
    #[test]
    fn flags_are_read_as_docker_reads_them() {
        for yes in ["1", "true", "True", "yes", "anything"] {
            let query = Query::parse(Some(&format!("all={yes}"))).unwrap();
            assert!(query.flag("all"), "{yes}");
        }
        for no in ["", "0", "no", "false", "FALSE", "none"] {
            let query = Query::parse(Some(&format!("all={no}"))).unwrap();
            assert!(!query.flag("all"), "{no:?}");
        }
        assert!(!Query::default().flag("all"));
    }

    #[test]
    fn numbers_are_parsed_or_refused() {
        let query = Query::parse(Some("t=10&limit=&bad=x")).unwrap();
        assert_eq!(query.parsed::<u64>("t").unwrap(), Some(10));
        assert_eq!(query.parsed::<u64>("limit").unwrap(), None);
        assert_eq!(query.parsed::<u64>("missing").unwrap(), None);
        assert_eq!(
            query.parsed::<u64>("bad").unwrap_err().status,
            hyper::StatusCode::BAD_REQUEST
        );
    }
}
