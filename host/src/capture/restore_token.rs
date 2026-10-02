//! On-disk store for the XDG portal screencast `restore_token`.
//!
//! # What this buys us
//!
//! `SelectSources` accepts a `restore_token`; when it names a session the
//! portal still has permission for, the previously-approved monitor selection
//! is restored and **no dialog is shown**. That is the difference between
//! approving once and approving before every single test run.
//!
//! # Why the token is a real privilege
//!
//! A restore token is a *bearer credential for screen capture*. Possessing it
//! is enough to start capturing the user's screen with no prompt and no further
//! consent, which is exactly the thing the portal dialog exists to ask about.
//! It is therefore written `0600` inside a `0700` directory and never logged
//! (see [`load`] - failures report the *path*, not the contents).
//! It is not a secret in the "password" sense, since the portal cannot
//! authenticate the holder, but it must still not be world-readable: a
//! restore token plus any process able to speak D-Bus as this user is a
//! silent screen-sharing session.
//!
//! # Degradation
//!
//! Every function here is total: nothing panics, and any problem - missing
//! file, empty file, corrupt content, unreadable directory, read-only
//! filesystem - degrades to "no token", which costs exactly one dialog. A
//! token store that can crash the capture thread to save a click would be a
//! bad trade.

use std::fs;
use std::path::{Path, PathBuf};

/// Overrides the token file location. Intended for tests and for anyone who
/// wants the token somewhere other than the XDG state directory.
pub const RESTORE_TOKEN_PATH_ENV: &str = "DISPLAYSWARM_RESTORE_TOKEN_PATH";

/// Set to a truthy value to make the next run forget the stored token.
///
/// Naming convention matches the other `DISPLAYSWARM_` switches in this crate; see
/// `DISPLAYSWARM_CAPTURE_OUTPUT` in [`super::linux`] and `BIND_ENV_VAR` in
/// [`crate::server`]. This is deliberately an environment variable and not a
/// `--cli` flag: it is read by the capture path itself, so it works for the
/// GUI build and the headless build alike without either entry point needing
/// to know about it.
pub const RESET_RESTORE_TOKEN_ENV: &str = "DISPLAYSWARM_RESET_RESTORE_TOKEN";

/// The XDG variable that says where per-user state belongs.
const STATE_HOME_ENV: &str = "XDG_STATE_HOME";

/// Fallback for `XDG_STATE_HOME`, relative to `$HOME`.
///
/// `$XDG_STATE_HOME` is unset surprisingly often on minimal systems even
/// though `~/.local/state` is the documented default.
const DEFAULT_STATE_SUBDIR: &str = ".local/state";

/// Per-application subdirectory, so this file is owned by DisplaySwarm alone.
const APP_DIR_NAME: &str = "displayswarm";

/// The token file itself.
const TOKEN_FILE_NAME: &str = "restore_token";

/// Owner read/write only. See the module docs: this is a screen-capture
/// credential.
const TOKEN_FILE_MODE: u32 = 0o600;

/// Owner-only directory, so the token cannot be listed or replaced by another
/// local user.
const STATE_DIR_MODE: u32 = 0o700;

/// Longest token accepted.
///
/// Portal tokens are a few hundred bytes at most. Anything longer means the
/// file is not a token file, and refusing it keeps a corrupt or concatenated
/// file from being sent to the portal as an option value.
const MAX_TOKEN_LEN: usize = 4096;

/// Where the token file lives, or `None` if the environment does not say.
///
/// Resolution order:
///
/// 1. [`RESTORE_TOKEN_PATH_ENV`], if set to a non-empty value. Used verbatim,
///    including by a relative path, because it exists to point tests at a temp
///    directory.
/// 2. `$XDG_STATE_HOME/displayswarm/restore_token`.
/// 3. `$HOME/.local/state/displayswarm/restore_token`.
///
/// A base directory is only accepted if it is absolute, matching
/// [`crate::display::privilege::helper_dir`]: a relative `XDG_STATE_HOME`
/// would resolve against the current working directory, which for a daemon is
/// not meaningful.
pub fn token_path() -> Option<PathBuf> {
    token_path_for(None)
}

