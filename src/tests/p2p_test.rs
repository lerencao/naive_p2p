use crate::config::NodeConfig;
use crate::node::P2PNode;
use tokio::runtime::Runtime;

fn fake_config() -> NodeConfig {
    NodeConfig::default()
}

#[test]
pub fn test_p2p_start() {
    let rt = Runtime::new().unwrap();
    let cfg = fake_config();
    let (mut node, _node_client) = P2PNode::new(cfg);
    let executor = rt.executor().clone();
    let f = async move {
        node.start(executor).await;
    };
    //    rt.spawn( move || async {
    //        node.start().await;
    //    });
    rt.block_on(f);
}
