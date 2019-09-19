
use naive_p2p::{api, p2p};




use tokio::runtime::Runtime;
use log::{info, error};
fn main() {
    let rt = Runtime::new().unwrap();
    let config = p2p::NodeConfig {
        port: 18888,
        addr: "127.0.0.1".to_string(),
        local_peer_id: "0".to_string(),
        max_inbounds: 10,
        max_outbounds: 10,
    };
    let (mut node, node_client) = p2p::P2PNode::new(config, vec![]);
    let node_fut = node.start();

    let http_addr = "127.0.0.1:8080".parse().unwrap();
    let http_api_fut = api::run_http_server(http_addr, node_client.clone());
    rt.spawn(async move {
        let result = http_api_fut.await;
        match result {
            Ok(_) => {
                info!("http stopped");
            },
            Err(e) => {
                error!("http error: {:?}", e);
            }
        };
    });
    rt.block_on(node_fut);
}
