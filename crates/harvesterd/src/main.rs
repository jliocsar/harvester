mod daemon;
mod fslist;
mod ops;
mod passthrough;
mod paths;
mod server;
mod session;
mod sid;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tokio::net::UnixListener;

const LOCK_FILE_NAME: &str = "harvesterd.lock";

const RUNNING_EXECUTABLE: &str = "/proc/self/exe";

const UNAVAILABLE_FINGERPRINT: &str = "unavailable";

fn fingerprint_running_executable() -> std::io::Result<String> {
    harvester_proto::fingerprint_reader(&mut std::fs::File::open(RUNNING_EXECUTABLE)?)
}

fn acquire_single_instance_lock(dir: &Path, sock_path: &Path) {
    let lock_path = dir.join(LOCK_FILE_NAME);
    let lock = match std::fs::File::create(&lock_path) {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("cannot open {}: {error}", lock_path.display());
            std::process::exit(1);
        }
    };

    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            eprintln!("harvesterd already running on {}", sock_path.display());
            std::process::exit(1);
        }
        Err(std::fs::TryLockError::Error(error)) => {
            eprintln!("cannot lock {}: {error}", lock_path.display());
            std::process::exit(1);
        }
    }

    std::mem::forget(lock);
}

fn remove_stale_socket(sock_path: &Path) {
    match std::fs::remove_file(sock_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            eprintln!("cannot unlink {}: {error}", sock_path.display());
            std::process::exit(1);
        }
    }
}

fn print_build_fingerprint() {
    match fingerprint_running_executable() {
        Ok(fingerprint) => println!("{fingerprint}"),
        Err(error) => {
            eprintln!("cannot fingerprint this binary: {error}");
            std::process::exit(1);
        }
    }
}

fn startup_build_fingerprint() -> String {
    fingerprint_running_executable().unwrap_or_else(|error| {
        eprintln!("cannot fingerprint {RUNNING_EXECUTABLE}: {error}");

        UNAVAILABLE_FINGERPRINT.to_string()
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    match std::env::args().nth(1) {
        None => {}
        Some(arg) if arg == "version" => {
            println!("{}", harvester_proto::PROTOCOL_VERSION);

            return;
        }
        Some(arg) if arg == "build" => {
            print_build_fingerprint();

            return;
        }
        Some(arg) => {
            eprintln!("unknown argument '{arg}' (usage: harvesterd [version|build])");
            std::process::exit(2);
        }
    }

    let sock_path = paths::socket();
    let dir = paths::harvester_dir();
    std::fs::create_dir_all(paths::log_dir()).expect("create ~/.harvester/log");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .expect("chmod 700 ~/.harvester");

    acquire_single_instance_lock(&dir, &sock_path);
    remove_stale_socket(&sock_path);

    let listener = UnixListener::bind(&sock_path).expect("bind unix socket");
    eprintln!(
        "harvesterd {} listening on {}",
        harvester_proto::PROTOCOL_VERSION,
        sock_path.display()
    );

    let daemon = daemon::Daemon::new(startup_build_fingerprint());
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let daemon = daemon.clone();
                tokio::spawn(async move { server::serve_connection(daemon, stream).await });
            }
            Err(error) => eprintln!("accept failed: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_running_executable_fingerprints() {
        let fingerprint = fingerprint_running_executable().expect("the test binary is readable");

        assert!(fingerprint.starts_with("fnv1a64-"), "{fingerprint}");
        assert_ne!(fingerprint, UNAVAILABLE_FINGERPRINT);
    }

    #[test]
    fn the_degraded_answer_is_never_a_real_fingerprint() {
        assert_ne!(
            UNAVAILABLE_FINGERPRINT,
            harvester_proto::fingerprint_bytes(b"")
        );
        assert!(!UNAVAILABLE_FINGERPRINT.starts_with("fnv1a64-"));
    }
}
