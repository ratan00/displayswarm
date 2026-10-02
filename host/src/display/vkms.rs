//! `vkms` instance lifecycle: naming, configfs operation plans, and probing.
//!
//! # Why this module exists at all
//!
//! There is **no user-space way to create a display**. A display is a KMS
//! pipeline (plane -> CRTC -> encoder -> connector) owned by a *kernel* DRM
//! driver, and DRM exposes no "create connector" uAPI. So DisplaySwarm cannot
//! conjure a display out of nothing: it has to **configure the `vkms` kernel
//! module**, which is a software-only KMS driver that creates a purely virtual
//! display with no hardware behind it.
//!
//! `vkms` supports several independent instances, each with its own connector,
//! and it is configured through **configfs**. That is the mechanism that makes
//! "one phone -> one display, N phones -> N displays" possible, because every
//! instance gets its own DRM card and its own connector, so the compositor
//! (KWin, Mutter, ...) sees N separate screens.
//!
//! # Layering
//!
//! Everything in this file is *pure* with respect to privilege: [`create_plan`]
//! and [`destroy_plan`] only build an ordered [`FsOp`] list, and [`apply_plan`]
//! hands that list to a [`FsBackend`]. Nothing here needs root *by itself* -
//! acquiring root is the job of [`crate::display::privilege`], which wraps this
//! module's plans in a `pkexec` invocation. That split is deliberate: it makes
//! the whole op sequence unit-testable without root, and it keeps the set of
//! filesystem operations that ever runs as root small and auditable.
//!
//! # Where the pipeline links go, and why their targets are absolute
//!
//! This is the part that is easy to get wrong, so it is spelled out against the
//! kernel source that decides it.
//!
//! On every kernel since 6.19 (this machine runs 7.2.7), `possible_crtcs` and
//! `possible_encoders` are **directories, not symlink attributes**.
//! `make_plane_group()` in `drivers/gpu/drm/vkms/vkms_configfs.c` does:
//!
//! ```c
//! config_group_init_type_name(&plane->possible_crtcs_group, "possible_crtcs", ...);
//! configfs_add_default_group(&plane->possible_crtcs_group, &plane->group);
//! ```
//!
//! so creating `planes/plane0` yields an **empty** `planes/plane0/possible_crtcs/`
//! directory, and the pipeline is wired by `ln -s` *inside* it. Pre-6.19 kernels
//! used a symlink *attribute* and a different procedure entirely; that layout is
//! detected at probe time and reported as [`LayoutError::TooOld`] rather than
//! attempted.
//!
//! The target string is then resolved by `configfs_symlink()` in
//! `fs/configfs/symlink.c`:
//!
//! ```c
//! static int get_target(const char *symname, ...)
//! {
//!         ret = kern_path(symname, LOOKUP_FOLLOW|LOOKUP_DIRECTORY, &path);
//!         if (path.dentry->d_sb != sb)
//!                 return -EPERM;
//! ```
//!
//! `kern_path()` is a plain VFS lookup of the string the caller passed. It is
//! **not** resolved relative to the link's parent directory, so a relative target
//! like `../crtcs/crtc0` is looked up relative to the *calling process's working
//! directory* and fails. **The target must be an absolute path**, and it must
//! resolve to something on the configfs superblock or the kernel returns
//! `-EPERM`. That is why the kernel documentation uses
//! `/config/vkms/my-vkms/crtcs/crtc0` and is right to.
//!
//! A related trap: the path a configfs link *reports* is not the string that was
//! passed in. `readlink` returns a kernfs-relative path synthesised by
//! `configfs_get_target_path()`, so a verification step must compare *resolved*
//! paths, never link text.
//!
//! Two more constraints from the same source, both of which the plan respects:
//!
//!  * `plane_possible_crtcs_allow_link()` returns `-EBUSY` if the device is
//!    already enabled, so every link has to exist **before** `enabled=1`.
//!  * A configfs link is "busy" from `rmdir`'s point of view, so teardown has to
//!    `rm` the links before it can `rmdir` the item groups.
//!
//! # The implicit default instance
//!
//! `modprobe vkms` alone is enough to get a working display: `vkms_drv.c` has a
//! `create_default_dev = true` module parameter and registers an immutable
//! built-in device that appears as e.g. `/dev/dri/card2` with connector
//! `card2-Virtual-1`. KWin adopts it with no further configuration. Note that
//! this device has **no** entry in `/sys/kernel/config/vkms` - it is not
//! configfs-managed, and the driver actively refuses to let an instance take its
//! name: `make_device_group()` returns `-EINVAL` for `default`.
//!
//! That is the trap this module has to handle: a phone connecting to a host
//! that has merely run `modprobe vkms` already has one virtual display, and
//! blindly creating a `vmon-*` instance on top of that yields **two** screens
//! for one phone. [`DisplayProbe::implicit_default_display`] is how that is
//! detected, and [`crate::display::ensure_display`] adopts the existing screen
//! instead of creating a duplicate. See that function for the full policy.

use std::fs;
use std::path::Path;

use super::DisplayError;

// ---------------------------------------------------------------------------
// Fixed kernel interface locations
// ---------------------------------------------------------------------------

/// Where configfs is mounted by every mainstream distribution.
pub const CONFIGFS_MOUNTPOINT: &str = "/sys/kernel/config";

/// Name of the `vkms` kernel module, as passed to `modprobe`.
pub const VKMS_MODULE: &str = "vkms";

/// `sysfs` directory the kernel creates for a loaded module.
pub const VKMS_MODULE_PATH: &str = "/sys/module/vkms";

/// Root of the `vkms` configfs group. Present only when `vkms` is loaded *and*
/// configfs is mounted.
pub const VKMS_CONFIGFS_ROOT: &str = "/sys/kernel/config/vkms";

/// DRM `sysfs` class directory: one `cardN` per card, `cardN-<name>-<n>` per
/// connector, `renderD*` per render node.
pub const DRM_CLASS_PATH: &str = "/sys/class/drm";

/// Every instance DisplaySwarm creates is named with this prefix. It is the
/// authoritative answer to "is this display ours?" - see [`is_our_instance`] -
/// because a `vkms` connector is named `Virtual-<n>` regardless of which
/// instance created it, so the connector name cannot be used instead.
///
/// The prefix also makes a collision with the driver's own device impossible:
/// `make_device_group()` rejects the name `default` outright, and `vmon-default`
/// is a different string anyway.
pub const INSTANCE_PREFIX: &str = "vmon-";

/// Longest sanitised suffix, excluding [`INSTANCE_PREFIX`]. Chosen so a full
/// instance name (prefix + suffix + optional hash disambiguator) stays well
/// inside the 64-character ceiling the helper script enforces.
pub const MAX_INSTANCE_SUFFIX: usize = 32;

/// Hard ceiling on an instance name, mirrored by the helper script's
/// `case` guard. First character must be `[a-z0-9]`, the rest `[a-z0-9-]`.
pub const MAX_INSTANCE_NAME_LEN: usize = 64;

/// `DRM_PLANE_TYPE_PRIMARY`. A vkms instance with no primary plane produces
/// nothing, so this is mandatory rather than optional.
pub const PLANE_TYPE_PRIMARY: u32 = 1;

/// `connector_status_connected`, as accepted by the connector's `status`
/// attribute. Without it a connector reports `unknown` and compositors treat
/// the output as absent.
pub const CONNECTOR_STATUS_CONNECTED: u32 = 1;

/// Name of the plane group created inside an instance. Any name is legal; this
/// one is fixed so the plan always produces the same tree.
pub const PLANE_ITEM_NAME: &str = "plane0";

/// Name of the CRTC group.
pub const CRTC_ITEM_NAME: &str = "crtc0";

/// Name of the encoder group.
pub const ENCODER_ITEM_NAME: &str = "encoder0";

/// Name of the connector group.
pub const CONNECTOR_ITEM_NAME: &str = "connector0";

/// The driver's own `DEFAULT_DEVICE_NAME`, checked at probe time so a kernel
/// older than 6.19 can be reported rather than mis-provisioned. It is also the
/// name `make_device_group()` refuses for a configfs instance, which is why the
/// `vmon-` prefix rules out a collision with it.
pub const DRIVER_DEFAULT_DEVICE_NAME: &str = "default";

// ---------------------------------------------------------------------------
// Instance naming
// ---------------------------------------------------------------------------

/// FNV-1a, 64 bit.
///
/// Used only to disambiguate names that sanitisation had to alter, so the
/// mapping `client id -> instance name` stays injective enough for the
/// multi-phone case the module exists to support. Not a security primitive:
/// the result only ever feeds a `vmon-` prefixed name, and the instance name
/// is re-validated against a strict allowlist afterwards.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// True if `name` is safe to use as a single configfs path component.
///
/// The allowlist is deliberately absolute: lowercase ASCII alphanumerics and
/// `-` only. That makes `.` and `/` unrepresentable, which means path
/// traversal (`../..`), absolute paths, `.`/`..` and multi-component names are
/// all impossible *by construction* rather than by a filter that could be
/// bypassed. A NUL byte cannot survive either - it is not in the class - which
/// matters because a `&str` is allowed to contain one and `std::fs` paths are
/// not the only consumer (the value is also passed to `pkexec` as `argv`).
///
/// The `..` and `/` checks below are therefore redundant. They are kept
/// anyway: this function is the last gate before the value reaches a
/// privileged process, so an assert-style check on the two properties that
/// actually matter is worth the two lines.
pub fn is_valid_instance_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_INSTANCE_NAME_LEN {
        return false;
    }
    if !name.contains("..") && !name.contains('/') && !name.contains('\0') {
        let mut chars = name.bytes();
        let first = chars.next().expect("non-empty");
        let first_ok = first.is_ascii_lowercase() || first.is_ascii_digit();
        let rest_ok = chars.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        first_ok && rest_ok
    } else {
        false
    }
}

/// True if `name` is a DisplaySwarm-managed instance (as opposed to one a user
/// created by hand, or the module's implicit default device which has no
/// configfs entry at all).
pub fn is_our_instance(name: &str) -> bool {
    name.starts_with(INSTANCE_PREFIX)
}

