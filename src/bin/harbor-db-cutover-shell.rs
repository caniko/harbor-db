use harbor_db::storage::{Result, login_shell};
fn run() -> Result<()> {
    login_shell::serve_login_shell(
        std::path::Path::new("/etc/harbor-db/cutover.json"),
        &std::env::args().skip(1).collect::<Vec<_>>(),
        &login_shell::hostname()?,
    )
}
fn main() {
    if let Err(e) = run() {
        eprintln!("harbor-db-cutover-shell: {e}");
        std::process::exit(1);
    }
}
