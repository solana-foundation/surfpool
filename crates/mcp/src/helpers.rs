use std::net::TcpListener;

use surfpool_types::DEFAULT_RPC_PORT;

pub fn is_port_available(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn surfnet_port_pair(surfnet_id: u16) -> Option<(u16, u16)> {
    let port = DEFAULT_RPC_PORT.checked_add(surfnet_id.checked_mul(10_000)?)?;
    let ws_port = port.checked_sub(9)?;
    Some((port, ws_port))
}

pub fn find_next_available_surfnet_port() -> Result<(u16, u16), String> {
    let mut surfnet_id = 0;
    while let Some((port, ws_port)) = surfnet_port_pair(surfnet_id) {
        if is_port_available(port) && is_port_available(ws_port) {
            return Ok((surfnet_id, port));
        }
        let Some(next_id) = surfnet_id.checked_add(1) else {
            break;
        };
        surfnet_id = next_id;
    }
    Err("No available surfnet ports of format 127.0.0.1:x8899 found.".to_string())
}

#[cfg(test)]
mod tests {
    use super::surfnet_port_pair;
    use surfpool_types::DEFAULT_RPC_PORT;

    #[test]
    fn port_pair_scan_stops_before_port_arithmetic_overflows() {
        assert_eq!(
            surfnet_port_pair(0),
            Some((DEFAULT_RPC_PORT, DEFAULT_RPC_PORT - 9))
        );
        assert!(surfnet_port_pair(u16::MAX).is_none());
        assert!(surfnet_port_pair(6).is_none());
    }
}