/// Turns a caller-supplied token key (`"<device_id>/<target kind>"`) into a
/// string that is safe as part of a file name.
///
/// ASCII letters, digits, `-` and `_` are kept, `/` becomes `.` so the two
/// halves of a key stay distinguishable, and everything else (including
/// non-ASCII, whitespace, NUL, `\`) becomes `_`. The result never contains a
/// path separator, so a key can never escape the state directory; it is capped
/// at 96 bytes and is never empty.
pub fn sanitize_key(key: &str) -> String {
    let mut out: String = key
        .chars()
        .map(|c| match c {
            c if c.is_ascii_alphanumeric() || c == '-' || c == '_' => c,
            '/' => '.',
            _ => '_',
        })
        .take(MAX_KEY_LEN)
        .collect();
    if out.is_empty() || out.chars().all(|c| c == '.') {
        out = format!("key{out}");
    }
    out
}

/// Longest sanitised key kept in a file name.
const MAX_KEY_LEN: usize = 96;

/// [`token_path`] for a per-device/per-target token.
///
/// `None` (or an empty key) is the legacy shared token file. `Some(key)` is
/// `restore_token.<sanitised key>` next to it (or, with
/// [`RESTORE_TOKEN_PATH_ENV`], `<that path>.<sanitised key>`).
pub fn token_path_for(key: Option<&str>) -> Option<PathBuf> {
    let suffix = key.map(str::trim).filter(|k| !k.is_empty()).map(sanitize_key);
    let base = if let Some(raw) = std::env::var_os(RESTORE_TOKEN_PATH_ENV).filter(|v| !v.is_empty())
    {
        PathBuf::from(raw)
    } else {
        absolute_state_dir()?.join(APP_DIR_NAME).join(TOKEN_FILE_NAME)
    };
    Some(with_key_suffix(base, suffix.as_deref()))
}

/// Appends `.<suffix>` to the file name of `base`.
fn with_key_suffix(base: PathBuf, suffix: Option<&str>) -> PathBuf {
    match suffix {
        None => base,
        Some(s) => {
            let mut name = base.file_name().map(|n| n.to_os_string()).unwrap_or_default();
            name.push(".");
            name.push(s);
            base.with_file_name(name)
        }
    }
}

/// `$XDG_STATE_HOME` or `$HOME/.local/state`, whichever is usable.
fn absolute_state_dir() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os(STATE_HOME_ENV).filter(|v| !v.is_empty()) {
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            return Some(path);
        }
        log::debug!(
            "{STATE_HOME_ENV} is set to the relative path {}, which is not usable; ignoring it",
            path.display()
        );
    }

    let home = std::env::var_os("HOME").filter(|v| !v.is_empty())?;
    let path = PathBuf::from(home).join(DEFAULT_STATE_SUBDIR);
    path.is_absolute().then_some(path)
}

/// Reads the stored token, honouring [`RESET_RESTORE_TOKEN_ENV`].
///
/// A "no token" answer is always a legitimate one: it is the first run, the
/// environment says where nothing is, the file is missing, or it is
/// unreadable. Every path returns `None` rather than an error, because failing
/// the capture over an optional optimisation is never right - the portal
/// simply asks the user to approve, which is the pre-existing behaviour.
///
/// If [`RESET_RESTORE_TOKEN_ENV`] is truthy the file is deleted first, so one
/// variable both forgets the old token and lets the run that follows store a
/// fresh one.
pub fn load() -> Option<String> {
    load_keyed(None)
}

/// [`load`] for a per-device/per-target token (see [`token_path_for`]). The
/// reset variable clears the keyed token too.
pub fn load_keyed(key: Option<&str>) -> Option<String> {
    if is_reset_requested() {
        log::info!(
            "{RESET_RESTORE_TOKEN_ENV} is set, so the stored screen-capture restore token \
             is being discarded; the next screencast will ask for approval once and store a \
             new token"
        );
        if let Err(e) = forget_keyed(key) {
            log::warn!("could not delete the restore token: {e}");
        }
        return None;
    }

    load_from(&token_path_for(key)?)
}

