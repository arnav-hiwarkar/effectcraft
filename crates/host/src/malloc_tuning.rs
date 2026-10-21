//! Enables glibc huge-page malloc before startup by re-executing once.
//! Set `EFFECTCRAFT_NO_MALLOC_TUNING` to any non-empty value to opt out.

use std::ffi::OsStr;

const TUNABLES: &str = "GLIBC_TUNABLES";
const HUGETLB: &str = "glibc.malloc.hugetlb=1";
const OPT_OUT: &str = "EFFECTCRAFT_NO_MALLOC_TUNING";
const MARKER: &str = "EFFECTCRAFT_MALLOC_REEXEC";

/// The `GLIBC_TUNABLES` value to re-execute with, or `None` to run as is.
fn tunables(current: Option<&OsStr>, opt_out: Option<&OsStr>, marker: Option<&OsStr>) -> Option<String> {
    if marker.is_some() || opt_out.is_some_and(|v| !v.is_empty()) {
        return None;
    }
    match current {
        None => Some(HUGETLB.to_string()),
        Some(v) if v.is_empty() => Some(HUGETLB.to_string()),
        Some(v) => {
            let v = v.to_str()?;
            if v.split(':').any(|t| t.split('=').next() == Some("glibc.malloc.hugetlb")) {
                return None;
            }
            Some(format!("{v}:{HUGETLB}"))
        }
    }
}

/// Re-executes this program once with huge-page malloc (glibc Linux).
/// Returns only when no re-exec is needed (`Ok`) or it failed (`Err`); on success the process
/// image is replaced.
pub fn reexec() -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;

    let Some(value) = tunables(std::env::var_os(TUNABLES).as_deref(), std::env::var_os(OPT_OUT).as_deref(), std::env::var_os(MARKER).as_deref()) else {
        return Ok(());
    };
    let exe = std::env::current_exe()?;
    let mut args = std::env::args_os();
    let mut cmd = std::process::Command::new(exe);
    if let Some(argv0) = args.next() {
        cmd.arg0(argv0);
    }
    Err(cmd.args(args).env(TUNABLES, value).env(MARKER, "1").exec())
}

#[cfg(test)]
mod tests {
    use super::tunables;
    use std::ffi::OsStr;

    fn s(value: &str) -> Option<&OsStr> {
        Some(OsStr::new(value))
    }

    #[test]
    fn unset_reexecs() {
        assert_eq!(tunables(None, None, None), Some("glibc.malloc.hugetlb=1".into()));
        assert_eq!(tunables(s(""), None, None), Some("glibc.malloc.hugetlb=1".into()));
    }

    #[test]
    fn appends_to_other_tunables() {
        assert_eq!(tunables(s("glibc.malloc.arena_max=2"), None, None), Some("glibc.malloc.arena_max=2:glibc.malloc.hugetlb=1".into()));
    }

    #[test]
    fn already_set_keeps_user_value() {
        assert_eq!(tunables(s("glibc.malloc.hugetlb=0"), None, None), None);
        assert_eq!(tunables(s("glibc.malloc.arena_max=2:glibc.malloc.hugetlb=2"), None, None), None);
    }

    #[test]
    fn opt_out_skips() {
        assert_eq!(tunables(None, s("1"), None), None);
        assert_eq!(tunables(None, s(""), None), Some("glibc.malloc.hugetlb=1".into()));
    }

    #[test]
    fn reexeced_skips() {
        assert_eq!(tunables(None, None, s("1")), None);
    }
}
