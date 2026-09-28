//! xdg-desktop-portal ScreenCast. The first start shows the desktop dialog;
//! a restore token skips it the next time. Tokens never reach logs.

use std::collections::HashMap;
use std::fs;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use golive_platform::{PlatformError, SourceKind};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedFd as ZFd, OwnedObjectPath, OwnedValue, Value};

pub const PERMISSION_HINT: &str = "Sem permissão de captura — confirme a tela no diálogo do sistema e tente de novo.";

const DEST: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const PORTAL_WAIT: Duration = Duration::from_secs(120);

pub struct PortalCast {
    fd: OwnedFd,
    node_id: u32,
    closer: SessionCloser,
}

struct SessionCloser {
    connection: Connection,
    session: OwnedObjectPath,
}

impl PortalCast {
    pub fn into_parts(self) -> (OwnedFd, u32, impl Sized) {
        (self.fd, self.node_id, self.closer)
    }
}

impl Drop for SessionCloser {
    fn drop(&mut self) {
        let Ok(proxy) = proxy(&self.connection, self.session.as_str(), "org.freedesktop.portal.Session") else {
            return;
        };
        let _ = proxy.call_method("Close", &());
    }
}

pub fn open_cast(kind: SourceKind, source_id: &str) -> Result<PortalCast, PlatformError> {
    let connection = Connection::session().map_err(|_| {
        PlatformError::Internal("sem barramento de sessão".into())
    })?;
    let unique = connection
        .unique_name()
        .map(|name| name.as_str().to_owned())
        .ok_or_else(|| PlatformError::Internal("portal sem nome único".into()))?;
    let sender = sender_token(&unique);

    let session_token = fresh_token("sess");
    let create_token = fresh_token("req");
    let create_path = request_path(&sender, &create_token);
    let session_for_create = session_token.clone();
    let handle_for_create = create_token.clone();
    let created = wait_response(&connection, &create_path, move |conn| {
        let screencast = Proxy::new(conn, DEST, PORTAL_PATH, "org.freedesktop.portal.ScreenCast")?;
        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("handle_token", Value::from(handle_for_create.as_str()));
        opts.insert("session_handle_token", Value::from(session_for_create.as_str()));
        screencast.call_method("CreateSession", &opts)
    })?;
    let session = session_path_from(&created, &sender, &session_token)?;

    let select_token = fresh_token("req");
    let select_path = request_path(&sender, &select_token);
    let session_for_select = session.clone();
    let handle_for_select = select_token.clone();
    let saved = load_token(source_id);
    let types = source_types(kind);
    wait_response(&connection, &select_path, move |conn| {
        let screencast = Proxy::new(conn, DEST, PORTAL_PATH, "org.freedesktop.portal.ScreenCast")?;
        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("handle_token", Value::from(handle_for_select.as_str()));
        opts.insert("types", Value::U32(types));
        opts.insert("multiple", Value::Bool(false));
        opts.insert("cursor_mode", Value::U32(2));
        opts.insert("persist_mode", Value::U32(2));
        if let Some(token) = saved.as_deref() {
            opts.insert("restore_token", Value::from(token));
        }
        screencast.call_method("SelectSources", &(session_for_select, opts))
    })?;

    let start_token = fresh_token("req");
    let start_path = request_path(&sender, &start_token);
    let session_for_start = session.clone();
    let handle_for_start = start_token.clone();
    let started = wait_response(&connection, &start_path, move |conn| {
        let screencast = Proxy::new(conn, DEST, PORTAL_PATH, "org.freedesktop.portal.ScreenCast")?;
        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("handle_token", Value::from(handle_for_start.as_str()));
        screencast.call_method("Start", &(session_for_start, "", opts))
    })?;
    if let Some(token) = string_field(&started, "restore_token") {
        let _ = save_token(source_id, &token);
    }
    let (node_id, _props) = first_stream(&started)?;
    let remote: ZFd = {
        let screencast = proxy(&connection, PORTAL_PATH, "org.freedesktop.portal.ScreenCast")?;
        screencast
            .call("OpenPipeWireRemote", &(session.clone(), HashMap::<&str, Value<'_>>::new()))
            .map_err(|_| PlatformError::Internal("portal não abriu o PipeWire".into()))?
    };
    let fd: OwnedFd = remote.into();
    Ok(PortalCast {
        fd,
        node_id,
        closer: SessionCloser { connection, session },
    })
}

fn source_types(kind: SourceKind) -> u32 {
    match kind {
        SourceKind::Display => 1,
        SourceKind::Window => 2,
    }
}

fn proxy<'a>(
    connection: &'a Connection,
    path: &'a str,
    interface: &'a str,
) -> Result<Proxy<'a>, PlatformError> {
    Proxy::new(connection, DEST, path, interface)
        .map_err(|_| PlatformError::Internal("portal indisponível".into()))
}