/// [`load`] against an explicit path. Split out so the store can be tested
/// without mutating the process environment, which is racy under a parallel
/// test runner.
fn load_from(path: &Path) -> Option<String> {
    // A missing file is the first-run case, not a problem: `Err` with
    // `NotFound` is expected and must not be logged as a warning.
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            // Reports the path, never the contents.
            log::warn!("could not read the restore token at {}: {e}", path.display());
            return None;
        }
    };

    match parse_stored_token(&raw) {
        Some(token) => Some(token),
        None => {
            log::warn!(
                "the restore token at {} is empty or corrupt, so it is ignored; delete it or set \
                 {RESET_RESTORE_TOKEN_ENV}=1 to start over",
                path.display()
            );
            None
        }
    }
}

/// Validates raw file content into a token.
///
/// Pure, so the corruption rules are unit-testable without a filesystem:
/// trimming makes a trailing newline (what [`save`] writes, and what an editor
/// adds) harmless, while an empty or control-character-bearing file is
/// rejected rather than shipped to the portal as a nonsense option value.
fn parse_stored_token(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_TOKEN_LEN {
        return None;
    }
    // Whitespace and control characters never appear in a portal token, so
    // their presence means the file is something else (JSON, a `key=value`
    // line, a truncated shell redirect).
    if trimmed
        .chars()
        .any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    Some(trimmed.to_string())
}

/// Stores `token`, creating the directory if needed.
///
/// The write goes to a temporary file in the same directory and is then
/// renamed over the target, so a crash mid-write cannot leave a half-written
/// token that would be rejected as corrupt on the next run. `rename` within
/// one directory is atomic.
///
/// Returns `Ok(())` even when there is nothing to store (an empty token), so
/// that a portal which declines to issue one does not produce a spurious
/// error on an otherwise successful capture.
pub fn save(token: &str) -> Result<(), String> {
    save_keyed(None, token)
}

/// [`save`] for a per-device/per-target token (see [`token_path_for`]).
pub fn save_keyed(key: Option<&str>, token: &str) -> Result<(), String> {
    let Some(path) = token_path_for(key) else {
        return Err(format!(
            "neither {RESTORE_TOKEN_PATH_ENV} nor {STATE_HOME_ENV} nor HOME is set, so there \
             is nowhere to store the screen-capture restore token"
        ));
    };
    save_to(&path, token)
}

/// [`save`] against an explicit path. See [`load_from`] for why.
fn save_to(path: &Path, token: &str) -> Result<(), String> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    if trimmed.len() > MAX_TOKEN_LEN {
        return Err(format!(
            "refusing to store a {}-byte restore token at {}; a portal token is never this long",
            trimmed.len(),
            path.display()
        ));
    }
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }

    // Set the mode on the temporary file *before* the token is written into
    // it: `fs::write` creates with 0666 & ~umask, which for a typical umask of
    // 022 is world-readable, i.e. briefly a world-readable capture credential.
    // `.tmp` is appended rather than replacing the extension: keyed token
    // files carry a dotted suffix that `with_extension` would eat, making two
    // keys share one temporary file.
    let tmp = with_key_suffix(path.to_path_buf(), Some("tmp"));
    let io = |what: &str, e: std::io::Error| format!("{what} {}: {e}", path.display());

    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(TOKEN_FILE_MODE)
            .open(&tmp)
            .map_err(|e| io("creating", e))?;
        // Trailing newline: the file stays cat/editor friendly.
        writeln!(file, "{trimmed}").map_err(|e| io("writing", e))?;
        file.sync_all().map_err(|e| io("flushing", e))?;
    }
    #[cfg(not(unix))]
    {
        fs::write(&tmp, format!("{trimmed}\n")).map_err(|e| io("writing", e))?;
    }

    // `set_permissions` is belt-and-braces for the non-atomic-mode path above
    // and documents the intent at the call site.
    set_mode(&tmp, TOKEN_FILE_MODE).map_err(|e| format!("setting mode on {}: {e}", path.display()))?;
    fs::rename(&tmp, path).map_err(|e| io("renaming into place", e))?;
    log::debug!("stored the screen-capture restore token at {}", path.display());
    Ok(())
}

