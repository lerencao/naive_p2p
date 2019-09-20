use clap::{value_t, App, Arg, ArgMatches};
use log::info;
use naive_p2p::config::NodeConfig;
use naive_p2p::{api, node};
use tokio::runtime::Runtime;

fn main() {
    let matches = App::new("naive p2p")
        .version("1.0")
        .author("lerencao")
        .arg(
            Arg::with_name("p2p-addr")
                .long("p2p_addr")
                .value_name("ADDR")
                .help("p2p node addr")
                .default_value("127.0.0.1:18080"),
        )
        .arg(
            Arg::with_name("p2p-id")
                .long("peer_id")
                .value_name("PEER_ID")
                .help("set peer uniq id")
                .required(true),
        )
        .arg(
            Arg::with_name("http-addr")
                .value_name("http-addr")
                .long("http_addr")
                .help("set http addr")
                .default_value("127.0.0.1:8080"),
        )
        .arg(
            Arg::with_name("config")
                .value_name("CONFIG-FILE")
                .help("load config from file")
                .long("config"),
        )
        .arg(
            Arg::with_name("v")
                .short("v")
                .multiple(true)
                .help("set the level of verbosity"),
        )
        .get_matches();
    info!("match: {:?}", &matches);

    let log_level = match matches.occurrences_of("v") {
        0 => "info",
        1 => "debug",
        2 | _ => "trace",
    };
    std::env::set_var("RUST_LOG", log_level);
    pretty_env_logger::init();

    let node_config = load_config(&matches);
    let http_addr = node_config.http_addr.parse().unwrap();

    let rt = Runtime::new().unwrap();
    let (mut node, node_client) = node::P2PNode::new(node_config);
    let node_fut = node.start(rt.executor().clone());

    // TODO: use the graceful shutdown
    let (_graceful_shutdown_tx, graceful_shutdown_rx) = futures::channel::oneshot::channel();

    let server = hyper::Server::bind(&http_addr);

    let http_api_fut = api::run_http_server(server, node_client.clone(), graceful_shutdown_rx);
    rt.spawn(http_api_fut);

    rt.block_on(node_fut);
}

pub fn load_config(args: &ArgMatches<'_>) -> NodeConfig {
    let p2p_addr = value_t!(args, "p2p_addr", String).ok();
    let http_addr = value_t!(args, "http_addr", String).ok();
    let peer_id = value_t!(args, "peer_id", String).ok();
    let config_path = value_t!(args, "config", String).ok();

    let mut config = match config_path {
        Some(path) => {
            let mut settings = config::Config::default();
            settings
                .merge(config::File::with_name(&path))
                .expect("merge config file should be ok");
            settings.try_into::<NodeConfig>().unwrap()
        }
        None => NodeConfig::default(),
    };
    if p2p_addr.is_some() {
        config.p2p_addr = p2p_addr.unwrap();
    }
    if http_addr.is_some() {
        config.http_addr = http_addr.unwrap();
    }
    if peer_id.is_some() {
        config.local_peer_id = peer_id.unwrap();
    }

    config
}
