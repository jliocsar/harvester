//! `Sid` — a session id that has been checked against pi's charset.
//!
//! A sid arrives as an unvalidated remote string and is then used in five
//! roles: pid-table key, path component (`<sid>.stderr.log`), argv element,
//! envelope field and wire field. Validating once, at the edge, is what keeps
//! every one of those roles honest — the type is the proof that it happened.

use std::borrow::Borrow;
use std::fmt;

use serde::Serialize;

/// Pi's own session-id rule, verbatim from `assertValidSessionId`
/// (`session-manager.ts:212`, pi 0.83.0):
/// `^[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?$`.
///
/// Deliberately pi's charset and not "uuidv7", even though Harvester's own
/// clients mint uuidv7: a narrower rule here would make every non-uuid session
/// already on disk — pi's own naming is wider — unresumable through Harvester.
/// Hand-rolled rather than a regex crate: three char classes do not earn a
/// dependency in a binary that ships as a static musl blob.
fn is_valid_session_id(candidate: &str) -> bool {
    let alphanumeric = |character: char| character.is_ascii_alphanumeric();

    candidate.starts_with(alphanumeric)
        && candidate.ends_with(alphanumeric)
        && candidate
            .chars()
            .all(|character| alphanumeric(character) || matches!(character, '.' | '_' | '-'))
}

/// A validated session id. The wire representation is the plain string it
/// always was (`#[serde(transparent)]`), so the protocol is unchanged; the
/// newtype exists purely so an unchecked string cannot reach a path, an argv
/// or the pid table.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Sid(String);

impl Sid {
    /// The one gate. `Err` carries the message the client is answered with.
    /// Takes a `&str` (rather than the owned string off the wire) so a caller
    /// that must echo the *rejected* sid back still owns it.
    pub fn parse(raw: &str) -> Result<Sid, String> {
        if !is_valid_session_id(raw) {
            return Err(format!(
                "invalid sid '{raw}': must be non-empty, contain only \
                 alphanumerics, '.', '_' or '-', and start and end with an \
                 alphanumeric character"
            ));
        }

        Ok(Sid(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// So a `HashMap<Sid, _>` can be looked up by `&str` without allocating.
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

impl From<Sid> for String {
    fn from(sid: Sid) -> String {
        sid.0
    }
}

/// Pi's charset rule, plus the wire-compatibility guarantee the newtype makes.
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

    /// The envelope a client actually sends still decodes to the same string,
    /// and that string still survives the gate — `Sid` changed no wire byte.
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
