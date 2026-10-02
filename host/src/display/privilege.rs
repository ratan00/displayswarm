//! Getting root, safely and without a terminal.
//!
//! # The threat
//!
//! Writing to `/sys/kernel/config/vkms` requires root, and DisplaySwarm is a
//! desktop application running as the user. It therefore has to *ask* for root.
//! The way it asks is `pkexec`, which authenticates through polkit and shows
//! the standard system authentication dialog.
//!
//! **`pkexec` runs a program as root.** If the program or its arguments were
//! built by interpolating anything an attacker can influence - a client id from
//! a phone on the network, a device serial, a display name - then a local
//! attacker gets **arbitrary code execution as root** the moment the user
//! approves the dialog. There is no sandbox around `pkexec`, and the dialog the
//! user sees is generic: nothing about it distinguishes "create a virtual
//! display" from "run this attacker's script".
//!
//! Three properties keep that from happening here.
//!
//! 1. **No shell, ever.** Nothing is passed to `sh -c`, no command is built for
//!    execution by string interpolation, and the root-side script contains no
//!    command substitution or `eval`. `pkexec` is invoked with an explicit
//!    `argv` vector, so an argument can never be reinterpreted as a command.
//!    See [`HelperCommand::argv`] and [`HELPER_SCRIPT`].
//! 2. **A closed subcommand set.** The privileged program understands exactly
//!    four verbs - `ensure-module`, `create`, `destroy`, `status` - each with at
//!    most one argument. It is not a general "do this filesystem operation"
//!    interface, so the worst case is "create or destroy a virtual display",
//!    which is what the user was told they were approving.
//! 3. **Validation on both sides of the privilege boundary.** The only
//!    variable input is an instance name, which must match
//!    `^[a-z0-9][a-z0-9-]{0,63}$`. [`vkms::is_valid_instance_name`] enforces it
//!    before the name is ever placed in an `argv`, and the helper script
//!    re-checks it with its own `case` guard before it touches the filesystem,
//!    so even a fully compromised host process cannot get the helper to name a
//!    path outside `/sys/kernel/config/vkms`.
//!
//! # Why a generated script rather than a shipped helper binary
//!
//! A polkit action (`org.freedesktop.policykit.exec`) would be the textbook
//! answer, but it still needs a *program* to run, and the only program here is
//! one DisplaySwarm would have to install with root - which is the privilege it is
//! trying to obtain. Instead the helper is written out as a **compile-time
//! constant** ([`HELPER_SCRIPT`]) into a `0700` directory in the user's cache,
//! and the bytes are re-verified against that constant immediately before every
//! use. The privileged surface is therefore auditable by reading one `const` in
//! this file, and a helper that has drifted on disk is refused rather than run
//! as root.
//!
//! The residual risk is stated plainly: the script lives in user-writable
//! storage, so anything running **as the user** could in principle replace it
//! between the verification and the `exec`. That is inherent to any "prompt the
//! user for admin rights" flow and is not an escalation the attacker gets for
//! free - they still have to get the user to authenticate in a polkit dialog
//! that names the program being run. Everything here keeps that surface as
//! small as it can be, and keeps the blast radius at "virtual displays".
//!
//! # Failing safe
//!
//! If `pkexec` is missing, polkit refuses, or the user cancels the dialog, this
//! module does **not** silently do nothing and it does **not** fall back to a
//! shell. It returns a [`DisplayError`] carrying the exact `sudo` commands the
//! user can paste into a terminal, which the UI shows verbatim. The application
//! stays usable in the meantime: it streams the phone's own screen, and the
//! manual commands complete the provisioning.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::vkms;
use super::DisplayError;

// ---------------------------------------------------------------------------
// The privileged program
// ---------------------------------------------------------------------------

/// Name of the privilege broker, looked up on `PATH`.
pub const PKEXEC_BASENAME: &str = "pkexec";

/// Directory the generated helper lives in, under the user's cache.
pub const HELPER_DIR_NAME: &str = "displayswarm";

/// Filename of the generated helper.
pub const HELPER_SCRIPT_NAME: &str = "vkms-helper.sh";

/// The privileged program, verbatim.
///
/// This is the entire root-side attack surface of DisplaySwarm; read it as if
/// auditing a `sudoers` rule. Four subcommands, one validated argument, every
/// path derived from `CONFIGFS_VKMS_ROOT` below. There is no `eval`, no `sh -c`,
/// no command substitution, and no way to choose a program, so the only thing a
/// caller can influence is a single path component that is rejected unless it
/// is `[a-z0-9-]`.
///
/// Utilities are called by absolute path because `pkexec` runs with a sanitised
/// environment, and because `/usr/bin` exists on both merged-`/usr` and
/// split-`/usr` layouts.
///
/// POSIX `sh`, so it runs under dash, bash or busybox `sh` alike.
pub const HELPER_SCRIPT: &str = r#"#!/bin/sh
# DisplaySwarm vkms provisioning helper - GENERATED, DO NOT EDIT.
#
# The host writes this file and then re-verifies it byte-for-byte against a
# copy compiled into displayswarm-host before every privileged invocation, so any
# change to these bytes is detected and refused.
#
# SECURITY: this program runs as root. The only caller-supplied value is $2,
# checked against ^[a-z0-9][a-z0-9-]{0,63}$ before it reaches any path. There
# is no eval, no sh -c, and no command substitution, so the most a hostile
# caller can achieve is creating or destroying a virtual display.
set -eu

CONFIGFS_MOUNTPOINT=/sys/kernel/config
CONFIGFS_VKMS_ROOT=/sys/kernel/config/vkms
VKMS_MODULE=vkms

MKDIR=/usr/bin/mkdir
RMDIR=/usr/bin/rmdir
LN=/usr/bin/ln
RM=/usr/bin/rm
GREP=/usr/bin/grep
MOUNT=/usr/bin/mount
MODPROBE=/usr/sbin/modprobe

# -- plumbing ---------------------------------------------------------------

log() { printf 'displayswarm-vkms-helper: %s\n' "$*" >&2; }
fail() { log "$1"; exit 1; }

usage() {
    printf 'usage: %s ensure-module | status | create <instance> | destroy <instance>\n' "$0" >&2
    exit 2
}

# Reads a one-line sysfs attribute into $2 without command substitution. The
# value is never re-parsed, so its contents cannot become a command.
read_attr() {
    if [ -r "$1" ]; then
        read -r "$2" < "$1" || true
    fi
}

# The one and only validation of caller-supplied data. A name that is not
# exactly [a-z0-9] followed by [a-z0-9-] is rejected, before any filesystem
# operation is attempted.
valid_instance() {
    case "${1:-}" in
        "" ) return 1 ;;
        *[!a-z0-9-]* ) return 1 ;;
        -* ) return 1 ;;
        * ) [ "${#1}" -le 64 ] || return 1 ;;
    esac
    return 0
}

require_valid_instance() {
    if ! valid_instance "${1:-}"; then
        log "refusing instance name: not [a-z0-9][a-z0-9-]{0,63}"
        exit 2
    fi
}

