use serde::{Serialize, Deserialize};

#[derive(Debug, Deserialize)]
pub struct P2pConfig {
    p2p_addr: String,
    p2p_port: u16,
    p2p_node_id: String,
    http_addr: String,
    http_port: u16,
}

pub fn load_config(path: &str) -> P2pConfig {
    let mut c = config::Config::new();

    c.merge(config::File::with_name(path)).unwrap();
    c.try_into::<P2pConfig>().unwrap()
}
