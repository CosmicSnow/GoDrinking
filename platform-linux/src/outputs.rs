//! Monitor list from `wl_output` v4 (connector name + current mode). No capture.

use golive_platform::{PlatformError, SourceInfo, SourceKind};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};

struct OutputAccum {
    id: wayland_client::backend::ObjectId,
    name: String,
    description: String,
    w: i32,
    h: i32,
}

struct State {
    outputs: Vec<OutputAccum>,
    proxies: Vec<wl_output::WlOutput>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global { name, interface, version } = event else {
            return;
        };
        if interface != "wl_output" {
            return;
        }
        let version = version.min(4);
        let output = registry.bind::<wl_output::WlOutput, _, _>(name, version, qh, ());
        state.outputs.push(OutputAccum {
            id: output.id(),
            name: String::new(),
            description: String::new(),
            w: 0,
            h: 0,
        });
        state.proxies.push(output);
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(slot) = state.outputs.iter_mut().find(|item| item.id == output.id()) else {
            return;
        };
        match event {
            wl_output::Event::Mode { flags, width, height, .. } => {
                if let WEnum::Value(flags) = flags {
                    if flags.contains(wl_output::Mode::Current) {
                        slot.w = width;
                        slot.h = height;
                    }
                }
            }
            wl_output::Event::Name { name } => slot.name = name,
            wl_output::Event::Description { description } => slot.description = description,
            _ => {}
        }
    }
}

impl Dispatch<wayland_client::protocol::wl_callback::WlCallback, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wayland_client::protocol::wl_callback::WlCallback,
        _event: wayland_client::protocol::wl_callback::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

pub fn list_displays() -> Result<Vec<SourceInfo>, PlatformError> {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err(PlatformError::Internal("sem WAYLAND_DISPLAY".into()));
    }
    let conn = Connection::connect_to_env().map_err(|_| {
        PlatformError::Internal("não conectou ao compositor Wayland".into())
    })?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = State { outputs: Vec::new(), proxies: Vec::new() };
    queue.roundtrip(&mut state).map_err(|_| {
        PlatformError::Internal("falha ao listar monitores".into())
    })?;
    queue.roundtrip(&mut state).map_err(|_| {
        PlatformError::Internal("falha ao listar monitores".into())
    })?;
    let _ = state.proxies;
    let mut infos = Vec::new();
    for output in state.outputs {
        if output.w <= 0 || output.h <= 0 {
            continue;
        }
        let id = if output.name.is_empty() {
            format!("output-{}", output.id.protocol_id())
        } else {
            output.name.clone()
        };
        let label = if output.description.is_empty() {
            id.clone()
        } else {
            output.description
        };
        infos.push(SourceInfo {
            kind: SourceKind::Display,
            id,
            name: format!("{label} · {}x{}", output.w, output.h),
            w: output.w as u32,
            h: output.h as u32,
        });
    }
    if infos.is_empty() {
        return Err(PlatformError::Internal("nenhum monitor Wayland".into()));
    }
    Ok(infos)
}
