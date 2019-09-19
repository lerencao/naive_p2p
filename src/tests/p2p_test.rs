use crate::p2p::{NodeConfig, P2PNode};
use tokio::runtime::Runtime;

fn fake_config() -> NodeConfig {
    NodeConfig {
        port: 10888,
        local_peer_id: "test".to_string(),
        addr: "127.0.0.1".to_string(),
        max_inbounds: 10,
        max_outbounds: 10,
    }
}

#[test]
pub fn test_p2p_start() {
    let rt = Runtime::new().unwrap();
    let cfg = fake_config();
    let (mut node, node_client) = P2PNode::new(cfg, vec![]);
    let f = async move {
        node.start().await;
    };
    //    rt.spawn( move || async {
    //        node.start().await;
    //    });
    rt.block_on(f);
}