/// Sanitises a logical display name into a configfs-safe instance suffix.
///
/// Everything outside `[a-z0-9-]` becomes `-`, runs of `-` collapse, leading
/// and trailing `-` are trimmed, and the result is truncated. Non-ASCII is
/// *not* transliterated (that would need a table and a normalisation policy);
/// it is simply replaced, and the caller adds a hash disambiguator so that
/// e.g. `"Pi Zero 2"` and `"Pi-Zero-2"` do not collide.
fn sanitise_suffix(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(MAX_INSTANCE_SUFFIX));
    let mut last_was_dash = true; // also suppresses a leading dash
    for byte in raw.bytes() {
        if out.len() >= MAX_INSTANCE_SUFFIX {
            break;
        }
        let keep = byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || (byte == b'-' && !last_was_dash);
        if !keep {
            if out.is_empty() || last_was_dash {
                continue;
            }
            out.push('-');
            last_was_dash = true;
            continue;
        }
        out.push(byte as char);
        last_was_dash = byte == b'-';
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Maps a client identity to a stable, sanitised configfs instance name.
///
/// The mapping is **pure and deterministic**: the same client id always yields
/// the same instance name, in this process and the next, with no state. That
/// is what makes re-connecting a phone idempotent - the second connection
/// finds the instance the first one created instead of making a second
/// display - and what makes N phones map onto N distinct instances.
///
/// | `client_id`          | instance name                  |
/// |---------------------|--------------------------------|
/// | `phone-a05f`         | `vmon-phone-a05f`              |
/// | `tcp-192.168.1.5`    | `vmon-tcp-192-168-1-5-1d3e0a91` |
/// | `usb-0a05f-0642`     | `vmon-usb-0a05f-0642`          |
/// | `Pi Zero 2`          | `vmon-pi-zero-2-<hash>`        |
/// | `""` / `"///"`       | `vmon-h<hash>`                 |
///
/// A trailing `-<hash>` (7 hex digits) is appended only when sanitisation had
/// to alter the input - non-ASCII, `/`, `.`, uppercase, truncation - because
/// only then can two distinct ids collapse onto the same sanitised form. An
/// id that is already a legal instance name comes through untouched, which is
/// why the common cases stay readable.
///
/// # Which client id to use
///
/// `server.rs` should pass the most stable per-device identifier it has:
/// the AOA device serial for a USB session, and the peer address for a TCP
/// session (see [`crate::display::tcp_client_id`]). A USB phone re-enumerates
/// and gets a new address, so its serial must be preferred, or a reconnect
/// would provision a second instance.
pub fn instance_name_for(client_id: &str) -> Result<String, DisplayError> {
    let lowered = client_id.to_ascii_lowercase();
    let suffix = sanitise_suffix(&lowered);

    let name = if suffix.is_empty() {
        // Nothing survived sanitisation (empty, or made entirely of rejected
        // characters). Fall back to a digest of the *original* id so the
        // result is still deterministic and still distinguishes different ids.
        format!("{}h{:016x}", INSTANCE_PREFIX, fnv1a64(client_id.as_bytes()))
    } else {
        let altered = suffix != lowered
            || lowered.len() > MAX_INSTANCE_SUFFIX
            || lowered.starts_with('-')
            || lowered.ends_with('-');
        if altered {
            // Disambiguate: "Pi Zero 2" and "pi-zero-2" sanitise identically.
            let hash = fnv1a64(client_id.as_bytes()) & 0x0fff_ffff;
            format!("{}{suffix}-{:07x}", INSTANCE_PREFIX, hash)
        } else {
            format!("{INSTANCE_PREFIX}{suffix}")
        }
    };

    if !is_valid_instance_name(&name) {
        // Unreachable given the construction above, but this value is handed to
        // a privileged process, so it is checked rather than assumed.
        return Err(DisplayError::InvalidClientId {
            reason: format!(
                "client id {client_id:?} sanitises to {name:?}, which is not a legal \
                 configfs instance name"
            ),
        });
    }
    Ok(name)
}

// ---------------------------------------------------------------------------
// Filesystem operation plans
// ---------------------------------------------------------------------------

/// One filesystem operation against configfs.
///
/// Deliberately a closed enum rather than a string command: there is no way
/// for a caller to smuggle an arbitrary program, path or argument into the
/// plan, which is what makes the plan safe to run as root and trivial to
/// assert on in a test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsOp {
    /// `mkdir <path>` - configfs group creation, i.e. "create this plane".
    Mkdir { path: String },
    /// Write a literal value to an attribute file, i.e. `echo 1 > <path>`.
    Write {
        path: String,
        value: String,
        /// When true a failure is logged and the plan continues. Only used
        /// for attributes whose absence must not abort provisioning.
        best_effort: bool,
    },
    /// `ln -s <target> <link>` inside configfs. `target` is relative.
    Symlink { link: String, target: String },
    /// `rm <path>` - remove a symlink attribute.
    Unlink { path: String },
    /// `rmdir <path>` - remove an empty configfs group.
    Rmdir { path: String },
}

impl FsOp {
    /// The path this operation touches.
    pub fn path(&self) -> &str {
        match self {
            FsOp::Mkdir { path }
            | FsOp::Write { path, .. }
            | FsOp::Symlink { link: path, .. }
            | FsOp::Unlink { path }
            | FsOp::Rmdir { path } => path,
        }
    }

    /// The operation as a shell command, with no error-suppression suffix.
    ///
    /// This is the form shown to a user in the manual `sudo` fallback: a human
    /// reads it top to bottom, so a best-effort step should not carry a
    /// `|| true` that implies a failure is expected.
    pub fn render_step(&self) -> String {
        match self {
            FsOp::Mkdir { path } => format!("mkdir {path}"),
            FsOp::Write { path, value, .. } => format!("echo {value} > {path}"),
            FsOp::Symlink { link, target } => format!("ln -s {target} {link}"),
            FsOp::Unlink { path } => format!("rm {path}"),
            FsOp::Rmdir { path } => format!("rmdir {path}"),
        }
    }

    /// [`Self::render_step`], plus `|| true` on a best-effort step.
    ///
    /// Presentation only, for logs and for the plan tests. Nothing in the
    /// privileged path is built by string interpolation:
    /// [`crate::display::privilege`] passes `argv` entries to a helper script,
    /// so even a hostile value could not be interpreted as a command. The only
    /// values ever rendered here have been through [`is_valid_instance_name`],
    /// which admits no shell metacharacter at all - and the manual commands are
    /// unit-tested to keep it that way.
    pub fn render(&self) -> String {
        let step = self.render_step();
        match self {
            FsOp::Write {
                best_effort: true, ..
            } => format!("{step} || true"),
            _ => step,
        }
    }
}

/// The fixed directory layout of one `vkms` configfs instance.
///
/// Derived from a root and a validated instance name; every path is a
/// single-component join of two values that have already been checked, so no
/// plan can address a path outside `root`.
///
/// Note which of these the *driver* creates for us, because it is what a plan
/// must not try to create or remove:
///
/// * `planes/`, `crtcs/`, `encoders/`, `connectors/` - created with the instance
///   group (`configfs_add_default_group` in `make_device_group`).
/// * `planes/plane0/possible_crtcs/` and `encoders/encoder0/possible_crtcs/`
///   and `connectors/connector0/possible_encoders/` - created with their item
///   group.
/// * `enabled`, `planes/plane0/type`, `connectors/connector0/status`,
///   `crtcs/crtc0/writeback` - attribute files created with the same groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstancePaths {
    /// `<root>/<instance>`.
    pub base: String,
    /// `<base>/planes/plane0`.
    pub plane: String,
    /// `<base>/planes/plane0/type`.
    pub plane_type: String,
    /// `<base>/planes/plane0/possible_crtcs`, the driver-created directory the
    /// link to the CRTC goes *into*.
    pub plane_possible_crtcs: String,
    /// `<base>/planes/plane0/possible_crtcs/crtc0`, the link itself.
    pub plane_possible_crtcs_link: String,
    /// `<base>/crtcs/crtc0`.
    pub crtc: String,
    /// `<base>/encoders/encoder0`.
    pub encoder: String,
    /// `<base>/encoders/encoder0/possible_crtcs`.
    pub encoder_possible_crtcs: String,
    /// `<base>/encoders/encoder0/possible_crtcs/crtc0`.
    pub encoder_possible_crtcs_link: String,
    /// `<base>/connectors/connector0`.
    pub connector: String,
    /// `<base>/connectors/connector0/status`.
    pub connector_status: String,
    /// `<base>/connectors/connector0/possible_encoders`.
    pub connector_possible_encoders: String,
    /// `<base>/connectors/connector0/possible_encoders/encoder0`.
    pub connector_possible_encoders_link: String,
    /// `<base>/enabled`.
    pub enabled: String,
}

impl InstancePaths {
    /// Builds the layout under `root` (which is the `vkms` configfs group, not
    /// its parent).
    ///
    /// Returns [`DisplayError::InvalidClientId`] if `instance` is not a legal
    /// name, so the one place a name enters the filesystem has one gate.
    pub fn new(root: &str, instance: &str) -> Result<Self, DisplayError> {
        if !is_valid_instance_name(instance) {
            return Err(DisplayError::InvalidClientId {
                reason: format!("{instance:?} is not a legal configfs instance name"),
            });
        }
        // A link target has to be an absolute path for `kern_path()` (see the
        // module docs), so refuse to build a plan against a relative root rather
        // than emitting targets the kernel will not resolve.
        if !root.starts_with('/') {
            return Err(DisplayError::InvalidClientId {
                reason: format!("the vkms configfs root {root:?} is not an absolute path"),
            });
        }
        let base = format!("{root}/{instance}");
        Ok(Self {
            plane: format!("{base}/planes/{PLANE_ITEM_NAME}"),
            plane_type: format!("{base}/planes/{PLANE_ITEM_NAME}/type"),
            plane_possible_crtcs: format!("{base}/planes/{PLANE_ITEM_NAME}/possible_crtcs"),
            plane_possible_crtcs_link: format!(
                "{base}/planes/{PLANE_ITEM_NAME}/possible_crtcs/{CRTC_ITEM_NAME}"
            ),
            crtc: format!("{base}/crtcs/{CRTC_ITEM_NAME}"),
            encoder: format!("{base}/encoders/{ENCODER_ITEM_NAME}"),
            encoder_possible_crtcs: format!("{base}/encoders/{ENCODER_ITEM_NAME}/possible_crtcs"),
            encoder_possible_crtcs_link: format!(
                "{base}/encoders/{ENCODER_ITEM_NAME}/possible_crtcs/{CRTC_ITEM_NAME}"
            ),
            connector: format!("{base}/connectors/{CONNECTOR_ITEM_NAME}"),
            connector_status: format!("{base}/connectors/{CONNECTOR_ITEM_NAME}/status"),
            connector_possible_encoders: format!(
                "{base}/connectors/{CONNECTOR_ITEM_NAME}/possible_encoders"
            ),
            connector_possible_encoders_link: format!(
                "{base}/connectors/{CONNECTOR_ITEM_NAME}/possible_encoders/{ENCODER_ITEM_NAME}"
            ),
            enabled: format!("{base}/enabled"),
            base,
        })
    }
}