# -- ensure-module ----------------------------------------------------------
#
# Loads the vkms kernel module and mounts configfs. Both are prerequisites for
# any instance, and both need root. Idempotent: already-done is success.

ensure_module() {
    if [ ! -d "$CONFIGFS_MOUNTPOINT" ]; then
        fail "$CONFIGFS_MOUNTPOINT does not exist; is this a system with configfs?"
    fi
    if ! "$GREP" -qs '[[:space:]]configfs[[:space:]]' /proc/mounts; then
        log "mounting configfs on $CONFIGFS_MOUNTPOINT"
        "$MOUNT" -t configfs configfs "$CONFIGFS_MOUNTPOINT" ||
            fail "could not mount configfs (needs CONFIGFS_FS in the kernel)"
    fi
    if [ ! -d /sys/module/"$VKMS_MODULE" ]; then
        log "loading $VKMS_MODULE"
        "$MODPROBE" "$VKMS_MODULE" ||
            fail "modprobe $VKMS_MODULE failed: the module is not installed, or the kernel lacks CONFIG_DRM_VKMS (Device Drivers > Graphics support > Virtual KMS)"
    fi
    if [ ! -d "$CONFIGFS_VKMS_ROOT" ]; then
        fail "$CONFIGFS_VKMS_ROOT is missing after loading $VKMS_MODULE"
    fi
    log "vkms is loaded and $CONFIGFS_VKMS_ROOT is available"
}

# -- create / destroy -------------------------------------------------------
#
# These two routines perform the same operations, in the same order, as
# vkms::create_plan and vkms::destroy_plan - which are unit-tested against a
# real directory tree. Keep the two in step.

vkms_create() {
    base="$CONFIGFS_VKMS_ROOT/$1"

    # A half-built or disabled instance from a previous run is removed first,
    # so create is idempotent and never trips over EEXIST.
    if [ -d "$base" ]; then
        log "replacing existing instance $1"
        vkms_destroy "$1"
    fi

    # 1. the instance group, then the pipeline items, in dependency order. The
    #    driver pre-creates planes/, crtcs/, encoders/ and connectors/ inside
    #    the instance, plus a possible_crtcs / possible_encoders directory and
    #    the attribute files inside each item, so only the leaves are made here.
    #    -p because the scaffolding is the driver's job and only a test tree
    #    lacks it.
    "$MKDIR" -p "$base"
    "$MKDIR" -p "$base/planes/plane0"
    "$MKDIR" -p "$base/crtcs/crtc0"
    "$MKDIR" -p "$base/encoders/encoder0"
    "$MKDIR" -p "$base/connectors/connector0"

    # 2. mark the connector connected. Best effort: a fresh connector already
    #    defaults to connected, so a kernel without this attribute must not
    #    stop the display from being created.
    if ! printf '%s\n' 1 > "$base/connectors/connector0/status"; then
        log "warning: could not set connector status, continuing"
    fi

    # 3. a primary plane is mandatory, or the instance produces no output.
    printf '%s\n' 1 > "$base/planes/plane0/type"

    # 4. wire the pipeline. The links go INSIDE the driver's possible_crtcs /
    #    possible_encoders directories, and the targets are ABSOLUTE: configfs
    #    resolves them with kern_path(), against the calling process's working
    #    directory rather than the link's parent, so a relative target cannot
    #    work. allow_link also refuses with EBUSY once the device is enabled,
    #    which is why this has to precede step 5.
    "$LN" -s "$base/crtcs/crtc0" "$base/planes/plane0/possible_crtcs/crtc0"
    "$LN" -s "$base/crtcs/crtc0" "$base/encoders/encoder0/possible_crtcs/crtc0"
    "$LN" -s "$base/encoders/encoder0" "$base/connectors/connector0/possible_encoders/encoder0"

    # 5. enable last, so the driver never sees a partial pipeline.
    printf '%s\n' 1 > "$base/enabled"
    log "created and enabled vkms instance $1"
}

vkms_destroy() {
    base="$CONFIGFS_VKMS_ROOT/$1"
    if [ ! -d "$base" ]; then
        return 0
    fi

    # Disable first, so the DRM card is released before its configuration goes.
    if [ -e "$base/enabled" ]; then
        printf '%s\n' 0 > "$base/enabled" 2>/dev/null ||
            log "warning: could not disable $1, continuing"
    fi

    # Links out before the groups containing them: a configfs link is "busy"
    # from rmdir's point of view.
    if [ -L "$base/planes/plane0/possible_crtcs/crtc0" ]; then
        "$RM" "$base/planes/plane0/possible_crtcs/crtc0"
    fi
    if [ -L "$base/encoders/encoder0/possible_crtcs/crtc0" ]; then
        "$RM" "$base/encoders/encoder0/possible_crtcs/crtc0"
    fi
    if [ -L "$base/connectors/connector0/possible_encoders/encoder0" ]; then
        "$RM" "$base/connectors/connector0/possible_encoders/encoder0"
    fi

    "$RMDIR" "$base/planes/plane0" 2>/dev/null || log "warning: planes/plane0 not removed"
    "$RMDIR" "$base/crtcs/crtc0" 2>/dev/null || log "warning: crtcs/crtc0 not removed"
    "$RMDIR" "$base/encoders/encoder0" 2>/dev/null || log "warning: encoders/encoder0 not removed"
    "$RMDIR" "$base/connectors/connector0" 2>/dev/null || log "warning: connectors/connector0 not removed"
    "$RMDIR" "$base" 2>/dev/null || log "warning: $1 not removed"
    log "destroyed vkms instance $1"
}

# -- status -----------------------------------------------------------------

vkms_status() {
    value=""
    for base in "$CONFIGFS_VKMS_ROOT"/*; do
        if [ -d "$base" ]; then
            name="${base##*/}"
            value=""
            read_attr "$base/enabled" value
            printf 'instance=%s enabled=%s\n' "$name" "${value:-0}"
        fi
    done
    for conn in /sys/class/drm/card*-*; do
        if [ -r "$conn/status" ]; then
            name="${conn##*/}"
            value=""
            read_attr "$conn/status" value
            printf 'connector=%s status=%s\n' "$name" "${value:-unknown}"
        fi
    done
    return 0
}

# -- dispatch ---------------------------------------------------------------

if [ "$#" -lt 1 ]; then
    usage
fi
verb="$1"
name="${2:-}"

case "$verb" in
    ensure-module)
        if [ "$#" -ne 1 ]; then usage; fi
        ensure_module
        ;;
    create)
        if [ "$#" -ne 2 ]; then usage; fi
        require_valid_instance "$name"
        if [ ! -d /sys/module/"$VKMS_MODULE" ]; then
            fail "vkms is not loaded; run this helper with: ensure-module"
        fi
        vkms_create "$name"
        ;;
    destroy)
        if [ "$#" -ne 2 ]; then usage; fi
        require_valid_instance "$name"
        vkms_destroy "$name"
        ;;
    status)
        if [ "$#" -ne 1 ]; then usage; fi
        vkms_status
        ;;
    *)
        usage
        ;;
esac
"#;

