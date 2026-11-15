//! Development-only local SSH/SFTP fixture; never exposes the real user's filesystem.
#[path = "../tests/support/sftp_server.rs"]
mod support;
use std::path::PathBuf;
fn main() {
    let parent = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .expect("usage: sftp_fixture <test-output-directory>");
    let server = support::Fixture::new_in(&parent, support::Options::default());
    let state = serde_json::json!({"host":"127.0.0.1","port":server.port,"username":support::USER,"password":support::PASSWORD,"host_key":server.key,"root":server.root.path()});
    std::fs::write(
        parent.join("fixture.json"),
        serde_json::to_vec_pretty(&state).unwrap(),
    )
    .unwrap();
    println!("{}", state);
    loop {
        std::thread::park();
    }
}