/// Builds the ordered plan that creates and enables one `vkms` instance.
///
/// The order is load-bearing and is asserted verbatim by
/// `test_create_plan_operation_sequence`:
///
/// 1. `mkdir` the instance group, then plane, CRTC, encoder and connector -
///    in that order. The four sub-group directories and the attribute files
///    come from the driver, so this is `mkdir -p` of a fresh leaf only.
/// 2. Set the connector's `status` to `connected` (best effort).
/// 3. Set the plane's `type` to `primary` - mandatory, an instance with no
///    primary plane produces no output.
/// 4. Create the three pipeline links, **inside** the driver's
///    `possible_crtcs` / `possible_encoders` directories, with absolute
///    targets. `allow_link` refuses with `-EBUSY` once the device is enabled,
///    so these cannot come after step 5.
/// 5. Set `enabled` to `1`, **last**, so the driver never sees a half-built
///    pipeline.
///
/// Calling this function does not touch the filesystem; feed the result to
/// [`apply_plan`].
pub fn create_plan_at(root: &str, instance: &str) -> Result<Vec<FsOp>, DisplayError> {
    let p = InstancePaths::new(root, instance)?;
    Ok(vec![
        FsOp::Mkdir { path: p.base.clone() },
        FsOp::Mkdir {
            path: p.plane.clone(),
        },
        FsOp::Mkdir {
            path: p.crtc.clone(),
        },
        FsOp::Mkdir {
            path: p.encoder.clone(),
        },
        FsOp::Mkdir {
            path: p.connector.clone(),
        },
        // The driver already defaults a fresh connector to `connected` (the
        // implicit default instance reports `status=connected` without anyone
        // writing this), so this is belt and braces rather than a fix for an
        // observed failure. It is marked best effort precisely because it is
        // not required: on a kernel that does not expose the attribute,
        // provisioning must still succeed.
        FsOp::Write {
            path: p.connector_status.clone(),
            value: CONNECTOR_STATUS_CONNECTED.to_string(),
            best_effort: true,
        },
        FsOp::Write {
            path: p.plane_type.clone(),
            value: PLANE_TYPE_PRIMARY.to_string(),
            best_effort: false,
        },
        FsOp::Symlink {
            link: p.plane_possible_crtcs_link.clone(),
            // Absolute, because configfs resolves the target with `kern_path()`.
            target: p.crtc.clone(),
        },
        FsOp::Symlink {
            link: p.encoder_possible_crtcs_link.clone(),
            target: p.crtc.clone(),
        },
        FsOp::Symlink {
            link: p.connector_possible_encoders_link.clone(),
            target: p.encoder.clone(),
        },
        FsOp::Write {
            path: p.enabled.clone(),
            value: "1".to_string(),
            best_effort: false,
        },
    ])
}

/// Builds the ordered plan that disables and removes one `vkms` instance.
///
/// Links come out before their groups, because a configfs link is "busy" from
/// `rmdir`'s point of view. `enabled` goes to `0` first so the driver releases
/// the DRM card before the configuration disappears underneath it. The order is
/// asserted verbatim by `test_destroy_plan_operation_sequence`.
pub fn destroy_plan_at(root: &str, instance: &str) -> Result<Vec<FsOp>, DisplayError> {
    let p = InstancePaths::new(root, instance)?;
    Ok(vec![
        FsOp::Write {
            path: p.enabled.clone(),
            value: "0".to_string(),
            best_effort: false,
        },
        FsOp::Unlink {
            path: p.plane_possible_crtcs_link,
        },
        FsOp::Unlink {
            path: p.encoder_possible_crtcs_link,
        },
        FsOp::Unlink {
            path: p.connector_possible_encoders_link,
        },
        FsOp::Rmdir { path: p.plane },
        FsOp::Rmdir { path: p.crtc },
        FsOp::Rmdir { path: p.encoder },
        FsOp::Rmdir { path: p.connector },
        FsOp::Rmdir { path: p.base },
    ])
}

/// [`create_plan_at`] against the live kernel's configfs group.
pub fn create_plan(instance: &str) -> Result<Vec<FsOp>, DisplayError> {
    create_plan_at(VKMS_CONFIGFS_ROOT, instance)
}

/// [`destroy_plan_at`] against the live kernel's configfs group.
pub fn destroy_plan(instance: &str) -> Result<Vec<FsOp>, DisplayError> {
    destroy_plan_at(VKMS_CONFIGFS_ROOT, instance)
}

// ---------------------------------------------------------------------------
// Plan execution
// ---------------------------------------------------------------------------

/// Performs the [`FsOp`]s of a plan.
///
/// Split out from [`apply_plan`] so a plan can be executed against a real
/// temporary directory in a unit test (which is what verifies that the
/// relative symlink targets actually resolve to the intended directories) as
/// well as against configfs.
pub trait FsBackend {
    fn mkdir(&mut self, path: &str) -> Result<(), String>;
    fn write(&mut self, path: &str, value: &str) -> Result<(), String>;
    fn symlink(&mut self, target: &str, link: &str) -> Result<(), String>;
    fn unlink(&mut self, path: &str) -> Result<(), String>;
    fn rmdir(&mut self, path: &str) -> Result<(), String>;
}

/// The real filesystem. Assumes the caller already holds the privileges needed
/// for the plan's paths - on a normal system that means root, because
/// `/sys/kernel/config` is root-owned.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealFs;

impl RealFs {
    fn mkdir_recursive(path: &str) -> Result<(), String> {
        // `mkdir -p` semantics, which is what the plans need:
        //
        //  * the driver pre-creates the `planes`, `crtcs`, `encoders` and
        //    `connectors` group directories inside a new configfs instance, so
        //    re-creating them is asking for something that already exists;
        //  * creating a directory is how configfs *creates a plane*, a CRTC, an
        //    encoder or a connector, so the parent has to exist first.
        //
        // Recursion is therefore not an escape from configfs - it only ever
        // fills in the driver's own scaffolding.
        fs::create_dir_all(path).map_err(|e| format!("mkdir -p {path}: {e}"))
    }
}

impl FsBackend for RealFs {
    fn mkdir(&mut self, path: &str) -> Result<(), String> {
        Self::mkdir_recursive(path)
    }

    fn write(&mut self, path: &str, value: &str) -> Result<(), String> {
        // `fs::write` is `open(O_WRONLY|O_CREAT|O_TRUNC)`, which is exactly what
        // `tee` does - the form the kernel documentation uses for configfs
        // attributes, so O_TRUNC is known to be accepted on them.
        fs::write(path, format!("{value}\n")).map_err(|e| format!("write {path}: {e}"))
    }

    fn symlink(&mut self, target: &str, link: &str) -> Result<(), String> {
        std::os::unix::fs::symlink(target, link).map_err(|e| format!("ln -s {target} {link}: {e}"))
    }

    fn unlink(&mut self, path: &str) -> Result<(), String> {
        fs::remove_file(path).map_err(|e| format!("rm {path}: {e}"))
    }

    fn rmdir(&mut self, path: &str) -> Result<(), String> {
        fs::remove_dir(path).map_err(|e| format!("rmdir {path}: {e}"))
    }
}

/// Runs every op in `plan` through `backend`, stopping at the first hard
/// failure.
///
/// A `best_effort` op that fails is reported through the returned warning
/// string instead of aborting. The returned `String` is a log line, not a user
/// message; caller-facing text comes from `DisplayError`.
pub fn apply_plan(plan: &[FsOp], backend: &mut dyn FsBackend) -> Result<Option<String>, String> {
    let mut warning = None;
    for op in plan {
        let (result, best_effort) = match op {
            FsOp::Mkdir { path } => (backend.mkdir(path), false),
            FsOp::Write {
                path,
                value,
                best_effort,
            } => (backend.write(path, value), *best_effort),
            FsOp::Symlink { link, target } => (backend.symlink(target, link), false),
            FsOp::Unlink { path } => (backend.unlink(path), false),
            FsOp::Rmdir { path } => (backend.rmdir(path), false),
        };
        if let Err(detail) = result {
            if best_effort {
                let line = format!("optional step failed, continuing: {detail}");
                log::warn!("{line}");
                warning = Some(line);
                continue;
            }
            return Err(detail);
        }
    }
    Ok(warning)
}

/// Convenience wrapper: build the create plan and run it against the real
/// filesystem. Needs root (or another user with write access to
/// `/sys/kernel/config`), which is what [`crate::display::privilege`] obtains.
pub fn create_instance(instance: &str) -> Result<Option<String>, DisplayError> {
    apply_plan(&create_plan(instance)?, &mut RealFs).map_err(|detail| DisplayError::Io {
        context: format!("creating vkms instance {instance}"),
        detail,
    })
}

/// Convenience wrapper: build the destroy plan and run it against the real
/// filesystem. Needs root, same as [`create_instance`].
///
/// Returns `Ok(None)` without touching anything if the instance is not
/// present, so teardown is idempotent.
pub fn destroy_instance(instance: &str) -> Result<Option<String>, DisplayError> {
    if !instance_exists_at(VKMS_CONFIGFS_ROOT, instance) {
        return Ok(None);
    }
    apply_plan(&destroy_plan(instance)?, &mut RealFs).map_err(|detail| DisplayError::Io {
        context: format!("destroying vkms instance {instance}"),
        detail,
    })
}

// ---------------------------------------------------------------------------
// Probing the live system
// ---------------------------------------------------------------------------

/// One connector exposed by a DRM card, e.g. `card2-Virtual-1`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConnectorInfo {
    /// Full sysfs entry name, e.g. `card2-Virtual-1`.
    pub name: String,
    /// The connector's own name with the card prefix removed, e.g. `Virtual-1`.
    ///
    /// Kept separately because this is what identifies a *virtual* output:
    /// `vkms` names every connector it creates `Virtual-<n>`, whatever instance
    /// created it, so the card prefix carries the attribution the connector
    /// name lacks.
    pub connector: String,
    /// `connected`, `disconnected` or `unknown`, verbatim from sysfs.
    pub status: String,
    /// Whether the compositor has turned the output on.
    pub enabled: bool,
    /// First advertised mode, which is the resolution a new display would be
    /// configured with. `vkms` advertises 1920x1080 among many others.
    pub preferred_mode: Option<String>,
}