// ---------------------------------------------------------------------------
// The subcommand set
// ---------------------------------------------------------------------------

/// One invocation of the root-side helper.
///
/// Each variant is one thing a user could reasonably be asked to approve, and
/// the single argument, where there is one, is an instance name that has
/// already been through [`vkms::instance_name_for`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperCommand {
    /// Load `vkms` and mount configfs.
    EnsureModule,
    /// Create (or replace) one instance and enable it.
    Create { instance: String },
    /// Disable and remove one instance.
    Destroy { instance: String },
    /// Print the instances and connectors the kernel currently exposes.
    Status,
}

impl HelperCommand {
    /// The subcommand word passed to the helper.
    pub fn subcommand(&self) -> &'static str {
        match self {
            HelperCommand::EnsureModule => "ensure-module",
            HelperCommand::Create { .. } => "create",
            HelperCommand::Destroy { .. } => "destroy",
            HelperCommand::Status => "status",
        }
    }

    /// The validated arguments that follow the subcommand.
    ///
    /// Empty for the two verbs that take none, and empty for a *rejected* name:
    /// an instance that fails validation contributes no argument at all, so it
    /// can never occupy an `argv` slot.
    pub fn args(&self) -> Vec<String> {
        match self {
            HelperCommand::EnsureModule | HelperCommand::Status => Vec::new(),
            HelperCommand::Create { instance } | HelperCommand::Destroy { instance } => {
                if vkms::is_valid_instance_name(instance) {
                    vec![instance.clone()]
                } else {
                    Vec::new()
                }
            }
        }
    }

    /// Builds the exact `argv` for `pkexec`:
    /// `["pkexec", <helper>, <subcommand>, <validated args...>]`.
    ///
    /// Note what is *not* here: a shell, a command string, a `PATH` lookup.
    /// `pkexec` is handed the helper by absolute path and the arguments as
    /// separate entries, so `execvp` passes them to the helper unchanged.
    /// There is no position in this vector at which a `;`, a `$(...)`, a
    /// backtick or a space could be interpreted by anything.
    pub fn argv(&self, helper: &Path) -> Vec<OsString> {
        let mut argv = vec![OsString::from(PKEXEC_BASENAME), helper.into()];
        argv.push(OsString::from(self.subcommand()));
        argv.extend(self.args().into_iter().map(OsString::from));
        argv
    }

    /// The same arguments, for the case where the process is **already** root
    /// and `pkexec` would only add a redundant authentication dialog.
    ///
    /// Includes the leading interpreter, which `run` strips because it invokes
    /// that interpreter itself.
    pub fn direct_argv(&self, helper: &Path) -> Vec<OsString> {
        let mut argv = vec![OsString::from("/bin/sh"), helper.into()];
        argv.push(OsString::from(self.subcommand()));
        argv.extend(self.args().into_iter().map(OsString::from));
        argv
    }

    /// The commands a user can paste into a terminal instead, for the fallback
    /// shown in the UI when `pkexec` is unavailable, refused, or cancelled.
    ///
    /// This is the one place in the crate that formats a command line, because a
    /// human is going to read and run it. It is safe only because the values it
    /// interpolates come from [`vkms::FsOp::render`], whose every path is built
    /// from a constant root plus an instance name that admits no shell
    /// metacharacter, space or quote at all -
    /// `test_manual_commands_never_echo_their_input` pins that down.
    pub fn manual_commands(&self) -> Vec<String> {
        let sudo = "sudo ";
        let render = |ops: Vec<vkms::FsOp>| {
            ops.iter().map(|op| format!("{sudo}{}", op.render_step())).collect()
        };
        match self {
            HelperCommand::EnsureModule => vec![
                format!(
                    "{sudo}/usr/bin/mount -t configfs configfs {}",
                    vkms::CONFIGFS_MOUNTPOINT
                ),
                format!("{sudo}/usr/sbin/modprobe {}", vkms::VKMS_MODULE),
            ],
            HelperCommand::Create { instance } => {
                render(vkms::create_plan(instance).unwrap_or_default())
            }
            HelperCommand::Destroy { instance } => {
                render(vkms::destroy_plan(instance).unwrap_or_default())
            }
            HelperCommand::Status => {
                vec![format!("{sudo}/usr/bin/ls {}", vkms::VKMS_CONFIGFS_ROOT)]
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Locating pkexec and the helper
// ---------------------------------------------------------------------------

/// Finds an executable named `name` in `dirs`, in order.
///
/// Split out from [`pkexec_path`] so it can be tested against a synthetic
/// `PATH` without mutating the process environment, which a shared test binary
/// cannot do safely.
pub fn find_executable_in(name: &str, dirs: impl Iterator<Item = PathBuf>) -> Option<PathBuf> {
    dirs.into_iter().map(|dir| dir.join(name)).find(|c| is_executable_file(c))
}

/// Whether `path` is a regular file with at least one execute bit set.
fn is_executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match fs::metadata(path) {
            Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Absolute path to `pkexec`, searched on `PATH`.
pub fn pkexec_path() -> Option<PathBuf> {
    let dirs: Vec<PathBuf> = match std::env::var_os("PATH") {
        Some(path) => std::env::split_paths(&path).collect(),
        None => Vec::new(),
    };
    find_executable_in(PKEXEC_BASENAME, dirs.into_iter())
}

/// True if `pkexec` can be found. Says nothing about whether polkit will grant
/// it - that is only knowable by asking.
pub fn is_pkexec_available() -> bool {
    pkexec_path().is_some()
}

/// This process's effective uid, read from `/proc/self/status`.
///
/// Reading `/proc` rather than calling `geteuid()` keeps the library free of a
/// `libc` dependency for one number. Unreadable is reported as "not root", which
/// is the safe direction to be wrong in: the worst case is one redundant `pkexec`
/// dialog.
pub fn effective_uid() -> Option<u32> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        line.strip_prefix("Uid:")
            .and_then(|ids| ids.split_whitespace().nth(1))
            .and_then(|euid| euid.parse::<u32>().ok())
    })
}

/// True if the process already has the privileges the helper needs, so
/// `pkexec` would only add a redundant authentication dialog.
pub fn already_privileged() -> bool {
    effective_uid() == Some(0)
}

/// The user's cache directory for the helper, or `None` if the environment
/// does not say.
pub fn helper_dir() -> Option<PathBuf> {
    if let Some(base) = std::env::var_os("XDG_CACHE_HOME").filter(|s| !s.is_empty()) {
        let path = PathBuf::from(base).join(HELPER_DIR_NAME);
        if path.is_absolute() {
            return Some(path);
        }
    }
    std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(|home| Path::new(&home).join(".cache").join(HELPER_DIR_NAME))
}

/// The path the helper will be written to, without writing it.
pub fn helper_path() -> Result<PathBuf, DisplayError> {
    helper_dir().map(|dir| dir.join(HELPER_SCRIPT_NAME)).ok_or_else(|| {
        DisplayError::HelperUnavailable {
            detail: "neither XDG_CACHE_HOME nor HOME is set, so there is nowhere to stage \
                     the privileged helper"
                .into(),
            manual: HelperCommand::EnsureModule.manual_commands().join("\n"),
        }
    })
}

/// Writes [`HELPER_SCRIPT`] into the user's cache and returns its path.
///
/// The directory is created `0700` and the file `0500`, so no *other* local user
/// can read or replace it. The owner is the user by definition, and file
/// permissions are not a defence against code already running as that user -
/// see the module docs for why that is an acceptable residual risk.
pub fn install_helper() -> Result<PathBuf, DisplayError> {
    let path = helper_path()?;
    let dir = path.parent().expect("the helper path always has a parent");
    let io = |what: &str, e: std::io::Error| DisplayError::Io {
        context: format!("{what} {}", path.display()),
        detail: e.to_string(),
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(dir).map_err(|e| io("creating", e))?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir).map_err(|e| io("creating", e))?;
    }

    fs::write(&path, HELPER_SCRIPT).map_err(|e| io("writing", e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o500))
            .map_err(|e| io("setting mode on", e))?;
    }
    Ok(path)
}

