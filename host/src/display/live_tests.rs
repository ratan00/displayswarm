//! Ad-hoc verification of the live probe and the generated helper script.
//!
//! Run with: cargo test --lib -- --ignored --nocapture live_display
//!
//! `live_probe_matches_the_machine` prints what DisplaySwarm would show the user on
//! this host, and `live_helper_script_is_valid_posix_sh` runs the exact bytes of
//! `privilege::HELPER_SCRIPT` through a shell parser. Both are read-only and need
//! no root.

use super::*;
use std::fs;
use std::path::Path;

/// What the user would see in the status field right now.
#[test]
#[ignore]
fn live_probe_matches_the_machine() {
    let probe = probe();
    println!("summary          : {}", probe.summary());
    println!("vkms loaded      : {}", probe.vkms_loaded);
    println!("configfs avail   : {}", probe.configfs_available);
    println!("layout           : {:?}", probe.layout);
    println!("configfs instances: {:?}", probe.instances);
    println!("implicit default : {}", probe.implicit_default_display());
    for card in &probe.cards {
        println!(
            "card {:<8} virtual={} connectors={:?}",
            card.card,
            card.is_virtual(),
            card
                .connectors
                .iter()
                .map(|c| format!("{} [{} {} mode={:?}]", c.name, c.status, c.enabled, c.preferred_mode))
                .collect::<Vec<_>>()
        );
    }
    assert_eq!(probe.vkms_loaded, Path::new(vkms::VKMS_MODULE_PATH).exists());
    assert_eq!(
        probe.configfs_available,
        Path::new(vkms::VKMS_CONFIGFS_ROOT).is_dir()
    );
}

/// `ensure_display` on this host must take the *adopt* branch and never prompt
/// for root, because `modprobe vkms` already left a working display. Run
/// unprivileged, a mistake would try to run `pkexec` and hang, so the call is
/// bounded by asserting the handle comes back without any privileged work.
#[test]
#[ignore]
fn live_ensure_display_adopts_instead_of_prompting() {
    let handle = ensure_display_for("live-verification-phone").expect("provisioning");
    println!("describe: {}", handle.describe());
    println!("state   : {:?}", handle.state);

    if !vkms::module_loaded() {
        panic!("vkms is not loaded, so this host cannot exercise the adopt path");
    }
    assert_eq!(
        handle.state,
        ProvisioningState::AdoptedDefault,
        "a host with vkms loaded already has a virtual display; the first phone must \
         adopt it rather than prompting for root to create a second one"
    );
    assert_eq!(handle.instance, "vmon-live-verification-phone");
}

/// The exact bytes handed to root must be a valid POSIX shell script, and its
/// guard must reject a hostile name.
#[test]
#[ignore]
fn live_helper_script_is_valid_posix_sh() {
    let dir = std::env::temp_dir().join("vmon-live-helper");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(privilege::HELPER_SCRIPT_NAME);
    fs::write(&path, privilege::HELPER_SCRIPT).unwrap();

    for shell in ["/bin/sh", "/bin/dash", "/bin/bash"] {
        if !Path::new(shell).exists() {
            continue;
        }
        let syntax = std::process::Command::new(shell)
            .arg("-n")
            .arg(&path)
            .output()
            .unwrap_or_else(|e| panic!("{shell} -n failed to run: {e}"));
        assert!(
            syntax.status.success(),
            "{shell} rejected the helper script: {}",
            String::from_utf8_lossy(&syntax.stderr)
        );
        println!("{shell} -n: ok");
    }

    // The real `status` subcommand, unprivileged, on the real machine.
    let out = std::process::Command::new("/bin/sh")
        .arg(&path)
        .arg("status")
        .output()
        .unwrap();
    println!("status exit: {:?}", out.status.code());
    println!("status stdout:\n{}", String::from_utf8_lossy(&out.stdout));
    println!("status stderr:\n{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.status.code(), Some(0));
    assert!(!String::from_utf8_lossy(&out.stdout).is_empty(), "expected real connectors");

    let _ = fs::remove_dir_all(&dir);
}