/// One DRM card and its connectors.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CardInfo {
    /// `card2`.
    pub card: String,
    pub connectors: Vec<ConnectorInfo>,
}

impl CardInfo {
    /// True if the card is a virtual display, i.e. `vkms` is loaded.
    ///
    /// `vkms` names its connectors `Virtual-<n>` and `Writeback-<n>`, so this
    /// is how an existing virtual display is recognised without keeping any
    /// state of our own.
    pub fn is_virtual(&self) -> bool {
        self.connectors.iter().any(|c| {
            c.connector.starts_with("Virtual-") || c.connector.starts_with("Writeback-")
        })
    }
}

/// One configfs instance, ours or the operator's.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstanceInfo {
    pub name: String,
    pub enabled: bool,
}

impl InstanceInfo {
    pub fn is_ours(&self) -> bool {
        is_our_instance(&self.name)
    }
}

/// Why a `vkms` configfs instance cannot be provisioned here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutError {
    /// Pre-6.19 kernels used a symlink *attribute* for `possible_crtcs` instead
    /// of a directory, so a different procedure applies. Detected by the driver
    /// exposing its built-in device as a configfs group named `default`.
    TooOld,
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LayoutError::TooOld => f.write_str(
                "this kernel predates 6.19, where vkms exposed possible_crtcs as a symlink \
                 attribute rather than a directory; provisioning needs a newer kernel, or the \
                 display already exists as the driver's default device",
            ),
        }
    }
}

/// The configfs layout this build of the driver implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfigfsLayout {
    /// `possible_crtcs` / `possible_encoders` are directories, and the pipeline
    /// is wired with `ln -s` inside them. Kernels 6.19 and newer, which is what
    /// [`create_plan_at`] implements.
    #[default]
    Modern,
    /// The pre-6.19 layout, which [`create_plan_at`] must not be run against.
    Legacy,
}

/// Which configfs layout the driver under `vkms_root` implements.
///
/// The discriminator is `vkms_drv.c`'s `default_config`: on 6.19 and newer it is
/// built directly as a DRM device, whereas before that it was registered as a
/// configfs group named `default`. So a `default` directory under the `vkms`
/// root means the legacy layout.
pub fn configfs_layout_at(vkms_root: &str) -> ConfigfsLayout {
    if Path::new(vkms_root).join(DRIVER_DEFAULT_DEVICE_NAME).is_dir() {
        ConfigfsLayout::Legacy
    } else {
        ConfigfsLayout::Modern
    }
}

/// The blocking layout problem under an explicit root, if there is one.
pub fn layout_at(vkms_root: &str) -> Option<LayoutError> {
    match configfs_layout_at(vkms_root) {
        ConfigfsLayout::Modern => None,
        ConfigfsLayout::Legacy => Some(LayoutError::TooOld),
    }
}

/// The live layout problem, if there is one.
pub fn legacy_layout() -> Option<LayoutError> {
    layout_at(VKMS_CONFIGFS_ROOT)
}

/// Everything DisplaySwarm can learn about the display state without any privilege.
///
/// Read-only: safe to call from the UI thread on a button press.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DisplayProbe {
    /// `/sys/module/vkms` exists.
    pub vkms_loaded: bool,
    /// configfs is mounted and the `vkms` group is present under it, so
    /// instances can be created.
    pub configfs_available: bool,
    /// Which configfs layout the driver implements.
    pub layout: ConfigfsLayout,
    /// Instances currently present in `/sys/kernel/config/vkms`.
    pub instances: Vec<InstanceInfo>,
    /// Cards and connectors currently exposed under `/sys/class/drm`.
    pub cards: Vec<CardInfo>,
}

impl DisplayProbe {
    /// The virtual display that exists **without** any configfs instance: the
    /// built-in device `modprobe vkms` registers.
    ///
    /// This is what stops DisplaySwarm from handing a phone that connected to a
    /// host where someone just ran `modprobe vkms` a *second* screen. vkms
    /// names the connector of its built-in device `Virtual-1` exactly as it
    /// names the connector of a configfs instance, so the connector alone
    /// cannot identify the owner - the discriminator is that an
    /// already-registered configfs instance *is* on disk with `enabled=1`.
    ///
    /// A caveat worth stating: if the operator hand-created an instance, the
    /// connectors cannot be attributed, so this reports the situation
    /// conservatively (true whenever a virtual connector exists and no
    /// enabled configfs instance does).
    pub fn implicit_default_display(&self) -> bool {
        let enabled_configfs_instance = self.instances.iter().any(|i| i.enabled);
        !enabled_configfs_instance
            && self
                .cards
                .iter()
                .any(|card| card.is_virtual() && card.connectors.iter().any(|c| c.status == "connected"))
    }

    /// The enabled DisplaySwarm-managed instance called `name`, if it is there.
    pub fn our_instance(&self, name: &str) -> Option<&InstanceInfo> {
        self.instances
            .iter()
            .find(|i| i.name == name && i.enabled)
    }

    /// The card that a newly created instance appeared as, given the card list
    /// from before it was created.
    ///
    /// Instance-to-card attribution is not exposed by the kernel, so this is a
    /// best-effort diff: a fresh card that was not there before must be the one
    /// the new instance registered. When several are created at once the
    /// mapping is approximate, which is why [`crate::display::ensure_display`]
    /// reports the count rather than claiming a specific card.
    pub fn new_card_since(&self, before: &[CardInfo]) -> Option<&CardInfo> {
        self.cards.iter().find(|card| {
            !before.iter().any(|old| {
                old.card == card.card
                    && old
                        .connectors
                        .iter()
                        .all(|c| card.connectors.contains(c))
            })
        })
    }

    /// Human-readable one-liner for the UI status field.
    pub fn summary(&self) -> String {
        if !self.vkms_loaded {
            return "Virtual display: vkms not loaded".to_string();
        }
        if self.layout == ConfigfsLayout::Legacy {
            return format!("Virtual display: needs a newer kernel ({})", LayoutError::TooOld);
        }
        let cards = self
            .cards
            .iter()
            .filter(|c| c.is_virtual())
            .map(|c| c.card.clone())
            .collect::<Vec<_>>();
        let owned = self
            .instances
            .iter()
            .filter(|i| i.is_ours())
            .map(|i| i.name.clone())
            .collect::<Vec<_>>();
        if owned.is_empty() {
            if self.implicit_default_display() {
                format!(
                    "Virtual display: {} (vkms default, no DisplaySwarm instance)",
                    cards.join(", ")
                )
            } else {
                format!("Virtual display: none ({})", cards.join(", "))
            }
        } else {
            format!(
                "Virtual display: {} via {}",
                owned.join(", "),
                if cards.is_empty() { "no card yet".into() } else { cards.join(", ") }
            )
        }
    }
}

/// True if the `vkms` module is loaded.
pub fn module_loaded() -> bool {
    Path::new(VKMS_MODULE_PATH).exists()
}

/// True if the `vkms` configfs group is present, i.e. configfs is mounted and
/// `vkms` is registered.
pub fn configfs_available() -> bool {
    Path::new(VKMS_CONFIGFS_ROOT).is_dir()
}

/// True if `root/<instance>` is present.
pub fn instance_exists_at(root: &str, instance: &str) -> bool {
    if !is_valid_instance_name(instance) {
        return false;
    }
    Path::new(root).join(instance).is_dir()
}

/// True if `VKMS_CONFIGFS_ROOT/<instance>` is present.
pub fn instance_exists(instance: &str) -> bool {
    instance_exists_at(VKMS_CONFIGFS_ROOT, instance)
}

/// [`read_probe`] against the live kernel.
pub fn read_probe() -> DisplayProbe {
    read_probe_at(VKMS_CONFIGFS_ROOT, DRM_CLASS_PATH)
}

/// [`read_probe`] against explicit roots, so the whole probe can be exercised
/// against a synthetic sysfs tree in a unit test.
pub fn read_probe_at(vkms_root: &str, drm_class: &str) -> DisplayProbe {
    DisplayProbe {
        vkms_loaded: module_loaded(),
        configfs_available: Path::new(vkms_root).is_dir(),
        layout: configfs_layout_at(vkms_root),
        instances: read_instances_at(vkms_root),
        cards: read_cards_at(drm_class),
    }
}