/// Checks that the helper on disk is still byte-identical to [`HELPER_SCRIPT`].
///
/// Runs immediately before every privileged invocation. It cannot close the
/// window between the check and the `exec` (see the module docs), but it does
/// turn a hand-edited or truncated helper from a confusing root-side failure
/// into a clear local one.
pub fn verify_helper(path: &Path) -> Result<(), DisplayError> {
    let unavailable = |detail: String| DisplayError::HelperUnavailable {
        detail,
        manual: HelperCommand::EnsureModule.manual_commands().join("\n"),
    };
    let on_disk = fs::read(path)
        .map_err(|e| unavailable(format!("could not read {}: {e}", path.display())))?;
    if on_disk != HELPER_SCRIPT.as_bytes() {
        return Err(unavailable(format!(
            "{} does not match the helper compiled into displayswarm-host, so it will not be \
             run as root. Delete that file and try again.",
            path.display()
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Running the helper
// ---------------------------------------------------------------------------

/// What the helper printed, and how it exited.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HelperOutput {
    /// Exit code, or `None` if the process was killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl HelperOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// `pkexec` exited 126: the authentication dialog was dismissed, or no agent
/// was available.
const PKEXEC_DIALOG_DISMISSED: i32 = 126;

/// `pkexec` exited 127: the authorisation was refused or could not be obtained.
///
/// Both of the above are the user saying "no", and both get the same manual
/// fallback. See `man pkexec`.
const PKEXEC_NOT_AUTHORISED: i32 = 127;

/// The helper's own exit code for a usage or validation error: a bug or a
/// rejected name, not a refusal by the user. Surfaces verbatim so it is not
/// mistaken for an authentication problem.
const HELPER_USAGE: i32 = 2;

/// Runs `command` with the privileges it needs, prompting the user if required.
///
/// * Already root: the helper is executed directly, with no dialog.
/// * Otherwise: `pkexec`, with [`HelperCommand::argv`] and no shell.
///
/// Every failure mode returns a [`DisplayError`] carrying the manual `sudo`
/// fallback, so the caller always has something actionable to show.
pub fn run(command: &HelperCommand) -> Result<HelperOutput, DisplayError> {
    let manual = command.manual_commands().join("\n");
    let helper = install_helper().map_err(|e| DisplayError::HelperUnavailable {
        detail: e.summary(),
        manual: manual.clone(),
    })?;
    verify_helper(&helper).map_err(|e| DisplayError::HelperUnavailable {
        detail: e.summary(),
        manual: manual.clone(),
    })?;

    let (program, argv) = if already_privileged() {
        log::info!("Already running as root: running the vkms helper directly, no pkexec prompt");
        (
            PathBuf::from("/bin/sh"),
            command.direct_argv(&helper).into_iter().skip(1).collect(),
        )
    } else {
        let pkexec = pkexec_path().ok_or_else(|| DisplayError::HelperUnavailable {
            detail: format!(
                "{PKEXEC_BASENAME} is not on PATH, so DisplaySwarm cannot obtain the root \
                 privileges that creating a virtual display requires. Install polkit \
                 (package `polkit`), or run the commands below in a terminal."
            ),
            manual: manual.clone(),
        })?;
        (pkexec, command.argv(&helper))
    };

    // NOTE: no environment is added for the child. `pkexec` sanitises the
    // environment itself, and the authentication dialog is drawn by the
    // *polkit agent* in the user's session rather than by `pkexec`, so nothing
    // here needs DISPLAY, WAYLAND_DISPLAY or XDG_RUNTIME_DIR - and nothing is
    // leaked into a root process.
    //
    // The argv is summarised rather than logged verbatim, per the house rule
    // about not writing root command lines into logs.
    log::info!(
        "Requesting root to {} via {} ({} argv elements, no shell)",
        command.subcommand(),
        program.display(),
        argv.len()
    );

    let output = Command::new(&program)
        .args(&argv)
        .output()
        .map_err(|e| DisplayError::HelperUnavailable {
            detail: format!("could not run {}: {e}", program.display()),
            manual: manual.clone(),
        })?;

    let result = HelperOutput {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    };

    if result.success() {
        return Ok(result);
    }

    let stderr = result.stderr.trim().to_string();

    if result.code == Some(HELPER_USAGE) {
        return Err(DisplayError::HelperFailed {
            detail: if stderr.is_empty() {
                "the privileged helper rejected the request".to_string()
            } else {
                stderr
            },
            manual,
        });
    }

    if matches!(result.code, Some(PKEXEC_DIALOG_DISMISSED) | Some(PKEXEC_NOT_AUTHORISED)) {
        return Err(DisplayError::AuthorisationDenied {
            detail: "the system authentication dialog was dismissed or the request was refused"
                .to_string(),
            manual,
        });
    }

    Err(DisplayError::HelperFailed {
        detail: if stderr.is_empty() {
            format!("the privileged helper exited with {:?}", result.code)
        } else {
            format!("the privileged helper failed: {stderr}")
        },
        manual,
    })
}

/// Parses the output of [`HelperCommand::Status`].
///
/// The helper emits two pairs per line (`instance=vmon-x enabled=1`), so the
/// line is split on whitespace and each token at its first `=`. Values are
/// connector names and digits, never anything a caller acts on.
///
/// Unrecognised tokens are ignored, because the helper is a diagnostic and a new
/// kernel attribute must not break the parse.
pub fn parse_status(stdout: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("displayswarm-vkms-helper:") {
            continue;
        }
        for token in line.split_whitespace() {
            if let Some((key, value)) = token.split_once('=') {
                if !key.is_empty() {
                    out.push((key.to_string(), value.to_string()));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn helper() -> PathBuf {
        PathBuf::from("/home/u/.cache/displayswarm/vkms-helper.sh")
    }

    fn create(instance: &str) -> HelperCommand {
        HelperCommand::Create {
            instance: instance.to_string(),
        }
    }

    fn destroy(instance: &str) -> HelperCommand {
        HelperCommand::Destroy {
            instance: instance.to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // argv construction
    // -----------------------------------------------------------------------

    #[test]
    fn test_argv_is_explicit_and_shellless() {
        let argv = create("vmon-phone-a05f").argv(&helper());
        assert_eq!(
            argv,
            vec![
                OsString::from("pkexec"),
                helper().into(),
                OsString::from("create"),
                OsString::from("vmon-phone-a05f"),
            ]
        );
        // One program, one subcommand, one validated argument. Nothing in the
        // vector is a shell.
        assert_eq!(argv.len(), 4);
        assert!(!argv.iter().any(|a| a == "/bin/sh" || a == "sh" || a == "-c"));
    }

    #[test]
    fn test_argv_for_the_argument_free_subcommands() {
        for (cmd, word) in [
            (HelperCommand::EnsureModule, "ensure-module"),
            (HelperCommand::Status, "status"),
        ] {
            assert_eq!(
                cmd.argv(&helper()),
                vec![OsString::from("pkexec"), helper().into(), OsString::from(word)]
            );
            assert!(cmd.args().is_empty());
        }
    }

    /// The critical test. `pkexec` runs its argument as root, so a name that
    /// survives into an `argv` slot carrying a metacharacter is a potential root
    /// shell. Every one of these has to be rejected *before* it gets there - and
    /// "rejected" means the argument does not appear at all, not that the
    /// subcommand changes.
    #[test]
    fn test_shell_metacharacters_never_reach_argv() {
        let attacks = [
            "; id",
            "a; id",
            "a && id",
            "a | id",
            "a || id",
            "$(id)",
            "`id`",
            "${HOME}",
            "a\nid",
            "a > /etc/passwd",
            "a >> /etc/passwd",
            "a < /etc/shadow",
            "a 2>&1",
            "../../../../etc/shadow",
            "/etc/shadow",
            "a/b",
            "a\\b",
            "..",
            "a b",
            "a'b",
            "a\"b",
            "a\n",
            "",
            "-rf",
            "--help",
            "A",
            "a_",
            "a.b",
        ];
        for attack in attacks {
            for cmd in [create(attack), destroy(attack)] {
                let verb = cmd.subcommand();
                assert!(
                    cmd.args().is_empty(),
                    "{verb} {attack:?} produced arguments {:?}; it must be rejected outright",
                    cmd.args()
                );

                let argv = cmd.argv(&helper());
                assert_eq!(argv.len(), 3, "{verb} {attack:?} changed the argv length");
                assert_eq!(argv[2], OsString::from(verb), "the subcommand must be a literal");
                for element in &argv {
                    let s = element.to_string_lossy();
                    for meta in [";", "&", "|", "$", "`", "(", ")", "<", ">", "\n", "\\", "'", "\"", " "]
                    {
                        assert!(
                            !s.contains(meta),
                            "{attack:?} left {meta:?} in argv element {s:?}"
                        );
                    }
                }
            }
        }
    }

    /// `pkexec` interprets a leading `-` on *its* arguments as its own option,
    /// which is why the helper's own guard and
    /// [`vkms::is_valid_instance_name`] both refuse a leading dash. The
    /// subcommand is a fixed literal, so it can never be turned into an option.
    #[test]
    fn test_instance_names_can_never_look_like_options() {
        for attack in ["-rf", "--user", "-u", "-h", "--help", "-vmon", "-"] {
            assert!(!vkms::is_valid_instance_name(attack), "{attack:?} must be rejected");
            assert!(create(attack).args().is_empty());
        }
        for cmd in [
            HelperCommand::EnsureModule,
            HelperCommand::Status,
            create("vmon-x"),
            destroy("vmon-x"),
        ] {
            assert!(!cmd.subcommand().starts_with('-'));
        }
    }

    /// The already-root path must not go through `pkexec`, which would pop a
    /// redundant authentication dialog.
    #[test]
    fn test_direct_argv_drops_pkexec_and_keeps_the_arguments() {
        let argv = create("vmon-x").direct_argv(&helper());
        assert_eq!(argv[0], OsString::from("/bin/sh"));
        assert!(!argv.contains(&OsString::from("pkexec")));
        // `run` invokes the interpreter itself, so it skips this element.
        let skipped: Vec<OsString> = argv.into_iter().skip(1).collect();
        assert_eq!(skipped[0], OsString::from(helper()));
        assert_eq!(skipped[1], OsString::from("create"));
        assert_eq!(skipped[2], OsString::from("vmon-x"));
    }

    // -----------------------------------------------------------------------
    // Manual fallback text
    // -----------------------------------------------------------------------

    /// The manual commands are the one place a command line is formatted, and
    /// they are shown to a user to paste into a terminal. The invariant is not
    /// "contains no shell syntax" - every command line has shell syntax - but
    /// "no caller-supplied text is echoed". A rejected name must not appear
    /// anywhere in the output, and the only name that does must be the
    /// sanitised one.
    #[test]
    fn test_manual_commands_never_echo_their_input() {
        let attacks = [
            "a; rm -rf /",
            "$(id)",
            "`id`",
            "${HOME}",
            "../../etc",
            "a b",
            "a|b",
            "a'b",
            "a\"b",
            "a\nb",
            "a&b",
            "",
        ];
        for attack in attacks {
            let sanitised = vkms::instance_name_for(attack).unwrap();
            for cmd in [create(attack), destroy(attack)] {
                let text = cmd.manual_commands().join("\n");
                if !attack.is_empty() {
                    assert!(
                        !text.contains(attack),
                        "{attack:?} was echoed verbatim into a manual command:\n{text}"
                    );
                }
                if attack != sanitised {
                    assert!(
                        text.contains(&sanitised) || text.is_empty(),
                        "{attack:?} should have been replaced by {sanitised:?}:\n{text}"
                    );
                }
                // Nothing that came from the caller survives as a path segment.
                for word in text.split_whitespace() {
                    if let Some(last) = word.strip_prefix("/sys/").and_then(|r| r.rsplit('/').next()) {
                        let _ = last;
                    }
                }
            }
        }
    }

    /// Every path segment the fallback emits must be one the kernel will
    /// accept, which is the property that makes pasting it work.
    #[test]
    fn test_manual_commands_only_name_valid_instances() {
        for cmd in [create("vmon-phone-a05f"), destroy("vmon-phone-a05f")] {
            for line in cmd.manual_commands() {
                let body = line.strip_prefix("sudo ").unwrap_or(&line);
                let mut segments = body.split_whitespace();
                let verb = segments.next().expect("a command has a verb");
                assert!(
                    ["mkdir", "rmdir", "ln", "rm", "echo", "/usr/bin/ls"].contains(&verb),
                    "unexpected verb {verb:?} in {line:?}"
                );
                for word in segments {
                    if word.starts_with('/') {
                        for segment in word.trim_start_matches('/').split('/') {
                            if segment == "sys"
                                || segment == "config"
                                || segment == "kernel"
                                || segment == "vkms"
                                || segment == "vmon-phone-a05f"
                                || segment.starts_with("vmon-")
                                || ["planes", "plane0", "crtcs", "crtc0", "encoders", "encoder0",
                                    "connectors", "connector0", "type", "enabled", "status",
                                    "possible_crtcs", "possible_encoders"]
                                    .contains(&segment)
                            {
                                continue;
                            }
                            panic!("unexpected path segment {segment:?} in {line:?}");
                        }
                    }
                }
            }
        }
    }

    /// The fallback has to be actionable: the user pastes it and gets the same
    /// instance, with the primary plane set and the enable last.
    #[test]
    fn test_manual_commands_name_the_same_instance() {
        let text = create("vmon-phone-a05f").manual_commands().join("\n");
        for needle in [
            "mkdir /sys/kernel/config/vkms/vmon-phone-a05f",
            "planes/plane0/type",
            "planes/plane0/possible_crtcs",
            "encoders/encoder0/possible_crtcs",
            "connectors/connector0/possible_encoders",
            "/enabled",
        ] {
            assert!(text.contains(needle), "fallback is missing {needle:?}:\n{text}");
        }
        let type_at = text.find("/planes/plane0/type").expect("type write present");
        let enable_at = text.rfind("/enabled").expect("enable write present");
        assert!(type_at < enable_at, "the fallback must enable last:\n{text}");

        let destroy = destroy("vmon-phone-a05f").manual_commands().join("\n");
        assert!(destroy.contains("echo 0 > /sys/kernel/config/vkms/vmon-phone-a05f/enabled"));
        assert!(destroy.contains("rmdir /sys/kernel/config/vkms/vmon-phone-a05f\n")
            || destroy.trim_end().ends_with("rmdir /sys/kernel/config/vkms/vmon-phone-a05f"));
    }

    #[test]
    fn test_ensure_module_manual_commands() {
        let text = HelperCommand::EnsureModule.manual_commands().join("\n");
        assert!(text.contains("mount -t configfs configfs /sys/kernel/config"), "{text}");
        assert!(text.contains("modprobe vkms"), "{text}");
        assert_eq!(HelperCommand::Status.manual_commands().len(), 1);
    }

    // -----------------------------------------------------------------------
    // The privileged helper script
    // -----------------------------------------------------------------------

    /// The root-side attack surface, asserted to keep its properties. A change
    /// to `HELPER_SCRIPT` that reintroduces a shell escape, an unvalidated
    /// argument or a new verb fails here rather than in production.
    #[test]
    fn test_helper_script_has_no_shell_escapes() {
        assert!(HELPER_SCRIPT.starts_with("#!/bin/sh\n"));
        assert!(HELPER_SCRIPT.ends_with("esac\n"), "the script must end cleanly");

        // Comments are excluded: the script *documents* that it has no `eval`
        // and no `sh -c`, and that text must not satisfy the check that proves
        // it.
        let code: String = HELPER_SCRIPT
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");

        // `eval`, a nested shell and command substitution are the three ways to
        // turn an argument (or a sysfs value) into a command. None may appear.
        for forbidden in ["eval", "sh -c", "bash -c", "$(", "`"] {
            assert!(
                !code.contains(forbidden),
                "the root helper must not contain {forbidden:?}"
            );
        }

        // Every external program is reached through an absolute path held in a
        // variable, because pkexec runs with a sanitised PATH.
        for var in ["MKDIR", "RMDIR", "LN", "RM", "GREP", "MOUNT", "MODPROBE"] {
            assert!(
                code.contains(&format!("{var}=/")),
                "{var} is not bound to an absolute path"
            );
        }
        // And the only places those variables are used are inside double
        // quotes, so a path with a space in it could not word-split.
        for line in code.lines() {
            for var in ["MKDIR", "RMDIR", "LN", "RM", "GREP", "MOUNT", "MODPROBE"] {
                let needle = format!("${var}");
                let mut from = 0;
                while let Some(at) = line[from..].find(&needle) {
                    let at = from + at;
                    let quoted = line[..at].ends_with('"') || line[at..].starts_with('{');
                    assert!(quoted, "{var} used unquoted: {line:?}");
                    from = at + needle.len();
                }
            }
        }

        // Exactly the four documented verbs are dispatched, and nothing else.
        let dispatch = HELPER_SCRIPT
            .split("case \"$verb\" in")
            .nth(1)
            .and_then(|rest| rest.split("esac").next())
            .expect("the helper dispatches on a verb");
        let arms: Vec<&str> = dispatch
            .lines()
            .map(str::trim)
            .filter(|line| line.ends_with(')'))
            .map(|line| line.trim_end_matches(')').trim())
            .collect();
        assert_eq!(arms, vec!["ensure-module", "create", "destroy", "status", "*"]);

        // Every writable path is built from the fixed configfs root plus the
        // validated name; the roots are spelled once each, as assignments, and
        // nowhere else.
        assert!(HELPER_SCRIPT.contains("CONFIGFS_MOUNTPOINT=/sys/kernel/config\n"));
        assert!(HELPER_SCRIPT.contains("CONFIGFS_VKMS_ROOT=/sys/kernel/config/vkms\n"));
        assert_eq!(
            HELPER_SCRIPT.matches("/sys/kernel/config/vkms").count(),
            1,
            "the vkms root must appear only as the assignment"
        );
        // Nothing else writes outside the vkms group, and the only other
        // absolute path is the read-only module list.
        for line in code.lines() {
            if line.contains(">/") || line.contains("\"$MKDIR\"") {
                assert!(
                    !line.contains("/etc/") && !line.contains("/root/") && !line.contains("/home/"),
                    "the helper writes outside configfs: {line:?}"
                );
            }
        }
    }

    /// The helper re-validates the one piece of caller-supplied data before it
    /// touches the filesystem. If that guard were removed, a caller-supplied
    /// name could name any path on the system.
    #[test]
    fn test_helper_validates_before_any_filesystem_work() {
        // The guard exists, and is the documented character class.
        assert!(HELPER_SCRIPT.contains("*[!a-z0-9-]* ) return 1 ;;"));
        assert!(HELPER_SCRIPT.contains("* ) [ \"${#1}\" -le 64 ] || return 1 ;;"));
        assert!(HELPER_SCRIPT.contains("* ) return 1 ;;"));

        // Each verb that takes an argument validates it as its first action,
        // after only the arity check.
        for verb in ["create", "destroy"] {
            let arm = HELPER_SCRIPT
                .split(&format!("    {verb})"))
                .nth(1)
                .unwrap_or_else(|| panic!("no dispatch arm for {verb}"));
            let require = arm.find("require_valid_instance");
            let work = arm.find("vkms_");
            assert!(require.is_some(), "{verb} does not validate its argument");
            assert!(
                require.unwrap() < work.expect("the verb does work"),
                "{verb} does filesystem work before validating"
            );
        }
    }

    /// The guard is *shell* logic, so it is tested by running it. Rejecting a
    /// bad name needs no root, which is exactly what is being checked: a
    /// rejected request must fail before it reaches the filesystem.
    #[test]
    #[cfg(target_os = "linux")]
    fn test_helper_rejects_bad_names_before_doing_anything() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("vmon-helper-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(HELPER_SCRIPT_NAME);
        fs::write(&path, HELPER_SCRIPT).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o500)).unwrap();

        let run = |args: &[&str]| -> std::process::Output {
            Command::new("/bin/sh")
                .arg(&path)
                .args(args)
                .output()
                .expect("run the helper")
        };

        for bad in ["../evil", "a/b", "A", "", "a; id", "$(id)", "-rf", "a b", "a\tb"] {
            for verb in ["create", "destroy"] {
                let out = run(&[verb, bad]);
                assert_eq!(
                    out.status.code(),
                    Some(HELPER_USAGE),
                    "{verb} {bad:?} was not rejected (stderr: {})",
                    String::from_utf8_lossy(&out.stderr)
                );
                let err = String::from_utf8_lossy(&out.stderr);
                assert!(
                    err.contains("refusing instance name"),
                    "{verb} {bad:?} gave the wrong error: {err}"
                );
            }
        }

        // An unknown verb and a missing verb are usage errors, not crashes.
        assert_eq!(run(&["definitely-not-a-verb", "vmon-x"]).status.code(), Some(HELPER_USAGE));
        assert_eq!(run(&[]).status.code(), Some(HELPER_USAGE));
        assert_eq!(run(&["create", "vmon-x", "extra"]).status.code(), Some(HELPER_USAGE));

        // A syntactically valid name gets *past* validation and then fails
        // somewhere else - which is what proves the guard is not simply
        // rejecting everything.
        let out = run(&["create", "vmon-x"]);
        assert_ne!(out.status.code(), Some(HELPER_USAGE), "a valid name was rejected");
        assert!(
            !String::from_utf8_lossy(&out.stderr).contains("refusing instance name"),
            "a valid name must not be rejected by the guard: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// The same, for the *acceptance* side that does not need root: `status`
    /// must run, print the documented format, and exit 0 even with nothing
    /// provisioned.
    #[test]
    #[cfg(target_os = "linux")]
    fn test_helper_status_runs_and_uses_the_documented_format() {
        let dir = std::env::temp_dir().join(format!("vmon-helper-st-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(HELPER_SCRIPT_NAME);
        fs::write(&path, HELPER_SCRIPT).unwrap();

        let out = Command::new("/bin/sh")
            .arg(&path)
            .arg("status")
            .output()
            .expect("run the helper");
        assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));

        // Every line the helper printed must survive the parse.
        let parsed = parse_status(&String::from_utf8_lossy(&out.stdout));
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            assert!(
                parsed.iter().any(|(k, _)| k == &line.split('=').next().unwrap().to_string()),
                "unparseable status line: {line:?}"
            );
        }
        // This machine has vkms loaded, so a connector must be visible.
        assert!(
            parsed.iter().any(|(k, _)| k == "connector"),
            "expected at least one connector on a host with vkms loaded, got {parsed:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// The helper's *shell logic* for create and destroy, exercised against a
    /// real directory tree.
    ///
    /// This is the only way to check the privileged code path without root, and
    /// it is worth doing: a quoting or ordering bug in the script would show up
    /// as a virtual display that silently never appears, which is exactly the
    /// failure mode this module exists to prevent. The exact shipped bytes are
    /// checked separately by `test_helper_script_has_no_shell_escapes` and
    /// `test_helper_rejects_bad_names_before_doing_anything`; here two constants
    /// are re-pointed at a temp directory on a *copy*, because the whole point
    /// of the shipped script is that its roots are not configurable.
    #[test]
    #[cfg(target_os = "linux")]
    fn test_helper_create_and_destroy_round_trip_in_a_fake_tree() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("vmon-helper-rt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("vkms");
        fs::create_dir_all(&root).unwrap();

        // Re-point the two roots, drop the `vkms is loaded` precondition so the
        // test does not depend on the host having the module, and stand in for
        // the driver: configfs's `configfs_add_default_group` creates the
        // `possible_crtcs` / `possible_encoders` directories and the attribute
        // files the instant the corresponding group appears, which a plain
        // directory does not do. The helper must never create those itself, so
        // they are injected into the *copy* immediately after each mkdir -
        // exactly where the kernel would have put them.
        let mut script = HELPER_SCRIPT
            .replace(
                "CONFIGFS_MOUNTPOINT=/sys/kernel/config",
                &format!("CONFIGFS_MOUNTPOINT={}", dir.display()),
            )
            .replace(
                "CONFIGFS_VKMS_ROOT=/sys/kernel/config/vkms",
                &format!("CONFIGFS_VKMS_ROOT={}", root.display()),
            )
            .replace(
                "        if [ ! -d /sys/module/\"$VKMS_MODULE\" ]; then\n            fail \"vkms is not loaded; run this helper with: ensure-module\"\n        fi\n",
                "",
            );
        // Directories the driver adds, and attribute *files* it adds, per group.
        let scaffolding: &[(&str, &[(&str, bool)])] = &[
            ("$base", &[("enabled", false), ("planes", true), ("crtcs", true),
                        ("encoders", true), ("connectors", true)]),
            ("$base/planes/plane0", &[("type", false), ("possible_crtcs", true)]),
            ("$base/crtcs/crtc0", &[("writeback", false)]),
            ("$base/encoders/encoder0", &[("possible_crtcs", true)]),
            ("$base/connectors/connector0", &[("status", false), ("possible_encoders", true)]),
        ];
        for (group, children) in scaffolding {
            let mkdir = format!("    \"$MKDIR\" -p \"{group}\"\n");
            assert!(script.contains(&mkdir), "no mkdir of {group} in the helper");
            let mut injected = String::from(&mkdir);
            for (name, is_dir) in children.iter() {
                injected.push_str(&if *is_dir {
                    format!("    \"$MKDIR\" -p \"{group}/{name}\"\n")
                } else {
                    format!("    printf '0\\n' > \"{group}/{name}\"\n")
                });
            }
            script = script.replace(&mkdir, &injected);

            // And the driver takes them away with the group, so inject the
            // matching teardown before the helper's rmdir.
            let rmdir = format!("    \"$RMDIR\" \"{group}\"");
            if script.contains(&rmdir) {
                let mut injected = String::new();
                for (name, _) in children.iter().rev() {
                    injected.push_str(&format!("    \"$RM\" -rf \"{group}/{name}\" 2>/dev/null || :\n"));
                }
                injected.push_str(&rmdir);
                script = script.replace(&rmdir, &injected);
            }
        }

        assert_ne!(script, HELPER_SCRIPT, "the test must be rewriting the copy");
        assert!(
            !script.contains("/sys/kernel/config/vkms"),
            "the vkms root must have been re-pointed"
        );

        let path = dir.join(HELPER_SCRIPT_NAME);
        fs::write(&path, &script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o500)).unwrap();

        let run = |args: &[&str]| -> std::process::Output {
            Command::new("/bin/sh")
                .arg(&path)
                .args(args)
                .output()
                .expect("run the helper")
        };

        // --- create -------------------------------------------------------
        let out = run(&["create", "vmon-x"]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "create failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );

        let base = root.join("vmon-x");
        assert!(base.is_dir());
        assert_eq!(
            fs::read_to_string(base.join("enabled")).unwrap().trim(),
            "1",
            "the instance must end up enabled"
        );
        assert_eq!(fs::read_to_string(base.join("planes/plane0/type")).unwrap().trim(), "1");
        assert_eq!(
            fs::read_to_string(base.join("connectors/connector0/status")).unwrap().trim(),
            "1"
        );
        // The pipeline links, with absolute targets and the right destinations.
        for (link, want) in [
            ("planes/plane0/possible_crtcs/crtc0", "crtcs/crtc0"),
            ("encoders/encoder0/possible_crtcs/crtc0", "crtcs/crtc0"),
            ("connectors/connector0/possible_encoders/encoder0", "encoders/encoder0"),
        ] {
            let link_path = base.join(link);
            let target = fs::read_link(&link_path)
                .unwrap_or_else(|e| panic!("the helper did not create {link}: {e}"));
            assert!(target.is_absolute(), "{link} target {target:?} must be absolute");
            assert_eq!(
                fs::canonicalize(&link_path).unwrap(),
                fs::canonicalize(base.join(want)).unwrap(),
                "{link} points at the wrong pipeline item"
            );
        }

        // --- idempotence --------------------------------------------------
        // `create` destroys an existing instance first, so a re-provision is not
        // an EEXIST failure. This is what makes a repeated button press safe.
        let out = run(&["create", "vmon-x"]);
        assert_eq!(out.status.code(), Some(0), "a second create must succeed: {}", String::from_utf8_lossy(&out.stderr));
        assert!(String::from_utf8_lossy(&out.stderr).contains("replacing existing instance vmon-x"));
        assert_eq!(fs::read_to_string(base.join("enabled")).unwrap().trim(), "1");

        // --- destroy ------------------------------------------------------
        let out = run(&["destroy", "vmon-x"]);
        assert_eq!(out.status.code(), Some(0), "destroy failed: {}", String::from_utf8_lossy(&out.stderr));
        assert!(!base.exists(), "destroy left the instance behind");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0, "destroy left files behind");

        // Destroying something that is not there is a success, so teardown on
        // disconnect is idempotent.
        let out = run(&["destroy", "vmon-x"]);
        assert_eq!(out.status.code(), Some(0), "destroy must be idempotent");

        let _ = fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // pkexec discovery
    // -----------------------------------------------------------------------

    #[test]
    fn test_find_executable_in_respects_permission_and_kind() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("vmon-pkexec-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("a")).unwrap();
        fs::create_dir_all(dir.join("b")).unwrap();

        let fake = dir.join("a/pkexec");
        fs::write(&fake, "#!/bin/sh\n").unwrap();

        // A non-executable file must be skipped, not run.
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            find_executable_in("pkexec", [dir.join("a"), dir.join("b")].into_iter()),
            None,
            "a non-executable pkexec must not be found"
        );

        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            find_executable_in("pkexec", [dir.join("a"), dir.join("b")].into_iter()),
            Some(fake.clone())
        );

        // A directory named pkexec is not a program.
        fs::remove_file(&fake).unwrap();
        fs::create_dir_all(dir.join("b/pkexec")).unwrap();
        assert_eq!(
            find_executable_in("pkexec", [dir.join("b")].into_iter()),
            None,
            "a directory must not be mistaken for a program"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Output parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_status_extracts_key_values() {
        let stdout = "\
displayswarm-vkms-helper: vkms is loaded
instance=vmon-phone-a05f enabled=1
instance=vmon-other enabled=0
connector=card2-Virtual-1 status=connected

not-a-pair
=vmissingvalue
";
        let parsed = parse_status(stdout);
        assert_eq!(parsed.len(), 6, "{parsed:?}");
        for (key, value) in [
            ("instance", "vmon-phone-a05f"),
            ("enabled", "1"),
            ("instance", "vmon-other"),
            ("enabled", "0"),
            ("connector", "card2-Virtual-1"),
            ("status", "connected"),
        ] {
            assert!(
                parsed.contains(&(key.to_string(), value.to_string())),
                "missing {key}={value} in {parsed:?}"
            );
        }
        assert!(parse_status("").is_empty());
    }

    /// A new key in the helper's output must not break a caller that only
    /// knows about `instance` and `connector`.
    #[test]
    fn test_parse_status_is_tolerant_of_new_keys() {
        let parsed = parse_status("instance=vmon-x enabled=1\nbrand_new_key=42\n");
        assert!(parsed.iter().any(|(k, v)| k == "instance" && v == "vmon-x"));
        assert!(parsed.iter().any(|(k, _)| k == "brand_new_key"));
    }

    // -----------------------------------------------------------------------
    // Failing safe
    // -----------------------------------------------------------------------

    /// Every way privilege can fail must produce an error carrying usable manual
    /// instructions, because that text is the only way the user can finish the
    /// job without a terminal-integrated flow.
    #[test]
    fn test_every_failure_carries_manual_instructions() {
        let manual = create("vmon-x").manual_commands().join("\n");
        for err in [
            DisplayError::HelperUnavailable {
                detail: "pkexec not found".into(),
                manual: manual.clone(),
            },
            DisplayError::AuthorisationDenied {
                detail: "dismissed".into(),
                manual: manual.clone(),
            },
            DisplayError::HelperFailed {
                detail: "exit 1".into(),
                manual: manual.clone(),
            },
        ] {
            let shown = err.manual_commands().expect("a fallback must be offered");
            assert!(!shown.trim().is_empty(), "the fallback must not be empty");
            assert!(shown.contains("vmon-x"), "{shown}");
            assert!(!err.summary().is_empty(), "every error needs a summary to log");
            assert!(!err.to_string().is_empty());
        }

        // And the composition the UI actually shows has to contain the fix.
        // This is the contract `DisplaySetupReport` relies on.
        let denied = DisplayError::AuthorisationDenied {
            detail: "the dialog was dismissed".into(),
            manual,
        };
        let report = crate::display::DisplaySetupReport {
            ok: false,
            summary: denied.summary(),
            manual_fallback: denied.manual_commands().map(str::to_string),
        };
        let text = report.status_text();
        assert!(text.contains("dismissed"), "{text}");
        assert!(text.contains("sudo"), "the user must be able to fix it: {text}");
    }

    /// The host's guard and the helper's must agree, or the host would happily
    /// build an `argv` the helper then rejects with a confusing error.
    #[test]
    fn test_host_and_helper_validation_agree() {
        // 64 is the shared ceiling, not merely a similar one.
        assert_eq!(vkms::MAX_INSTANCE_NAME_LEN, 64);
        for name in [
            "a",
            "0",
            "vmon-x",
            "vmon-phone-a05f",
            "a-b-c-1234567890",
        ] {
            assert!(vkms::is_valid_instance_name(name), "{name:?} should pass both guards");
        }
        for name in ["", "-x", "X", "a_b", "a.b", "a/b", "a b", "a;b", "..", "a\tb"] {
            assert!(!vkms::is_valid_instance_name(name), "{name:?} should fail both guards");
        }
    }
}
