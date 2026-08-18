use std::path::PathBuf;

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME must be set"))
}

pub fn socket() -> PathBuf {
    home().join(harvester_proto::DAEMON_SOCKET_PATH)
}

pub fn harvester_dir() -> PathBuf {
    socket()
        .parent()
        .expect("socket path has a parent")
        .to_path_buf()
}

pub fn log_dir() -> PathBuf {
    harvester_dir().join("log")
}

pub fn pi_sessions_root() -> PathBuf {
    home().join(".pi/agent/sessions")
}
