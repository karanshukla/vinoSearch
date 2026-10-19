//! A folder chooser through xdg-desktop-portal's FileChooser, so the dialog is
//! the desktop's own (Dolphin's on Plasma, Nautilus's on GNOME) and egui needs
//! no file dialog of its own.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedValue, Value};

const PORTAL: &str = "org.freedesktop.portal.Desktop";

/// Blocks until the dialog closes. None when it was cancelled.
pub fn pick_folder(title: &str) -> Result<Option<PathBuf>> {
    let conn = Connection::session()?;
    // The portal answers on a Request object whose path it derives from our
    // bus name and this token. Listening there before asking means a fast
    // answer can't arrive ahead of the match rule.
    let token = format!("vinosearch{}", std::process::id());
    let sender = conn
        .unique_name()
        .context("no unique bus name")?
        .trim_start_matches(':')
        .replace('.', "_");
    let request = Proxy::new(
        &conn,
        PORTAL,
        format!("/org/freedesktop/portal/desktop/request/{sender}/{token}"),
        "org.freedesktop.portal.Request",
    )?;
    let mut responses = request.receive_signal("Response")?;
    let chooser = Proxy::new(
        &conn,
        PORTAL,
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.FileChooser",
    )?;
    let options: HashMap<&str, Value> = HashMap::from([
        ("handle_token", Value::from(token.as_str())),
        ("directory", Value::from(true)),
        ("modal", Value::from(true)),
    ]);
    let _: zbus::zvariant::OwnedObjectPath = chooser
        .call("OpenFile", &("", title, options))
        .context("the desktop has no file chooser portal")?;
    let message = responses.next().context("the portal went away")?;
    let (code, results): (u32, HashMap<String, OwnedValue>) = message.body().deserialize()?;
    if code != 0 {
        return Ok(None);
    }
    let uri = match results.get("uris").map(|v| &**v) {
        Some(Value::Array(a)) => a.iter().find_map(|v| match v {
            Value::Str(s) => Some(s.to_string()),
            _ => None,
        }),
        _ => None,
    };
    Ok(uri.as_deref().and_then(file_uri_path))
}

/// `file:///a%20b` as `/a b`. Anything but a local file URI is None.
fn file_uri_path(uri: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(b) = bytes
                .get(i + 1..i + 3)
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_decode_to_paths() {
        assert_eq!(
            file_uri_path("file:///home/u/My%20Notes"),
            Some(PathBuf::from("/home/u/My Notes"))
        );
        assert_eq!(
            file_uri_path("file://localhost/tmp/%C3%A9t%C3%A9"),
            Some(PathBuf::from("/tmp/été"))
        );
        assert_eq!(
            file_uri_path("file:///a/100%"),
            Some(PathBuf::from("/a/100%"))
        );
        assert_eq!(file_uri_path("https://example.com/x"), None);
        assert_eq!(file_uri_path("file://host/x"), None);
    }
}