/// Deletes the stored token, if any.
///
/// Idempotent: a missing file is success, so this can run unconditionally on
/// the reset path.
pub fn forget() -> Result<(), String> {
    forget_keyed(None)
}

/// [`forget`] for a per-device/per-target token (see [`token_path_for`]).
pub fn forget_keyed(key: Option<&str>) -> Result<(), String> {
    let Some(path) = token_path_for(key) else {
        return Ok(());
    };
    forget_from(&path)
}

/// [`forget`] against an explicit path. See [`load_from`] for why.
fn forget_from(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => {
            log::info!("deleted the screen-capture restore token at {}", path.display());
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("could not delete {}: {e}", path.display())),
    }
}

/// Whether [`RESET_RESTORE_TOKEN_ENV`] asks for the token to be forgotten.
///
/// Unset, empty, and whitespace-only all mean "no", following the same
/// "empty means unset" rule as `DISPLAYSWARM_CAPTURE_OUTPUT`. Anything else is
/// compared case-insensitively against the usual affirmative spellings so
/// `=true` and `=YES` behave like `=1`.
pub fn is_reset_requested() -> bool {
    parse_reset_flag(&std::env::var(RESET_RESTORE_TOKEN_ENV).unwrap_or_default())
}

/// Pure parser behind [`is_reset_requested`].
fn parse_reset_flag(raw: &str) -> bool {
    let value = raw.trim();
    if value.is_empty() {
        return false;
    }
    value.eq_ignore_ascii_case("1")
        || value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("yes")
        || value.eq_ignore_ascii_case("on")
}

/// Creates `dir` and its parents as `0700`, if it does not exist.
fn create_private_dir(dir: &Path) -> Result<(), String> {
    if dir.is_dir() {
        return Ok(());
    }
    // Pre-existing directory with the wrong mode is left alone: it may be
    // shared state the user set up deliberately, and it is not this file's
    // business to re-permission someone's directory.
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(STATE_DIR_MODE);
        builder
            .create(dir)
            .map_err(|e| format!("creating the state directory {}: {e}", dir.display()))?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
            .map_err(|e| format!("creating the state directory {}: {e}", dir.display()))?;
    }
    Ok(())
}

