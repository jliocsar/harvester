use std::borrow::Borrow;
use std::fmt;

use serde::Serialize;

fn is_valid_session_id(candidate: &str) -> bool {
    let alphanumeric = |character: char| character.is_ascii_alphanumeric();

    candidate.starts_with(alphanumeric)
        && candidate.ends_with(alphanumeric)
        && candidate
            .chars()
            .all(|character| alphanumeric(character) || matches!(character, '.' | '_' | '-'))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub(crate) struct Sid(String);

impl Sid {
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        if !is_valid_session_id(raw) {
            return Err(format!(
                "invalid sid '{raw}': must be non-empty, contain only \
                 alphanumerics, '.', '_' or '-', and start and end with an \
                 alphanumeric character"
            ));
        }

        Ok(Self(raw.to_string()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for Sid {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parses(raw: &str) -> bool {
        Sid::parse(raw).is_ok()
    }

    #[test]
    fn a_uuidv7_is_accepted() {
        assert!(parses("0198a2f1-4b3c-7def-8901-23456789abcd"));
    }

    #[test]
    fn pis_wider_charset_is_accepted() {
        assert!(parses("a"));
        assert!(parses("9"));
        assert!(parses("my.session_name-2"));
    }

    #[test]
    fn an_empty_sid_is_refused() {
        assert!(!parses(""));
    }

    #[test]
    fn a_path_traversal_sid_is_refused() {
        assert!(!parses("../../.ssh/authorized_keys"));
        assert!(!parses(".."));
        assert!(!parses("a/b"));
    }

    #[test]
    fn separators_at_either_end_are_refused() {
        assert!(!parses("-abc"));
        assert!(!parses("abc-"));
        assert!(!parses("."));
        assert!(!parses("_"));
    }

    #[test]
    fn non_ascii_and_whitespace_are_refused() {
        assert!(!parses("séance"));
        assert!(!parses("two words"));
        assert!(!parses("nul\0byte"));
    }

    #[test]
    fn the_wire_representation_is_still_a_plain_string() {
        let sid = Sid::parse("01H8XYZ").expect("valid sid");

        assert_eq!(
            serde_json::to_string(&sid).expect("serializable"),
            "\"01H8XYZ\""
        );
    }

    #[test]
    fn a_wire_request_round_trips_through_the_gate() {
        let line = br#"{"id":1,"type":"pi","sid":"01H8XYZ","cmd":{"type":"get_state"}}"#;
        let request: harvester_proto::Request =
            serde_json::from_slice(line).expect("a well-formed request line");

        let harvester_proto::Request::Pi { sid, .. } = request else {
            panic!("decoded as the wrong request variant");
        };
        let sid = Sid::parse(&sid).expect("valid sid");

        assert_eq!(sid.as_str(), "01H8XYZ");
        assert_eq!(
            serde_json::to_value(&sid).expect("serializable"),
            serde_json::json!("01H8XYZ")
        );
    }
}
