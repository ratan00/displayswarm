//! "Choose files" for the window: the XDG desktop portal's FileChooser on
//! Linux (every desktop, sandboxed or not), the common Open dialog on Windows.
//! Blocking; call it off the UI thread. `Ok(vec![])` means the user cancelled.

use std::path::PathBuf;

/// A `file://` URI as a path (percent-decoded); `None` for any other scheme.
pub fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file://host/path`: only the local host (empty or "localhost").
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    if !path.starts_with('/') {
        return None;
    }
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(v) = hex {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
    }
    #[cfg(not(unix))]
    {
        Some(PathBuf::from(String::from_utf8_lossy(&out).into_owned()))
    }
}

/// The paths in a Windows multi-select result: `dir\0name\0name\0\0`, or a
/// single full path when one file was chosen.
pub fn parse_multiselect(buf: &[u16]) -> Vec<PathBuf> {
    let parts: Vec<String> = buf
        .split(|&c| c == 0)
        .take_while(|p| !p.is_empty())
        .map(String::from_utf16_lossy)
        .collect();
    match parts.as_slice() {
        [] => Vec::new(),
        [one] => vec![PathBuf::from(one)],
        [dir, names @ ..] => names.iter().map(|n| PathBuf::from(dir).join(n)).collect(),
    }
}

/// Asks the user for files to send; empty when cancelled.
pub fn choose_files(title: &str) -> Result<Vec<PathBuf>, String> {
    #[cfg(target_os = "linux")]
    {
        return portal::open_files(title);
    }
    #[cfg(windows)]
    {
        return win::open_files(title);
    }
    #[allow(unreachable_code)]
    {
        let _ = title;
        Err("no file dialog on this platform".into())
    }
}

#[cfg(target_os = "linux")]
mod portal {
    use super::*;
    use futures_util::StreamExt;
    use std::collections::HashMap;
    use zbus::zvariant::{OwnedValue, Value};

    pub fn open_files(title: &str) -> Result<Vec<PathBuf>, String> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
        rt.block_on(open_files_async(title)).map_err(|e| format!("file chooser: {e}"))
    }

    async fn open_files_async(title: &str) -> zbus::Result<Vec<PathBuf>> {
        let conn = zbus::Connection::session().await?;
        let token = format!("displayswarm{}", std::process::id());
        let sender = conn.unique_name().map(|n| n.as_str().trim_start_matches(':').replace('.', "_")).unwrap_or_default();
        let request_path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
        // Listen before calling, or a fast answer is missed.
        let request = zbus::Proxy::new(&conn, "org.freedesktop.portal.Desktop", request_path, "org.freedesktop.portal.Request").await?;
        let mut responses = request.receive_signal("Response").await?;
        let chooser = zbus::Proxy::new(
            &conn,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.FileChooser",
        )
        .await?;
        let mut options: HashMap<&str, Value> = HashMap::new();
        options.insert("handle_token", Value::from(token.as_str()));
        options.insert("multiple", Value::from(true));
        options.insert("modal", Value::from(true));
        let _: zbus::zvariant::OwnedObjectPath = chooser.call("OpenFile", &("", title, options)).await?;
        let Some(msg) = responses.next().await else { return Ok(Vec::new()) };
        let (code, results): (u32, HashMap<String, OwnedValue>) = msg.body().deserialize()?;
        if code != 0 {
            return Ok(Vec::new()); // cancelled
        }
        let uris: Vec<String> = match results.get("uris") {
            Some(v) => Vec::<String>::try_from(v.try_clone()?).unwrap_or_default(),
            None => Vec::new(),
        };
        Ok(uris.iter().filter_map(|u| file_uri_to_path(u)).collect())
    }
}

#[cfg(windows)]
mod win {
    use super::*;
    use ::windows::core::PCWSTR;
    use ::windows::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OFN_ALLOWMULTISELECT, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };

    pub fn open_files(title: &str) -> Result<Vec<PathBuf>, String> {
        let mut buf = vec![0u16; 64 * 1024];
        let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        let mut ofn = OPENFILENAMEW {
            lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
            lpstrFile: ::windows::core::PWSTR(buf.as_mut_ptr()),
            nMaxFile: buf.len() as u32,
            lpstrTitle: PCWSTR(title.as_ptr()),
            Flags: OFN_ALLOWMULTISELECT | OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST,
            ..Default::default()
        };
        if unsafe { GetOpenFileNameW(&mut ofn) }.as_bool() {
            Ok(parse_multiselect(&buf))
        } else {
            Ok(Vec::new()) // cancelled (or an error the dialog already showed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_become_paths() {
        assert_eq!(file_uri_to_path("file:///home/amy/My%20Doc.pdf"), Some(PathBuf::from("/home/amy/My Doc.pdf")));
        assert_eq!(file_uri_to_path("file://localhost/tmp/a"), Some(PathBuf::from("/tmp/a")));
        assert_eq!(file_uri_to_path("file:///t%C3%A9l%C3%A9"), Some(PathBuf::from("/t\u{e9}l\u{e9}")));
        assert_eq!(file_uri_to_path("https://example.com/a"), None);
        assert_eq!(file_uri_to_path("file://otherhost/a"), None);
        assert_eq!(file_uri_to_path("file:///100%"), Some(PathBuf::from("/100%")), "a stray % stays");
    }

    #[test]
    fn windows_multiselect_results_are_split() {
        let w = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        let one = [w(r"C:\a\b.txt"), vec![0, 0]].concat();
        assert_eq!(parse_multiselect(&one), vec![PathBuf::from(r"C:\a\b.txt")]);
        let many = [w(r"C:\dir"), vec![0], w("x.png"), vec![0], w("y.pdf"), vec![0, 0]].concat();
        assert_eq!(parse_multiselect(&many), vec![PathBuf::from(r"C:\dir").join("x.png"), PathBuf::from(r"C:\dir").join("y.pdf")]);
        assert!(parse_multiselect(&[0, 0]).is_empty());
    }
}
