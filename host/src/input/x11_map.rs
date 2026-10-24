//! X11: maps the virtual touch and pen devices onto one monitor through the
//! "Coordinate Transformation Matrix" device property. Cinnamon's settings
//! daemon (and others) pin unmapped touchscreens to the primary output, which
//! sends the phone's touches to the wrong monitor.

use x11rb::connection::Connection;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode};

use crate::display::OutputRegion;

const MATRIX_PROPERTY: &str = "Coordinate Transformation Matrix";

/// The row-major matrix that sends the device's 0..1 range onto `region`
/// within the root window `bounds`. `None` is the identity (whole root).
pub fn matrix(region: Option<(OutputRegion, OutputRegion)>) -> [f32; 9] {
    let Some((r, b)) = region else {
        return [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
    };
    let (bw, bh) = (b.width.max(1) as f32, b.height.max(1) as f32);
    [
        r.width as f32 / bw,
        0.0,
        (r.x - b.x) as f32 / bw,
        0.0,
        r.height as f32 / bh,
        (r.y - b.y) as f32 / bh,
        0.0,
        0.0,
        1.0,
    ]
}

/// Sets the matrix on every X pointer device (attached or floating) whose
/// "Device Node" is one of `nodes` (`eventN` names). Returns the nodes X has
/// devices for; one missing is not registered with X yet (a libinput tablet
/// only appears on the pen's first proximity), so the caller retries.
pub fn set_matrix(nodes: &[String], m: [f32; 9]) -> Result<Vec<String>, String> {
    let (conn, _) = x11rb::connect(None).map_err(|e| format!("X11 connect: {e}"))?;
    let intern = |name: &str| -> Result<u32, String> {
        let r = conn.intern_atom(false, name.as_bytes()).map_err(|e| e.to_string())?;
        Ok(r.reply().map_err(|e| e.to_string())?.atom)
    };
    let property = intern(MATRIX_PROPERTY)?;
    let float = intern("FLOAT")?;
    let device_node = intern("Device Node")?;
    let devices = conn
        .xinput_xi_query_device(u16::from(xinput::Device::ALL))
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?;
    let words: Vec<u32> = m.iter().map(|f| f.to_bits()).collect();
    let mut found = Vec::new();
    let pointer = |t| t == xinput::DeviceType::SLAVE_POINTER || t == xinput::DeviceType::FLOATING_SLAVE;
    for info in devices.infos.iter().filter(|i| pointer(i.type_)) {
        let Some(node) = node_of(&conn, info.deviceid, device_node) else { continue };
        if !nodes.contains(&node) {
            continue;
        }
        conn.xinput_xi_change_property(
            info.deviceid,
            PropMode::REPLACE,
            property,
            float,
            words.len() as u32,
            &xinput::XIChangePropertyAux::Data32(words.clone()),
        )
        .map_err(|e| e.to_string())?
        .check()
        .map_err(|e| e.to_string())?;
        if !found.contains(&node) {
            found.push(node);
        }
    }
    conn.flush().map_err(|e| e.to_string())?;
    Ok(found)
}

/// The `eventN` name of the device's "Device Node" property
/// (`/dev/input/eventN`), if it has one.
fn node_of(conn: &impl Connection, deviceid: u16, device_node: u32) -> Option<String> {
    let reply = conn
        .xinput_xi_get_property(deviceid, false, device_node, u32::from(AtomEnum::ANY), 0, 64)
        .ok()?
        .reply()
        .ok()?;
    let xinput::XIGetPropertyItems::Data8(bytes) = reply.items else { return None };
    let path = std::str::from_utf8(&bytes).ok()?.trim_end_matches('\0');
    path.rsplit('/').next().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn r(x: i32, y: i32, w: u32, h: u32) -> OutputRegion {
        OutputRegion { x, y, width: w, height: h }
    }

    #[test]
    fn identity_without_a_region() {
        assert_eq!(matrix(None), [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn maps_the_right_hand_monitor() {
        let m = matrix(Some((r(1920, 0, 2000, 1200), r(0, 0, 3920, 1200))));
        assert!((m[0] - 2000.0 / 3920.0).abs() < 1e-6);
        assert!((m[2] - 1920.0 / 3920.0).abs() < 1e-6);
        assert_eq!((m[4], m[5]), (1.0, 0.0));
    }
}
