//! Keep the authority lease through SSH commands and external Git hooks.
use super::{Result, cutover, durable, invalid, string};
use std::path::Path;

pub fn current_user() -> Result<String> {
    super::accounts::name(
        unsafe { libc::geteuid() },
        16384,
        "login account is absent",
        "invalid login account",
    )
}
pub fn hostname() -> Result<String> {
    let mut buffer = vec![0u8; 256];
    if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let end = buffer
        .iter()
        .position(|b| *b == 0)
        .ok_or_else(|| invalid("hostname exceeds its limit"))?;
    String::from_utf8(buffer[..end].to_vec()).map_err(|_| invalid("invalid hostname"))
}
pub fn serve_login_shell(path: &Path, argv: &[String], host: &str) -> Result<()> {
    let manifest = cutover::validate_manifest(&durable::read_config_json(path)?, host)?;
    let user = current_user()?;
    let selected: Vec<_> = manifest["resources"]
        .as_object()
        .ok_or_else(|| invalid("invalid cutover resources"))?
        .values()
        .filter(|e| {
            e["kind"] == "filesystem"
                && e["user"] == user
                && e["login_shell"].as_str().is_some_and(|s| !s.is_empty())
        })
        .collect();
    if selected.len() != 1 {
        return Err(invalid(
            "login user must select exactly one declared filesystem authority",
        ));
    }
    let config = selected[0];
    let mut command = vec![string(config, "login_shell")?.into()];
    command.extend_from_slice(argv);
    cutover::serve(config, &command)
}