/// Applies an owner-only mode to `path`.
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| format!("setting the mode on {}: {e}", path.display()))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A private temp directory for one test.
    ///
    /// No `tempfile` dev-dependency, matching `crate::display::privilege`, but
    /// with a counter rather than only the pid: the test runner is
    /// multi-threaded and every test in here lives in the same process, so a
    /// pid-only name would have them fight over one directory.
    struct TempState(PathBuf);

    impl TempState {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("vmon-token-{label}-{}-{seq}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            Self(path)
        }

        /// The token file inside a state dir that does not exist yet.
        fn token_file(&self) -> PathBuf {
            self.0.join(APP_DIR_NAME).join(TOKEN_FILE_NAME)
        }
    }

    impl Drop for TempState {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// `save` then `load`, the whole point of the module.
    #[test]
    fn a_saved_token_round_trips() {
        let state = TempState::new("roundtrip");
        let path = state.token_file();

        assert_eq!(load_from(&path), None, "nothing stored yet");
        save_to(&path, "aBcD-1234_token").expect("save");
        assert_eq!(load_from(&path).as_deref(), Some("aBcD-1234_token"));
    }

    /// The first run, which must be silent and non-fatal.
    #[test]
    fn a_missing_file_is_not_an_error() {
        let state = TempState::new("missing");
        assert_eq!(load_from(&state.token_file()), None);
    }

    /// A zero-length file and a whitespace-only file are both "no token".
    #[test]
    fn empty_and_whitespace_only_files_are_ignored() {
        let state = TempState::new("empty");
        let path = state.token_file();

        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "").unwrap();
        assert_eq!(load_from(&path), None, "empty file");

        fs::write(&path, "   \n\t  \n").unwrap();
        assert_eq!(load_from(&path), None, "whitespace-only file");
    }

    /// The privilege claim: the token must not be readable by anyone else.
    #[test]
    fn the_token_file_is_owner_only() {
        let state = TempState::new("mode");
        let path = state.token_file();
        save_to(&path, "secret").expect("save");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, TOKEN_FILE_MODE,
                "a restore token authorises screen capture without a prompt; \
                 it must be 0600, not the 0644 a plain fs::write would leave"
            );
            assert_eq!(fs::metadata(&path).unwrap().len(), "secret\n".len() as u64);
        }
    }

    /// A directory that has to be created first (normal first-run case).
    #[test]
    fn saving_creates_the_state_directory() {
        let state = TempState::new("mkdir");
        let path = state.token_file();
        assert!(!path.parent().unwrap().exists());
        save_to(&path, "tok").expect("save must create its directory");
        assert!(path.is_file());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, STATE_DIR_MODE, "state directory must be 0700");
        }
    }

    /// Garbage in the file must not be forwarded to the portal.
    #[test]
    fn corrupt_content_is_rejected() {
        // Whitespace inside the value: not a token, whatever the bytes are.
        assert_eq!(parse_stored_token("abc def"), None);
        assert_eq!(parse_stored_token("abc\ndef"), None);
        // Control characters.
        assert_eq!(parse_stored_token("abc\u{0}def"), None);
        // Two concatenated records.
        assert_eq!(parse_stored_token("tok1\ntok2\n"), None);
        // Absurdly long.
        assert_eq!(parse_stored_token(&"a".repeat(MAX_TOKEN_LEN + 1)), None);
        // The long edge of valid: exactly at the limit.
        assert_eq!(parse_stored_token(&"a".repeat(MAX_TOKEN_LEN)).is_some(), true);
        // A trailing newline is what save() writes, so it must be accepted.
        assert_eq!(parse_stored_token("tok\n").as_deref(), Some("tok"));
        // As is a leading one, from a careless editor.
        assert_eq!(parse_stored_token(" tok \r\n").as_deref(), Some("tok"));
    }

    /// Non-UTF-8 bytes are a read error, which must degrade, not panic.
    #[test]
    fn invalid_utf8_degrades_to_no_token() {
        let state = TempState::new("badutf8");
        let path = state.token_file();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, [0x66, 0xff, 0xfe, 0x6f]).unwrap();
        assert_eq!(load_from(&path), None);
    }

    /// A directory where the file should be is also just "no token".
    #[test]
    fn an_unreadable_path_degrades_to_no_token() {
        let state = TempState::new("isdir");
        let path = state.token_file();
        // Put a directory at the token's path: read_to_string fails with a
        // non-NotFound error.
        fs::create_dir_all(&path).unwrap();
        assert_eq!(load_from(&path), None);
    }

    /// Overwriting must not leave a stale token behind, since the portal
    /// invalidates a token after one use.
    #[test]
    fn a_second_save_replaces_the_first() {
        let state = TempState::new("replace");
        let path = state.token_file();
        save_to(&path, "first").unwrap();
        save_to(&path, "second").unwrap();
        assert_eq!(load_from(&path).as_deref(), Some("second"));
        // The temporary file used for the atomic rename must be gone.
        assert!(!path.with_extension("tmp").exists());
    }

    /// Storing nothing must not be an error and must not clobber a token.
    #[test]
    fn an_empty_token_is_a_no_op() {
        let state = TempState::new("emptysave");
        let path = state.token_file();
        save_to(&path, "keep-me").unwrap();
        assert!(save_to(&path, "   ").is_ok());
        assert_eq!(load_from(&path).as_deref(), Some("keep-me"));
    }

    /// Deleting is idempotent, because it runs on the reset path where the
    /// file may not exist.
    #[test]
    fn forgetting_is_idempotent() {
        let state = TempState::new("forget");
        let path = state.token_file();
        assert!(fs::remove_file(&path).is_err(), "precondition: no file yet");
        assert!(forget_from(&path).is_ok(), "missing file is success");
        save_to(&path, "tok").unwrap();
        assert!(forget_from(&path).is_ok());
        assert_eq!(load_from(&path), None);
        assert!(forget_from(&path).is_ok(), "second delete is success");
    }

    #[test]
    fn a_token_longer_than_the_limit_is_refused_outright() {
        let state = TempState::new("toolong");
        let path = state.token_file();
        let err = save_to(&path, &"a".repeat(MAX_TOKEN_LEN + 1)).unwrap_err();
        assert!(err.contains("never this long"), "{err}");
        assert!(!path.exists(), "nothing must be written");
    }

    #[test]
    fn reset_flag_parsing() {
        for truthy in ["1", "true", "TRUE", "yes", "YES", "on", " 1 "] {
            assert!(parse_reset_flag(truthy), "{truthy:?} should be truthy");
        }
        for falsy in ["", "  ", "0", "false", "no", "off", "2", "maybe"] {
            assert!(!parse_reset_flag(falsy), "{falsy:?} should be falsy");
        }
    }

    #[test]
    fn keys_are_sanitised_for_the_filesystem() {
        assert_eq!(sanitize_key("dev1/monitor"), "dev1.monitor");
        assert_eq!(sanitize_key("a-b_c"), "a-b_c");
        // No separator can survive, so a key cannot leave the state directory.
        let evil = sanitize_key("../../etc/passwd\0 \\x");
        assert!(!evil.contains('/') && !evil.contains('\\') && !evil.contains('\0'), "{evil}");
        assert_eq!(sanitize_key("é"), "_");
        assert_eq!(sanitize_key(""), "key");
        assert_eq!(sanitize_key("/"), "key.");
        assert_eq!(sanitize_key(&"a".repeat(500)).len(), MAX_KEY_LEN);
    }

    #[test]
    fn keyed_paths_differ_and_unkeyed_is_legacy() {
        let base = PathBuf::from("/x/displayswarm/restore_token");
        assert_eq!(with_key_suffix(base.clone(), None), base);
        assert_eq!(
            with_key_suffix(base.clone(), Some("dev1.monitor")),
            PathBuf::from("/x/displayswarm/restore_token.dev1.monitor")
        );
        assert_ne!(
            with_key_suffix(base.clone(), Some("dev1.monitor")),
            with_key_suffix(base, Some("dev1.virtual"))
        );
    }

    #[test]
    fn keyed_tokens_are_independent_and_owner_only() {
        let state = TempState::new("keyed");
        let base = state.token_file();
        let a = with_key_suffix(base.clone(), Some("dev1.monitor"));
        let b = with_key_suffix(base.clone(), Some("dev1.virtual"));
        save_to(&a, "tok-a").unwrap();
        save_to(&b, "tok-b").unwrap();
        save_to(&base, "legacy").unwrap();
        assert_eq!(load_from(&a).as_deref(), Some("tok-a"));
        assert_eq!(load_from(&b).as_deref(), Some("tok-b"));
        assert_eq!(load_from(&base).as_deref(), Some("legacy"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&a).unwrap().permissions().mode() & 0o777, TOKEN_FILE_MODE);
        }
        forget_from(&a).unwrap();
        assert_eq!(load_from(&a), None);
        assert_eq!(load_from(&b).as_deref(), Some("tok-b"));
    }

    /// The path contract, checked without touching the environment.
    #[test]
    fn token_paths_are_under_a_displayswarm_directory() {
        let state = TempState::new("path");
        let token_file = state.token_file();
        let parent = token_file.parent().unwrap();
        assert_eq!(parent.file_name().unwrap(), APP_DIR_NAME);
        assert_eq!(token_file.file_name().unwrap(), TOKEN_FILE_NAME);
    }
}