fn wait_response(
    connection: &Connection,
    request_path: &str,
    call: impl FnOnce(&Connection) -> zbus::Result<zbus::message::Message> + Send + 'static,
) -> Result<HashMap<String, OwnedValue>, PlatformError> {
    let connection = connection.clone();
    let request_path = request_path.to_owned();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = (|| {
            let request = proxy(&connection, &request_path, "org.freedesktop.portal.Request")?;
            let mut signals = request
                .receive_signal("Response")
                .map_err(|_| PlatformError::Internal("portal sem resposta".into()))?;
            call(&connection).map_err(|_| PlatformError::Internal("chamada ao portal falhou".into()))?;
            let message = signals
                .next()
                .ok_or_else(|| PlatformError::Internal("portal encerrou a espera".into()))?;
            let (code, results): (u32, HashMap<String, OwnedValue>) = message
                .body()
                .deserialize()
                .map_err(|_| PlatformError::Internal("resposta do portal ilegível".into()))?;
            match code {
                0 => Ok(results),
                1 => Err(PlatformError::PermissionDenied { hint: PERMISSION_HINT }),
                _ => Err(PlatformError::Internal("portal recusou a captura".into())),
            }
        })();
        let _ = tx.send(result);
    });
    rx.recv_timeout(PORTAL_WAIT)
        .map_err(|_| PlatformError::Internal("portal excedeu o tempo".into()))?
}

fn session_path_from(
    results: &HashMap<String, OwnedValue>,
    sender: &str,
    token: &str,
) -> Result<OwnedObjectPath, PlatformError> {
    if let Some(path) = string_field(results, "session_handle") {
        if let Ok(path) = OwnedObjectPath::try_from(path) {
            return Ok(path);
        }
    }
    let path = format!("/org/freedesktop/portal/desktop/session/{sender}/{token}");
    OwnedObjectPath::try_from(path).map_err(|_| PlatformError::Internal("sessão do portal inválida".into()))
}

fn first_stream(
    results: &HashMap<String, OwnedValue>,
) -> Result<(u32, HashMap<String, OwnedValue>), PlatformError> {
    let Some(raw) = results.get("streams") else {
        return Err(PlatformError::Internal("portal não devolveu stream".into()));
    };
    let owned = raw
        .try_clone()
        .map_err(|_| PlatformError::Internal("portal não devolveu stream".into()))?;
    let streams = Vec::<(u32, HashMap<String, OwnedValue>)>::try_from(owned)
        .map_err(|_| PlatformError::Internal("portal não devolveu stream".into()))?;
    streams
        .into_iter()
        .next()
        .ok_or_else(|| PlatformError::Internal("portal não devolveu stream".into()))
}

fn string_field(results: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    let value = results.get(key)?;
    if let Ok(text) = <&str>::try_from(value) {
        return Some(text.to_owned());
    }
    if let Ok(path) = <&zbus::zvariant::ObjectPath>::try_from(value) {
        return Some(path.to_string());
    }
    None
}

pub fn sender_token(unique_name: &str) -> String {
    unique_name.trim_start_matches(':').replace('.', "_")
}

fn request_path(sender: &str, token: &str) -> String {
    format!("/org/freedesktop/portal/desktop/request/{sender}/{token}")
}

fn fresh_token(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!(
        "{prefix}{}{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn token_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    Some(base.join("goDrinking").join("screencast-tokens.json"))
}

fn load_token(source_id: &str) -> Option<String> {
    let path = token_path()?;
    let text = fs::read_to_string(path).ok()?;
    let map: HashMap<String, String> = serde_json::from_str(&text).ok()?;
    map.get(source_id).cloned()
}

fn save_token(source_id: &str, token: &str) -> Result<(), ()> {
    let path = token_path().ok_or(())?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|_| ())?;
    }
    let mut map: HashMap<String, String> = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    map.insert(source_id.to_owned(), token.to_owned());
    let text = serde_json::to_string(&map).map_err(|_| ())?;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| ())?;
    use std::io::Write;
    file.write_all(text.as_bytes()).map_err(|_| ())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::sender_token;

    #[test]
    fn sender_token_is_a_dbus_path_element() {
        assert_eq!(sender_token(":1.42"), "1_42");
        assert!(!sender_token(":1.42").contains('.'));
        assert!(!sender_token(":1.42").contains(':'));
    }
}
