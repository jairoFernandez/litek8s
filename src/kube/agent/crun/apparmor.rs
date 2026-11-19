//! The AppArmor profile of `appArmorProfile: RuntimeDefault` (and so of
//! every pod of an untrusted namespace): containerd's `cri-containerd`
//! profile under our name. On top of the capability set, masked paths and
//! seccomp filter it confines signals and ptrace to processes of the same
//! profile, and refuses mounts and writes to /proc and /sys by path (so
//! also through a bind mount or a new procfs).

use std::path::Path;

use crate::kube::agent::runtime::APPARMOR_DEFAULT;

fn profile() -> String {
    format!(
        r#"#include <tunables/global>

profile {APPARMOR_DEFAULT} flags=(attach_disconnected,mediate_deleted) {{
  #include <abstractions/base>

  network,
  capability,
  file,
  umount,

  signal (receive) peer=unconfined,
  signal (receive) peer=crun,
  signal (send,receive) peer={APPARMOR_DEFAULT},

  deny @{{PROC}}/* w,
  deny @{{PROC}}/{{[^1-9],[^1-9][^0-9],[^1-9s][^0-9y][^0-9s],[^1-9][^0-9][^0-9][^0-9]*}}/** w,
  deny @{{PROC}}/sys/[^k]** w,
  deny @{{PROC}}/sys/kernel/{{?,??,[^s][^h][^m]**}} w,
  deny @{{PROC}}/sysrq-trigger rwklx,
  deny @{{PROC}}/mem rwklx,
  deny @{{PROC}}/kmem rwklx,
  deny @{{PROC}}/kcore rwklx,

  deny mount,

  deny /sys/[^f]*/** wklx,
  deny /sys/f[^s]*/** wklx,
  deny /sys/fs/[^c]*/** wklx,
  deny /sys/fs/c[^g]*/** wklx,
  deny /sys/fs/cg[^r]*/** wklx,
  deny /sys/firmware/** rwklx,
  deny /sys/devices/virtual/powercap/** rwklx,
  deny /sys/kernel/security/** rwklx,

  ptrace (trace,tracedby,read,readby) peer={APPARMOR_DEFAULT},
}}
"#
    )
}

/// Whether the kernel enforces AppArmor profiles.
pub(crate) fn enabled() -> bool {
    std::fs::read_to_string("/sys/module/apparmor/parameters/enabled").is_ok_and(|v| v.trim() == "Y")
}

/// Load (or replace) the default profile, from a file under `dir`. `false`:
/// this node has no AppArmor, or the profile cannot be loaded (no
/// `apparmor_parser`, a rootless sandbox); containers then run without it,
/// as on other Kubernetes nodes without AppArmor.
pub(crate) async fn load(dir: &Path) -> bool {
    if !enabled() {
        return false;
    }
    let file = dir.join("apparmor-default");
    if std::fs::write(&file, profile()).is_err() {
        return false;
    }
    match tokio::process::Command::new("apparmor_parser").arg("-Kr").arg(&file).output().await {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            eprintln!("node-agent: the AppArmor profile {APPARMOR_DEFAULT} was not loaded, containers run without it: {}", String::from_utf8_lossy(&out.stderr).trim());
            false
        }
        Err(e) => {
            eprintln!("node-agent: the AppArmor profile {APPARMOR_DEFAULT} was not loaded, containers run without it: apparmor_parser: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_profile_is_named_and_confines_signals_to_itself() {
        let p = profile();
        assert!(p.contains("profile litek8s-default flags=(attach_disconnected,mediate_deleted) {"), "{p}");
        assert!(p.contains("signal (send,receive) peer=litek8s-default,") && p.contains("deny mount,"));
        assert!(p.contains("deny @{PROC}/sys/kernel/{?,??,[^s][^h][^m]**} w,"), "{p}");
        assert!(!p.contains("userns"));
    }
}
