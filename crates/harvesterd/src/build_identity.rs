//! The daemon's build identity: which executable image is this process?
//!
//! `PROTOCOL_VERSION` answers "can these two peers talk?" and nothing else. The
//! questions left over are about the *file*, and both are answered here with
//! one string and no remote dependency (no `sha256sum` on the machine):
//!
//! - *Is the file on disk the one I ship?* — `harvesterd build` prints the
//!   fingerprint of the binary that was just started, so the client compares it
//!   against the fingerprint of the binary it is about to upload.
//! - *Is the running daemon that file?* — the daemon fingerprints itself at
//!   startup and reports it in `ping`, so a daemon started before an upgrade is
//!   recognisable even though the file under it has already been replaced.
//!
//! The hash itself lives in `harvester-proto`
//! ([`harvester_proto::fingerprint_reader`]), not here: the string is
//! wire-visible in `PingData.build` and the client must compute the identical
//! value, so there is exactly one implementation of it. What is genuinely
//! daemon-local — which file to read, and what to report when it cannot be
//! read — is what remains in this module.

/// The running process's own executable image. Read rather than
/// `std::env::current_exe()`'s path so the answer stays right after the file
/// underneath has been replaced by an upgrade: this is the image that is
/// *running*, which is exactly what `ping` claims to report. For the
/// `harvesterd build` subcommand the process was just launched from the file on
/// disk, so the same read answers the on-disk question too.
const RUNNING_EXECUTABLE: &str = "/proc/self/exe";

/// Reported when the executable cannot be read at all. Never equal to a real
/// fingerprint, so a client comparing for equality re-installs — the safe
/// direction: a needless upload beats a machine pinned to a stale daemon.
const UNAVAILABLE_FINGERPRINT: &str = "unavailable";

/// Fingerprint the running executable, or the sentinel if it cannot be read.
/// The failure is logged rather than fatal: an unreadable `/proc/self/exe`
/// (an exotic mount namespace) must not stop the daemon from serving sessions.
pub fn running_executable_fingerprint() -> String {
    match read_running_executable_fingerprint() {
        Ok(fingerprint) => fingerprint,
        Err(error) => {
            eprintln!("cannot fingerprint {RUNNING_EXECUTABLE}: {error}");

            UNAVAILABLE_FINGERPRINT.to_string()
        }
    }
}

/// The fallible half, so `harvesterd build` can fail loudly (a client asking
/// for a fingerprint must never be handed a sentinel it would compare as
/// "different forever") while the daemon degrades.
pub fn read_running_executable_fingerprint() -> std::io::Result<String> {
    let mut executable = std::fs::File::open(RUNNING_EXECUTABLE)?;

    harvester_proto::fingerprint_reader(&mut executable)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hash's own behaviour is pinned in `harvester-proto`; what is this
    /// module's to prove is that it reads the right file and gets a real answer
    /// — the test binary is itself a `/proc/self/exe`.
    #[test]
    fn the_running_executable_fingerprints() {
        let fingerprint =
            read_running_executable_fingerprint().expect("the test binary is readable");

        assert!(fingerprint.starts_with("fnv1a64-"), "{fingerprint}");
        assert_ne!(fingerprint, UNAVAILABLE_FINGERPRINT);
    }

    #[test]
    fn the_degraded_answer_is_never_a_real_fingerprint() {
        // What makes the sentinel safe: a client comparing it against any
        // fingerprint sees a difference and re-installs.
        assert_ne!(
            UNAVAILABLE_FINGERPRINT,
            harvester_proto::fingerprint_bytes(b"")
        );
        assert!(!UNAVAILABLE_FINGERPRINT.starts_with("fnv1a64-"));
    }
}
