//! The daemon's build fingerprint: a cheap hash of its own executable image.
//!
//! `PROTOCOL_VERSION` answers "can these two peers talk?" and nothing else. The
//! questions left over are about the *file*, and both are answered here with
//! one number and no remote dependency (no `sha256sum` on the machine):
//!
//! - *Is the file on disk the one I ship?* — `harvesterd build` prints the
//!   fingerprint of the binary that was just started, so the client compares it
//!   against the fingerprint of the binary it is about to upload.
//! - *Is the running daemon that file?* — the daemon fingerprints itself at
//!   startup and reports it in `ping`, so a daemon started before an upgrade is
//!   recognisable even though the file under it has already been replaced.
//!
//! **Not a security boundary.** The hash is non-cryptographic (FNV-1a, chosen
//! so the static musl binary earns no crypto dependency for a staleness check)
//! and anything that can answer on the socket at all can answer with any string
//! it likes. It detects an accidentally stale deploy — the failure that
//! actually happens — not a hostile one.

use std::io::Read;

/// The running process's own executable image. Read rather than
/// `std::env::current_exe()`'s path so the answer stays right after the file
/// underneath has been replaced by an upgrade: this is the image that is
/// *running*, which is exactly what `ping` claims to report. For the
/// `harvesterd build` subcommand the process was just launched from the file on
/// disk, so the same read answers the on-disk question too.
const RUNNING_EXECUTABLE: &str = "/proc/self/exe";

/// Bytes hashed per read. Big enough that a multi-megabyte binary is a handful
/// of syscalls, small enough to stay off the daemon's stack budget (it is a
/// heap buffer, but the constant is the tunable and gets a name either way).
const HASH_CHUNK_BYTES: usize = 64 * 1024;

/// FNV-1a, 64-bit — the reference constants.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

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

    fingerprint_reader(&mut executable)
}

/// `fnv1a64-<byte count>-<hash>`. The byte count is free to carry and makes the
/// value readable by a human comparing two deploys; the tag names the algorithm
/// so a future change to it is visible in the value rather than silent.
fn fingerprint_reader<R: Read>(reader: &mut R) -> std::io::Result<String> {
    let mut buf = vec![0_u8; HASH_CHUNK_BYTES];
    let mut hash = FNV_OFFSET_BASIS;
    let mut byte_count: u64 = 0;

    loop {
        let filled = reader.read(&mut buf)?;

        if filled == 0 {
            break;
        }

        byte_count += filled as u64;

        for byte in &buf[..filled] {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME);
        }
    }

    Ok(format!("fnv1a64-{byte_count}-{hash:016x}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published FNV-1a-64 test vectors, so a rewrite of the loop above
    /// cannot quietly become a different (still deterministic) function.
    #[test]
    fn the_hash_matches_the_reference_fnv1a64_vectors() {
        for (input, expected_hash) in [
            ("", 0xcbf2_9ce4_8422_2325_u64),
            ("a", 0xaf63_dc4c_8601_ec8c),
            ("foobar", 0x8594_4171_f739_67e8),
        ] {
            let fingerprint =
                fingerprint_reader(&mut input.as_bytes()).expect("a slice cannot fail to read");

            assert_eq!(
                fingerprint,
                format!("fnv1a64-{}-{expected_hash:016x}", input.len()),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn the_fingerprint_is_stable_across_calls() {
        let bytes = vec![7_u8; HASH_CHUNK_BYTES * 3 + 11];

        let first = fingerprint_reader(&mut bytes.as_slice()).expect("no io error");
        let second = fingerprint_reader(&mut bytes.as_slice()).expect("no io error");

        assert_eq!(first, second);
    }

    #[test]
    fn chunking_does_not_change_the_hash() {
        // The chunk loop is the only place a rewrite could corrupt state; an
        // independent single-pass hash over the same bytes is the check.
        let bytes: Vec<u8> = (0..HASH_CHUNK_BYTES * 2 + 3)
            .map(|index| (index % 251) as u8)
            .collect();
        let single_pass = bytes.iter().fold(FNV_OFFSET_BASIS, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
        });

        let fingerprint = fingerprint_reader(&mut bytes.as_slice()).expect("no io error");

        assert_eq!(
            fingerprint,
            format!("fnv1a64-{}-{single_pass:016x}", bytes.len())
        );
    }

    #[test]
    fn one_flipped_byte_changes_the_fingerprint() {
        // The whole job: a rebuilt binary of the same size must not compare
        // equal to the old one.
        let mut bytes = vec![0_u8; HASH_CHUNK_BYTES + 64];
        let unchanged = fingerprint_reader(&mut bytes.as_slice()).expect("no io error");
        bytes[HASH_CHUNK_BYTES] = 1;

        let changed = fingerprint_reader(&mut bytes.as_slice()).expect("no io error");

        assert_ne!(unchanged, changed);
    }

    #[test]
    fn a_truncated_binary_does_not_match_the_whole_one() {
        let bytes = vec![3_u8; 4096];

        let whole = fingerprint_reader(&mut bytes.as_slice()).expect("no io error");
        let truncated = fingerprint_reader(&mut &bytes[..2048]).expect("no io error");

        assert_ne!(whole, truncated);
    }

    #[test]
    fn the_running_executable_fingerprints() {
        // The test binary is itself a `/proc/self/exe`, so this exercises the
        // real read path rather than a fixture.
        let fingerprint =
            read_running_executable_fingerprint().expect("the test binary is readable");

        assert!(fingerprint.starts_with("fnv1a64-"), "{fingerprint}");
        assert_ne!(fingerprint, UNAVAILABLE_FINGERPRINT);
    }
}