/// Lists the instances under a `vkms` configfs group, sorted for stability.
pub fn read_instances_at(vkms_root: &str) -> Vec<InstanceInfo> {
    let mut out: Vec<InstanceInfo> = Vec::new();
    let Ok(entries) = fs::read_dir(vkms_root) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.path().is_dir() {
            continue;
        }
        let enabled = fs::read_to_string(entry.path().join("enabled"))
            .map(|s| s.trim() == "1")
            .unwrap_or(false);
        out.push(InstanceInfo { name, enabled });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Lists DRM cards and their connectors, sorted by card name.
///
/// A connector is a sysfs entry named `cardN-<connector>-<index>`; cards are
/// `cardN` and render nodes are `renderD*`, which are ignored.
pub fn read_cards_at(drm_class: &str) -> Vec<CardInfo> {
    let mut out: Vec<CardInfo> = Vec::new();
    let Ok(entries) = fs::read_dir(drm_class) else {
        return out;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();

    for name in &names {
        let Some((card, connector)) = split_connector_name(name) else {
            continue;
        };
        // A bare `cardN` entry is the card itself, not one of its outputs.
        let Some(connector) = connector else {
            if !out.iter().any(|c| c.card == card) {
                out.push(CardInfo {
                    card,
                    connectors: Vec::new(),
                });
            }
            continue;
        };
        let entry_path = Path::new(drm_class).join(name);
        let info = ConnectorInfo {
            name: name.clone(),
            connector,
            status: fs::read_to_string(entry_path.join("status"))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "unknown".to_string()),
            enabled: fs::read_to_string(entry_path.join("enabled"))
                .map(|s| s.trim() == "enabled")
                .unwrap_or(false),
            preferred_mode: fs::read_to_string(entry_path.join("modes"))
                .ok()
                .and_then(|m| m.lines().next().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty()),
        };
        match out.iter_mut().find(|c| c.card == card) {
            Some(existing) => existing.connectors.push(info),
            None => out.push(CardInfo {
                card,
                connectors: vec![info],
            }),
        }
    }
    out
}

/// Splits `card2-Virtual-1` into `("card2", Some("Virtual-1"))`, and a bare
/// `card2` into `("card2", None)`.
///
/// Returns `None` for anything that is not a card or a card connector
/// (`renderD128`, `version`, ...).
pub fn split_connector_name(entry: &str) -> Option<(String, Option<String>)> {
    // A bare card has no connector; splitting only happens for `cardN-<...>`.
    let (card, rest) = match entry.split_once('-') {
        Some((card, rest)) => (card, Some(rest.to_string())),
        None => (entry, None),
    };
    if !card.starts_with("card") || card.len() == 4 {
        return None;
    }
    if !card[4..].bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((card.to_string(), rest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    // -----------------------------------------------------------------------
    // Instance name validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_valid_instance_names() {
        for name in [
            "a",
            "0",
            "vmon-phone-a05f",
            "vmon-0123456789",
            "vmon-a-b-c",
        ] {
            assert!(is_valid_instance_name(name), "{name:?} should be valid");
        }
    }

    /// The gate before a value reaches `pkexec`. Every rejected class is one
    /// that would change the meaning of a path or of a command.
    #[test]
    fn test_rejects_unsafe_instance_names() {
        for name in [
            "",
            "..",
            "../../etc/shadow",
            "a/b",
            "a\\b",
            "a b",
            "a;b",
            "a\nb",
            "a\0b",
            "A",                  // uppercase
            "_a",                 // must start alphanumeric
            "-a",                 // ditto
            "a.",                 // dot
            "a~b",
            "a$b",
            "a`b",
            "$(id)",
            "`id`",
            "a&&b",
            "a|b",
            "a>b",
            "a'",
            "a\"",
        ] {
            assert!(
                !is_valid_instance_name(name),
                "{name:?} must be rejected, it can change the meaning of a path"
            );
        }

        // Length ceiling, enforced independently of the sanitiser.
        let long = format!("a{}", "b".repeat(MAX_INSTANCE_NAME_LEN));
        assert!(!is_valid_instance_name(&long));
        assert!(is_valid_instance_name(&"a".repeat(MAX_INSTANCE_NAME_LEN)));
    }

    /// The whole point of the allowlist: a traversal payload must not survive
    /// sanitisation in any position, however it is spelled.
    #[test]
    fn test_sanitisation_defeats_path_traversal() {
        let attacks = [
            "../../../../etc/shadow",
            "/etc/shadow",
            "..%2f..%2fetc",
            "....//....//etc/passwd",
            "./././etc",
            "a/../../b",
        ];
        for attack in attacks {
            let name = instance_name_for(attack).expect("sanitised");
            assert!(
                is_valid_instance_name(&name),
                "{attack:?} produced illegal {name:?}"
            );
            assert!(name.starts_with(INSTANCE_PREFIX), "{name:?}");
            assert!(!name.contains(".."), "{attack:?} left '..' in {name:?}");
            assert!(!name.contains('/'), "{attack:?} left '/' in {name:?}");
            // And it must still be a single path component under the root.
            assert_eq!(
                Path::new(VKMS_CONFIGFS_ROOT).join(&name).components().count(),
                Path::new(VKMS_CONFIGFS_ROOT).components().count() + 1,
                "{attack:?} escaped the vkms configfs root"
            );
        }
    }

    // -----------------------------------------------------------------------
    // client id -> instance name
    // -----------------------------------------------------------------------

    /// The mapping is what makes reconnection idempotent, so it has to be
    /// stable across calls and processes (no clock, no counter, no state).
    #[test]
    fn test_instance_name_is_deterministic() {
        for id in ["phone-a05f", "usb-0a05f-0642", "TCP 127.0.0.1", "///", ""] {
            let a = instance_name_for(id).unwrap();
            let b = instance_name_for(id).unwrap();
            assert_eq!(a, b, "{id:?} mapped differently on a second call");
            assert!(is_valid_instance_name(&a));
        }
    }

    /// An id that is already a legal name comes through unchanged, so the
    /// instances an operator sees in `/sys/kernel/config/vkms` stay readable.
    #[test]
    fn test_clean_ids_pass_through() {
        assert_eq!(instance_name_for("phone-a05f").unwrap(), "vmon-phone-a05f");
        assert_eq!(instance_name_for("usb-0a05f-0642").unwrap(), "vmon-usb-0a05f-0642");
        assert_eq!(instance_name_for("phone1").unwrap(), "vmon-phone1");
    }

    /// Ids that sanitise to the same string must not collide, otherwise the
    /// second phone would adopt the first phone's display. This is the
    /// multi-phone requirement expressed as a property of the naming.
    #[test]
    fn test_colliding_ids_get_distinct_instances() {
        let groups = [
            vec!["Pi Zero 2", "pi-zero-2", "pi.zero.2", "pi/zero/2", "pi_zero_2"],
            vec!["phone 1", "phone-1", "phone.1", "phone/1", "phone_1"],
            vec!["///", "???", "   ", ""],
        ];
        for group in groups {
            let mut seen = std::collections::BTreeSet::new();
            for id in &group {
                let name = instance_name_for(id).unwrap();
                assert!(
                    seen.insert(name.clone()),
                    "{id:?} collided with another id in {group:?} on {name:?}"
                );
            }
        }
    }

    /// An id made entirely of rejected characters still has to produce a legal
    /// name, deterministically.
    #[test]
    fn test_unusable_ids_fall_back_to_a_digest() {
        let name = instance_name_for("///").unwrap();
        assert!(is_valid_instance_name(&name), "{name:?}");
        assert!(name.starts_with("vmon-h"), "{name:?}");
        assert_eq!(name, instance_name_for("///").unwrap());
        assert_ne!(name, instance_name_for("???").unwrap());
    }

    /// The sanitiser has a bounded output, so a hostile or absurd client id
    /// cannot produce an arbitrarily long path.
    #[test]
    fn test_name_length_is_bounded() {
        let name = instance_name_for(&"a".repeat(4096)).unwrap();
        assert!(name.len() <= MAX_INSTANCE_NAME_LEN, "{} chars", name.len());
        assert!(is_valid_instance_name(&name));
    }

    /// A USB phone re-enumerates with a different address, so the transport
    /// label alone must not be the identity. This documents the expectation
    /// that `server.rs` passes the AOA serial.
    #[test]
    fn test_distinct_phones_map_to_distinct_instances() {
        let a = instance_name_for("usb-0a05f-0642").unwrap();
        let b = instance_name_for("usb-4b1f-1180").unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with(INSTANCE_PREFIX) && b.starts_with(INSTANCE_PREFIX));
    }

    // -----------------------------------------------------------------------
    // The create plan
    // -----------------------------------------------------------------------

    /// The exact ordered list of filesystem operations that creates an
    /// instance. Asserted verbatim, because the order is load-bearing:
    /// building a group before its parent, enabling before the pipeline is
    /// linked, or missing the primary plane type all fail in ways that look
    /// like a compositor bug rather than a provisioning bug.
    #[test]
    fn test_create_plan_operation_sequence() {
        let plan = create_plan_at("/config/vkms", "vmon-phone-a05f").unwrap();
        let rendered: Vec<String> = plan.iter().map(|op| op.render()).collect();
        assert_eq!(
            rendered,
            vec![
                // 1. instance group, then plane, CRTC, encoder, connector.
                "mkdir /config/vkms/vmon-phone-a05f".to_string(),
                "mkdir /config/vkms/vmon-phone-a05f/planes/plane0".to_string(),
                "mkdir /config/vkms/vmon-phone-a05f/crtcs/crtc0".to_string(),
                "mkdir /config/vkms/vmon-phone-a05f/encoders/encoder0".to_string(),
                "mkdir /config/vkms/vmon-phone-a05f/connectors/connector0".to_string(),
                // 2. connector connected, best effort so a kernel without the
                //    attribute still provisions.
                "echo 1 > /config/vkms/vmon-phone-a05f/connectors/connector0/status || true"
                    .to_string(),
                // 3. primary plane type, mandatory.
                "echo 1 > /config/vkms/vmon-phone-a05f/planes/plane0/type".to_string(),
                // 4. the three pipeline links, INSIDE the driver's
                //    possible_crtcs / possible_encoders directories, with
                //    absolute targets.
                "ln -s /config/vkms/vmon-phone-a05f/crtcs/crtc0 /config/vkms/vmon-phone-a05f/planes/plane0/possible_crtcs/crtc0"
                    .to_string(),
                "ln -s /config/vkms/vmon-phone-a05f/crtcs/crtc0 /config/vkms/vmon-phone-a05f/encoders/encoder0/possible_crtcs/crtc0"
                    .to_string(),
                "ln -s /config/vkms/vmon-phone-a05f/encoders/encoder0 /config/vkms/vmon-phone-a05f/connectors/connector0/possible_encoders/encoder0"
                    .to_string(),
                // 5. enabled LAST, so the driver never sees a partial pipeline.
                "echo 1 > /config/vkms/vmon-phone-a05f/enabled".to_string(),
            ]
        );

        // The same ordering, asserted structurally rather than by rendered
        // string, so a reformat of `render()` cannot mask a reordering.
        let kinds: Vec<&str> = plan
            .iter()
            .map(|op| match op {
                FsOp::Mkdir { .. } => "mkdir",
                FsOp::Write { .. } => "write",
                FsOp::Symlink { .. } => "symlink",
                FsOp::Unlink { .. } => "unlink",
                FsOp::Rmdir { .. } => "rmdir",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "mkdir", "mkdir", "mkdir", "mkdir", "mkdir", "write", "write", "symlink", "symlink",
                "symlink", "write"
            ]
        );

        // Plane before CRTC before connector, and the primary type before the
        // enable: the two invariants the module documentation promises.
        let index = |needle: &str| {
            plan.iter()
                .position(|op| op.path().ends_with(needle))
                .unwrap_or_else(|| panic!("no op touching {needle}"))
        };
        assert!(index(PLANE_ITEM_NAME) < index(CRTC_ITEM_NAME), "plane must precede crtc");
        assert!(index(CRTC_ITEM_NAME) < index(ENCODER_ITEM_NAME), "crtc must precede encoder");
        assert!(index(ENCODER_ITEM_NAME) < index(CONNECTOR_ITEM_NAME), "encoder must precede connector");
        assert!(index("plane0/type") < index("/enabled"), "type must precede enable");
        assert!(
            index("possible_crtcs/crtc0") < index("/enabled"),
            "the pipeline must be linked before the instance is enabled"
        );
        assert!(
            index("possible_encoders/encoder0") < index("/enabled"),
            "the encoder must be linked before the instance is enabled"
        );
        // The enable really is last, and it is the only enable in the plan.
        assert_eq!(plan.last().map(|op| op.path()), Some("/config/vkms/vmon-phone-a05f/enabled"));
        assert_eq!(plan.iter().filter(|op| op.path().ends_with("/enabled")).count(), 1);
    }

    /// The links have to go *inside* the driver's `possible_crtcs` /
    /// `possible_encoders` directories, and their targets have to be absolute.
    ///
    /// This is the assertion that would catch the two mistakes this module was
    /// actually written to avoid: treating `possible_crtcs` as a symlink
    /// attribute (the pre-6.19 layout), and using a relative target
    /// (`../crtcs/crtc0`), which `configfs_symlink` resolves against the
    /// caller's working directory rather than the link's parent and therefore
    /// cannot work. See the module docs for the kernel source.
    #[test]
    fn test_create_plan_links_live_in_possible_directories() {
        let plan = create_plan_at("/config/vkms", "vmon-x").unwrap();
        let links: Vec<(&str, &str)> = plan
            .iter()
            .filter_map(|op| match op {
                FsOp::Symlink { link, target } => Some((link.as_str(), target.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(links.len(), 3, "the pipeline needs exactly three links");

        for (link, target) in &links {
            // The link lives one level below a `possible_*` DIRECTORY. The
            // plan never creates that directory, because the driver does.
            let parent = link.rsplit_once('/').expect("a link has a parent").0;
            assert!(
                parent.ends_with("/possible_crtcs") || parent.ends_with("/possible_encoders"),
                "link {link} is not inside a possible_* directory"
            );
            assert!(
                !plan.iter().any(|op| op.path() == parent),
                "the plan must not create {parent}; the driver does"
            );
            // Absolute, and inside the instance.
            assert!(target.starts_with('/'), "{target} must be absolute for kern_path()");
            assert!(!target.contains(".."), "{target} must not be a relative hop");
            assert!(
                target.starts_with("/config/vkms/vmon-x/"),
                "{target} points outside the instance"
            );
        }

        // Which pipeline item each link points at.
        let target_of = |suffix: &str| {
            links
                .iter()
                .find(|(link, _)| link.ends_with(suffix))
                .map(|(_, target)| *target)
                .unwrap_or_else(|| panic!("no link ending {suffix}"))
        };
        assert_eq!(target_of("planes/plane0/possible_crtcs/crtc0"), "/config/vkms/vmon-x/crtcs/crtc0");
        assert_eq!(target_of("encoders/encoder0/possible_crtcs/crtc0"), "/config/vkms/vmon-x/crtcs/crtc0");
        assert_eq!(
            target_of("connectors/connector0/possible_encoders/encoder0"),
            "/config/vkms/vmon-x/encoders/encoder0"
        );
    }

    /// Every validation rule in `vkms_config_is_valid()` has to be satisfied by
    /// this pipeline, or the kernel refuses `enabled=1` with `-EINVAL` and no
    /// display appears. Checked against the rules in `vkms_config.c`.
    #[test]
    fn test_create_plan_satisfies_every_kernel_validation_rule() {
        let plan = create_plan_at("/config/vkms", "vmon-x").unwrap();
        let link = |suffix: &str| {
            plan.iter()
                .find_map(|op| match op {
                    FsOp::Symlink { link, .. } if link.ends_with(suffix) => Some(link.as_str()),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no link ending {suffix}"))
        };

        // "The number of X must be between 1 and 31": exactly one of each.
        for item in [PLANE_ITEM_NAME, CRTC_ITEM_NAME, ENCODER_ITEM_NAME, CONNECTOR_ITEM_NAME] {
            let group = plan
                .iter()
                .filter(|op| matches!(op, FsOp::Mkdir { path } if path.ends_with(item)))
                .count();
            assert_eq!(group, 1, "exactly one {item} is created");
        }

        // "All planes must have at least one possible CRTC".
        assert!(link("planes/plane0/possible_crtcs/crtc0").contains("crtcs/crtc0"));
        // "All encoders must have at least one possible CRTC".
        assert!(link("encoders/encoder0/possible_crtcs/crtc0").contains("crtcs/crtc0"));
        // "All CRTCs must have at least one possible encoder" - satisfied
        // because the single encoder points at the single CRTC.
        assert!(link("encoders/encoder0/possible_crtcs/crtc0").contains("crtcs/crtc0"));
        // "All connectors must have at least one possible encoder".
        assert!(link("connectors/connector0/possible_encoders/encoder0").contains("encoders/encoder0"));

        // A primary plane exists, which is what makes the display light up.
        let primary = plan.iter().any(|op| {
            matches!(op, FsOp::Write { path, value, best_effort: false }
                if path.ends_with("planes/plane0/type") && value == "1")
        });
        assert!(primary, "the plane must be set to primary (1)");
    }

    /// The destroy plan, verbatim. Symlinks out before groups, because
    /// `rmdir` refuses a non-empty group.
    #[test]
    fn test_destroy_plan_operation_sequence() {
        let plan = destroy_plan_at("/config/vkms", "vmon-phone-a05f").unwrap();
        let rendered: Vec<String> = plan.iter().map(|op| op.render()).collect();
        assert_eq!(
            rendered,
            vec![
                // disable first, so the card is released before it is orphaned
                "echo 0 > /config/vkms/vmon-phone-a05f/enabled".to_string(),
                "rm /config/vkms/vmon-phone-a05f/planes/plane0/possible_crtcs/crtc0".to_string(),
                "rm /config/vkms/vmon-phone-a05f/encoders/encoder0/possible_crtcs/crtc0".to_string(),
                "rm /config/vkms/vmon-phone-a05f/connectors/connector0/possible_encoders/encoder0"
                    .to_string(),
                "rmdir /config/vkms/vmon-phone-a05f/planes/plane0".to_string(),
                "rmdir /config/vkms/vmon-phone-a05f/crtcs/crtc0".to_string(),
                "rmdir /config/vkms/vmon-phone-a05f/encoders/encoder0".to_string(),
                "rmdir /config/vkms/vmon-phone-a05f/connectors/connector0".to_string(),
                "rmdir /config/vkms/vmon-phone-a05f".to_string(),
            ]
        );

        // Unlink before the matching rmdir, for every group. The group the
        // `rmdir` targets is the item, whose name is a prefix of the link's
        // path.
        let unlinks: Vec<&str> = plan
            .iter()
            .filter_map(|op| match op {
                FsOp::Unlink { path } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(unlinks.len(), 3);
        for item in [PLANE_ITEM_NAME, ENCODER_ITEM_NAME, CONNECTOR_ITEM_NAME] {
            let group = format!("/{item}");
            let last_unlink = plan
                .iter()
                .rposition(|op| matches!(op, FsOp::Unlink { path } if path.contains(&group)))
                .unwrap_or_else(|| panic!("no unlink under {item}"));
            let rmdir = plan
                .iter()
                .position(|op| matches!(op, FsOp::Rmdir { path } if path.ends_with(&group)))
                .unwrap_or_else(|| panic!("no rmdir for {item}"));
            assert!(last_unlink < rmdir, "{item}: unlink must precede rmdir");
        }
    }

    /// Create and destroy must be exactly complementary, or a teardown would
    /// leave something behind (or a re-provision would hit EEXIST).
    #[test]
    fn test_plans_are_complementary() {
        let create = create_plan_at("/config/vkms", "vmon-x").unwrap();
        let destroy = destroy_plan_at("/config/vkms", "vmon-x").unwrap();

        // The groups create makes are the groups destroy removes. Attribute
        // writes are excluded: `enabled`, `type` and `status` are files the
        // *driver* creates with the group, and it removes them again, so the
        // plan neither creates nor has to unlink them.
        let created_groups: Vec<&str> = create
            .iter()
            .filter_map(|op| match op {
                FsOp::Mkdir { path } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        let destroyed_groups: Vec<&str> = destroy
            .iter()
            .filter_map(|op| match op {
                FsOp::Rmdir { path } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert!(created_groups.contains(&"/config/vkms/vmon-x/planes/plane0"));
        for path in &created_groups {
            assert!(
                destroyed_groups.contains(path),
                "create made the group {path}, which destroy never removes"
            );
        }
        for path in &destroyed_groups {
            assert!(
                created_groups.contains(path),
                "destroy removes the group {path}, which create never made"
            );
        }

        // And every attribute create writes lives inside a group destroy
        // removes, so nothing can be left orphaned.
        for op in &create {
            if let FsOp::Write { path, .. } = op {
                let group = path.rsplit_once('/').expect("an attribute has a parent").0;
                assert!(
                    destroyed_groups.contains(&group),
                    "{path} is written into {group}, which destroy never removes"
                );
            }
        }

        // Every symlink create makes is unlinked by destroy, by the same path.
        let links: Vec<&str> = create
            .iter()
            .filter_map(|op| match op {
                FsOp::Symlink { link, .. } => Some(link.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(links.len(), 3);
        for link in &links {
            assert!(
                destroyed_groups.iter().any(|g| g == link) || links.contains(link),
                "create links {link}, which destroy never unlinks"
            );
            let unlinked = destroy.iter().any(|op| matches!(op, FsOp::Unlink { path } if path == link));
            assert!(unlinked, "create links {link}, which destroy never unlinks");
        }
    }

    /// The plan builder is the one gate between a name and a path, so it has to
    /// refuse rather than sanitise a second time.
    #[test]
    fn test_plans_reject_illegal_instance_names() {
        for bad in ["../evil", "a/b", "", "A", "vmon-x;rm -rf /"] {
            assert!(create_plan_at("/config/vkms", bad).is_err(), "create accepted {bad:?}");
            assert!(destroy_plan_at("/config/vkms", bad).is_err(), "destroy accepted {bad:?}");
        }
    }

    /// Every path a plan touches must be inside the configfs root it was given,
    /// and every symlink *target* must be too. A single-component join of two
    /// validated values cannot escape, and this is the test that keeps that true.
    #[test]
    fn test_plan_paths_stay_inside_the_root() {
        let root = "/config/vkms";
        for plan in [
            create_plan_at(root, "vmon-x").unwrap(),
            destroy_plan_at(root, "vmon-x").unwrap(),
        ] {
            for op in &plan {
                let path = op.path();
                assert!(path.starts_with(root), "{path} escaped {root}");
                assert!(!path.contains("/../"), "{path} contains a traversal");
                if let FsOp::Symlink { target, .. } = op {
                    // Absolute, because configfs resolves the target with
                    // `kern_path()` rather than relative to the link.
                    assert!(target.starts_with('/'), "{target} is not absolute");
                    assert!(target.starts_with(root), "{target} escaped {root}");
                    // A pipeline item of *this* instance, nothing else.
                    let instance_root = format!("{root}/vmon-x/");
                    let landed = target.strip_prefix(&instance_root).unwrap_or_else(|| {
                        panic!("{target} is not inside {instance_root}")
                    });
                    assert!(
                        [format!("crtcs/{CRTC_ITEM_NAME}"), format!("encoders/{ENCODER_ITEM_NAME}")]
                            .contains(&landed.to_string()),
                        "{target} points at {landed}, which is not a pipeline item of this instance"
                    );
                }
            }
        }
    }

    /// A plan built against a relative root would emit relative link targets,
    /// which `kern_path()` resolves against the caller's working directory and
    /// which therefore cannot work. Refused rather than produced.
    #[test]
    fn test_plans_require_an_absolute_root() {
        for bad_root in ["vkms", "config/vkms", "./vkms", ""] {
            assert!(
                create_plan_at(bad_root, "vmon-x").is_err(),
                "a relative root {bad_root:?} must be refused"
            );
        }
    }

    /// The pre-6.19 layout has to be recognised, not attempted. There,
    /// `possible_crtcs` is a symlink *attribute* rather than a directory, so
    /// this plan would create the instance and then fail at `enabled=1`.
    #[test]
    fn test_legacy_layout_is_detected() {
        let tmp = TempDir::new("layout");
        let root = tmp.join("vkms");
        fs::create_dir_all(&root).unwrap();

        // 6.19 and newer: the driver creates its default device directly as a
        // DRM device, with no `default` directory in configfs.
        let root = root.to_str().unwrap();
        assert_eq!(configfs_layout_at(root), ConfigfsLayout::Modern);
        assert!(layout_at(root).is_none());

        // Pre-6.19: `default_config` is registered as a configfs group.
        fs::create_dir_all(tmp.join("vkms").join(DRIVER_DEFAULT_DEVICE_NAME)).unwrap();
        assert_eq!(configfs_layout_at(root), ConfigfsLayout::Legacy);
        assert_eq!(layout_at(root), Some(LayoutError::TooOld));

        // And the probe carries it, so the UI can say why.
        // NOTE: `read_probe_at` reports `vkms_loaded` from the REAL /sys, so the
        // summary depends on whether the driver happens to be loaded on the
        // machine running the tests. Assert only on the part this test controls.
        let probe = read_probe_at(root, "/nonexistent/drm");
        assert_eq!(probe.layout, ConfigfsLayout::Legacy);
        if module_loaded() {
            assert!(probe.summary().contains("newer kernel"), "{}", probe.summary());
        } else {
            assert!(
                probe.summary().contains("not loaded"),
                "{}",
                probe.summary()
            );
        }
    }

    // -----------------------------------------------------------------------
    // Executing a plan against a real directory
    // -----------------------------------------------------------------------

    /// A throwaway directory per test, cleaned up on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "vmon-vkms-test-{}-{}-{}",
                std::process::id(),
                tag,
                n
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn join(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }

    /// Dropped at the end of a test.
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A plain directory behaves differently from configfs in one important way:
    /// the **driver creates scaffolding when a group is created**, and removes it
    /// again with the group. Applied to an ordinary directory, a plan that
    /// correctly omits all of that fails at the final `rmdir` with `ENOTEMPTY`
    /// for a reason that has nothing to do with the plan.
    ///
    /// From `vkms_configfs.c`:
    ///
    /// * `make_device_group` adds `planes`, `crtcs`, `encoders`, `connectors`
    ///   and the `enabled` attribute.
    /// * `make_plane_group` adds a `possible_crtcs` **directory** and `type`.
    /// * `make_crtc_group` adds `writeback`.
    /// * `make_encoder_group` adds a `possible_crtcs` directory.
    /// * `make_connector_group` adds a `possible_encoders` directory and `status`.
    ///
    /// This shim reproduces exactly that, which is what lets the round-trip test
    /// assert that create-then-destroy leaves *nothing* behind - the property
    /// teardown depends on - and lets the plan omit the scaffolding it must not
    /// create itself.
    struct FakeConfigFs {
        /// Attribute files this emulation believes the driver created.
        created: Vec<String>,
        /// Reject writes whose path ends with one of these, simulating a kernel
        /// without the attribute.
        refuse: Vec<&'static str>,
    }

    impl FakeConfigFs {
        fn new() -> Self {
            Self {
                created: Vec::new(),
                refuse: Vec::new(),
            }
        }

        /// Reject writes to any path ending with one of `suffixes`, simulating a
        /// kernel that does not expose the attribute.
        fn refusing(suffixes: &[&'static str]) -> Self {
            Self {
                created: Vec::new(),
                refuse: suffixes.to_vec(),
            }
        }

        /// The scaffolding the driver adds when the group at `path` is created,
        /// as `(name, is_directory)`.
        fn driver_children(path: &str) -> Vec<(&'static str, bool)> {
            let last = path.rsplit('/').next().unwrap_or("");
            match last {
                PLANE_ITEM_NAME => vec![("possible_crtcs", true), ("type", false)],
                CRTC_ITEM_NAME => vec![("writeback", false)],
                ENCODER_ITEM_NAME => vec![("possible_crtcs", true)],
                CONNECTOR_ITEM_NAME => vec![("possible_encoders", true), ("status", false)],
                // The instance group itself, identified by our own prefix.
                name if name.starts_with(INSTANCE_PREFIX) => {
                    vec![("enabled", false), ("planes", true), ("crtcs", true), ("encoders", true),
                         ("connectors", true)]
                }
                _ => Vec::new(),
            }
        }
    }

    impl FsBackend for FakeConfigFs {
        fn mkdir(&mut self, path: &str) -> Result<(), String> {
            RealFs.mkdir(path)?;
            for (name, is_dir) in Self::driver_children(path) {
                let child = format!("{path}/{name}");
                if is_dir {
                    fs::create_dir_all(&child).map_err(|e| format!("{child}: {e}"))?;
                } else {
                    fs::write(&child, "0\n").map_err(|e| format!("{child}: {e}"))?;
                }
                self.created.push(child);
            }
            Ok(())
        }

        fn write(&mut self, path: &str, value: &str) -> Result<(), String> {
            if self.refuse.iter().any(|suffix| path.ends_with(suffix)) {
                return Err(format!("{path}: Invalid argument (simulated)"));
            }
            RealFs.write(path, value)
        }

        fn symlink(&mut self, target: &str, link: &str) -> Result<(), String> {
            RealFs.symlink(target, link)
        }

        fn unlink(&mut self, path: &str) -> Result<(), String> {
            RealFs.unlink(path)
        }

        fn rmdir(&mut self, path: &str) -> Result<(), String> {
            // The driver takes its scaffolding with the group.
            for (name, is_dir) in Self::driver_children(path) {
                let child = format!("{path}/{name}");
                if is_dir {
                    let _ = fs::remove_dir_all(&child);
                } else {
                    let _ = fs::remove_file(&child);
                }
            }
            RealFs.rmdir(path)
        }
    }

    /// Applies the create plan to a real directory tree, which is the only way
    /// to check that the *relative* symlink targets really do resolve to the
    /// intended configfs groups - an absolute target would silently produce a
    /// dangling link and the instance would never light up.
    #[test]
    fn test_create_plan_builds_a_working_tree() {
        let tmp = TempDir::new("create");
        let root = tmp.join("vkms");
        fs::create_dir_all(&root).unwrap();
        let root = root.to_str().unwrap();

        let plan = create_plan_at(root, "vmon-x").unwrap();
        let warning = apply_plan(&plan, &mut FakeConfigFs::new()).expect("plan applied");
        assert!(warning.is_none(), "no optional step should have failed: {warning:?}");

        let base = Path::new(root).join("vmon-x");
        for dir in ["planes/plane0", "crtcs/crtc0", "encoders/encoder0", "connectors/connector0"] {
            assert!(base.join(dir).is_dir(), "{dir} missing");
        }
        assert_eq!(
            fs::read_to_string(base.join("planes/plane0/type")).unwrap().trim(),
            "1",
            "the plane must be primary"
        );
        assert_eq!(fs::read_to_string(base.join("enabled")).unwrap().trim(), "1");
        assert_eq!(fs::read_to_string(base.join("connectors/connector0/status")).unwrap().trim(), "1");

        // The links exist, resolve to the real groups, and are absolute - which
        // is what `configfs_symlink`'s `kern_path()` lookup requires.
        for (link, want) in [
            (
                "planes/plane0/possible_crtcs/crtc0",
                "crtcs/crtc0",
            ),
            (
                "encoders/encoder0/possible_crtcs/crtc0",
                "crtcs/crtc0",
            ),
            (
                "connectors/connector0/possible_encoders/encoder0",
                "encoders/encoder0",
            ),
        ] {
            let link_path = base.join(link);
            let target = fs::read_link(&link_path)
                .unwrap_or_else(|e| panic!("{link} is not a symlink: {e}"));
            assert!(target.is_absolute(), "{link} target {target:?} must be absolute");
            assert!(
                fs::canonicalize(&link_path).is_ok(),
                "{link} -> {target:?} is dangling"
            );
            // The resolved link must point at the instance's own group, not
            // something else with the same basename.
            let resolved = fs::canonicalize(&link_path).unwrap();
            assert_eq!(
                resolved,
                fs::canonicalize(base.join(want)).unwrap(),
                "{link} resolved to the wrong group"
            );
        }

        // The driver's scaffolding is present without the plan creating it,
        // exactly as configfs would have it.
        for dir in [
            "planes",
            "crtcs",
            "encoders",
            "connectors",
            "planes/plane0/possible_crtcs",
            "encoders/encoder0/possible_crtcs",
            "connectors/connector0/possible_encoders",
        ] {
            assert!(base.join(dir).is_dir(), "the driver should have provided {dir}");
        }
        for attr in [
            "enabled",
            "planes/plane0/type",
            "crtcs/crtc0/writeback",
            "connectors/connector0/status",
        ] {
            assert!(base.join(attr).is_file(), "the driver should have provided {attr}");
        }
    }

    /// Round trip: create, then destroy, leaves nothing behind. This is what
    /// makes `destroy_instance` safe to call on a disconnect, and it is the only
    /// test that would catch a plan which forgets to remove something.
    #[test]
    fn test_create_then_destroy_round_trips() {
        let tmp = TempDir::new("roundtrip");
        let root = tmp.join("vkms");
        fs::create_dir_all(&root).unwrap();
        let root = root.to_str().unwrap();

        let mut fs_backend = FakeConfigFs::new();
        apply_plan(&create_plan_at(root, "vmon-x").unwrap(), &mut fs_backend).expect("create");
        assert!(instance_exists_at(root, "vmon-x"));

        apply_plan(&destroy_plan_at(root, "vmon-x").unwrap(), &mut fs_backend).expect("destroy");
        assert!(!instance_exists_at(root, "vmon-x"), "instance dir survived destroy");
        assert_eq!(
            fs::read_dir(root).unwrap().count(),
            0,
            "destroy left files behind under the vkms root"
        );
    }

    /// A missing optional attribute must not abort provisioning, or a kernel
    /// without it would leave the user with no display and no explanation.
    #[test]
    fn test_best_effort_write_does_not_abort_the_plan() {
        let tmp = TempDir::new("besteffort");
        let root = tmp.join("vkms");
        fs::create_dir_all(&root).unwrap();
        let root = root.to_str().unwrap();

        // Simulate a kernel with no connector `status` attribute.
        let mut backend = FakeConfigFs::refusing(&["/status"]);
        let warning = apply_plan(&create_plan_at(root, "vmon-x").unwrap(), &mut backend)
            .expect("the plan must continue past an optional failure");
        assert!(warning.is_some(), "the failure should be reported as a warning");
        assert!(warning.unwrap().contains("optional step failed"));
        // Crucially, it still got as far as enabling the instance.
        assert_eq!(
            fs::read_to_string(Path::new(root).join("vmon-x/enabled")).unwrap().trim(),
            "1"
        );
    }

    /// A *mandatory* failure must stop the plan, rather than leaving the driver
    /// a half-configured instance.
    #[test]
    fn test_mandatory_failure_aborts_the_plan() {
        let tmp = TempDir::new("mandatory");
        let root = tmp.join("vkms");
        fs::create_dir_all(&root).unwrap();
        let root = root.to_str().unwrap();

        // Simulate a kernel that rejects the primary plane type.
        let mut backend = FakeConfigFs::refusing(&["/type"]);
        let err = apply_plan(&create_plan_at(root, "vmon-x").unwrap(), &mut backend)
            .expect_err("the plan must abort on a mandatory failure");
        assert!(err.contains("Invalid argument"), "{err}");
        // `enabled` is after the type write, so it must never have been set.
        assert_eq!(
            fs::read_to_string(Path::new(root).join("vmon-x/enabled")).unwrap().trim(),
            "0",
            "the instance must not have been enabled"
        );
    }

    // -----------------------------------------------------------------------
    // Probing
    // -----------------------------------------------------------------------

    /// A synthetic sysfs tree: one configfs instance of ours (enabled), one
    /// card with a `Virtual-1` connector, plus noise that must be ignored.
    fn fake_drm_tree() -> TempDir {
        let tmp = TempDir::new("probe");
        for entry in ["card1-DP-1", "card2-Virtual-1", "renderD128", "version"] {
            let dir = tmp.join(entry);
            fs::create_dir_all(&dir).unwrap();
        }
        fs::write(tmp.join("card1-DP-1/status"), "connected\n").unwrap();
        fs::write(tmp.join("card1-DP-1/enabled"), "enabled\n").unwrap();
        fs::write(tmp.join("card1-DP-1/modes"), "3840x2160\n1920x1080\n").unwrap();
        fs::write(tmp.join("card2-Virtual-1/status"), "connected\n").unwrap();
        fs::write(tmp.join("card2-Virtual-1/enabled"), "enabled\n").unwrap();
        fs::write(tmp.join("card2-Virtual-1/modes"), "1024x768\n1920x1080\n").unwrap();
        tmp
    }

    #[test]
    fn test_read_cards_ignores_render_nodes_and_joins_connectors() {
        let tmp = fake_drm_tree();
        let cards = read_cards_at(tmp.0.to_str().unwrap());
        let names: Vec<&str> = cards.iter().map(|c| c.card.as_str()).collect();
        assert_eq!(names, vec!["card1", "card2"], "renderD128 and version are not cards");

        let card2 = cards.iter().find(|c| c.card == "card2").unwrap();
        assert_eq!(card2.connectors.len(), 1);
        let conn = &card2.connectors[0];
        assert_eq!(conn.name, "card2-Virtual-1");
        assert_eq!(conn.connector, "Virtual-1", "the card prefix carries the attribution");
        assert_eq!(conn.status, "connected");
        assert!(conn.enabled);
        assert_eq!(conn.preferred_mode.as_deref(), Some("1024x768"));
        assert!(card2.is_virtual());

        let card1 = cards.iter().find(|c| c.card == "card1").unwrap();
        assert!(!card1.is_virtual(), "a physical DP connector is not a virtual display");
    }

    #[test]
    fn test_split_connector_name() {
        assert_eq!(
            split_connector_name("card2-Virtual-1"),
            Some(("card2".into(), Some("Virtual-1".into())))
        );
        assert_eq!(split_connector_name("card10-HDMI-A-2"),
            Some(("card10".into(), Some("HDMI-A-2".into()))));
        assert_eq!(split_connector_name("card2"), Some(("card2".into(), None)));
        for junk in ["renderD128", "version", "card", "cardX-1", "xcard2-1", ""] {
            assert_eq!(split_connector_name(junk), None, "{junk:?} should not be a card");
        }
    }

    #[test]
    fn test_read_instances_reads_enabled_flag() {
        let tmp = TempDir::new("instances");
        let root = tmp.join("vkms");
        fs::create_dir_all(root.join("vmon-b")).unwrap();
        fs::create_dir_all(root.join("vmon-a")).unwrap();
        fs::create_dir_all(root.join("handmade")).unwrap();
        fs::write(root.join("vmon-a/enabled"), "1\n").unwrap();
        fs::write(root.join("vmon-b/enabled"), "0\n").unwrap();

        let instances = read_instances_at(root.to_str().unwrap());
        let names: Vec<&str> = instances.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["handmade", "vmon-a", "vmon-b"], "sorted, and dirs only");
        assert!(instances[0].enabled == false);
        assert!(instances[1].enabled == true);
        assert!(instances[2].enabled == false);
        assert!(!instances[0].is_ours(), "a hand-made instance is not ours");
        assert!(instances[1].is_ours());
    }

    /// The duplicate-screen trap. `modprobe vkms` alone leaves a connected
    /// `Virtual-1` with no configfs instance; creating ours on top of that
    /// would give one phone two screens.
    #[test]
    fn test_implicit_default_display_detection() {
        let tmp = fake_drm_tree();
        let probe = |instances: Vec<InstanceInfo>| DisplayProbe {
            vkms_loaded: true,
            configfs_available: true,
            layout: ConfigfsLayout::Modern,
            instances,
            cards: read_cards_at(tmp.0.to_str().unwrap()),
        };

        // Card2 is virtual, no configfs instance: the implicit default. This is
        // the case that must NOT trigger a second instance.
        let implicit = probe(vec![]);
        assert!(implicit.implicit_default_display());
        assert!(implicit.summary().contains("vkms default"));

        // One enabled instance of ours: we own the screen, nothing implicit.
        let ours = probe(vec![InstanceInfo {
            name: "vmon-phone-a05f".into(),
            enabled: true,
        }]);
        assert!(!ours.implicit_default_display());
        assert!(ours.our_instance("vmon-phone-a05f").is_some());
        assert!(ours.summary().contains("vmon-phone-a05f"));

        // A *disabled* instance does not count as provisioned, so the implicit
        // default is still what is on screen.
        let disabled = probe(vec![InstanceInfo {
            name: "vmon-phone-a05f".into(),
            enabled: false,
        }]);
        assert!(disabled.implicit_default_display());

        // No virtual card at all (a headless box with vkms loaded but no
        // instance): nothing to adopt.
        let none = DisplayProbe {
            vkms_loaded: true,
            configfs_available: true,
            layout: ConfigfsLayout::Modern,
            instances: vec![],
            cards: vec![CardInfo {
                card: "card1".into(),
                connectors: read_cards_at(tmp.0.to_str().unwrap())[0].connectors.clone(),
            }],
        };
        assert!(!none.implicit_default_display());
    }

    #[test]
    fn test_new_card_since_finds_the_added_card() {
        let tmp = fake_drm_tree();
        let drm = tmp.0.to_str().unwrap();
        let before = read_cards_at(drm);

        let tmp2 = fake_drm_tree();
        fs::create_dir_all(tmp2.join("card3-Virtual-1")).unwrap();
        fs::write(tmp2.join("card3-Virtual-1/status"), "connected\n").unwrap();
        fs::write(tmp2.join("card3-Virtual-1/enabled"), "disabled\n").unwrap();
        let after = read_cards_at(tmp2.0.to_str().unwrap());

        let probe = DisplayProbe {
            cards: after,
            ..DisplayProbe::default()
        };
        let new = probe.new_card_since(&before).expect("a new card appeared");
        assert_eq!(new.card, "card3");
        assert!(new.is_virtual());
        assert!(new.connectors[0].preferred_mode.is_none(), "no modes file written");
    }

    /// A probe of a machine with nothing in place must not panic and must not
    /// claim a display exists.
    #[test]
    fn test_probe_of_missing_directories_is_empty() {
        let probe = read_probe_at("/nonexistent/vkms", "/nonexistent/drm");
        assert!(probe.instances.is_empty());
        assert!(probe.cards.is_empty());
        assert!(!probe.implicit_default_display());
        // `vkms_loaded` comes from the real /sys, so the summary is
        // "nothing found" only when the driver is absent. Branch on it rather
        // than making the test pass or fail depending on the host.
        if module_loaded() {
            assert!(probe.summary().contains("none"), "{}", probe.summary());
        } else {
            assert!(
                probe.summary().contains("not loaded"),
                "{}",
                probe.summary()
            );
        }
    }
}
