use futures::TryStreamExt;
use hyper::{
    service::{make_service_fn, service_fn},
    Body, Method, Request, Response, StatusCode,
};

use crate::node::NodeRequestSender;
use crate::types::{GenericError, GenericResult};
use failure::Fail;
use hyper::server::conn::AddrIncoming;
use hyper::server::Builder;
use log::{error, info};
use serde::{Deserialize, Serialize};

/// data posted from performance-client
#[derive(Serialize, Deserialize)]
struct Message {
    nonce: u32,
    bytes: String,
}

#[derive(Serialize, Deserialize)]
struct MessageResp {
    cur_nonce: i32,
    cur_hash: String,
}

pub async fn run_http_server(
    server: Builder<AddrIncoming>,
    node_client: NodeRequestSender,
    shutdown_signal: futures::channel::oneshot::Receiver<()>,
) {
    let handler = MessageHandler { node_client };
    let server = server
        .serve(make_service_fn(move |_sock| {
            let handler = handler.clone();
            async {
                let svr = service_fn(move |req| request_handler(req, handler.clone()));
                Ok::<_, GenericError>(svr)
            }
        }))
        .with_graceful_shutdown(async {
            shutdown_signal.await.ok();
        });

    match server.await {
        Err(e) => {
            error!(target: "http", "hyper server error, reason: {:?}", e);
        }
        Ok(_) => {
            info!(target: "http", "hyper server stopped");
        }
    }
}

/// dispatch http request
async fn request_handler(
    req: Request<Body>,
    mut handler: MessageHandler,
) -> GenericResult<Response<Body>> {
    match (req.method(), req.uri().path()) {
        // post message to p2p network
        (&Method::POST, "/") => handler.handle_message(req).await,
        // get cur hash state of the p2p node
        (&Method::GET, "/state") => handler.get_state(req).await,
        _ => {
            let resp = Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body("not found".into())
                .unwrap();
            Ok(resp)
        }
    }
}

#[derive(Clone)]
struct MessageHandler {
    node_client: NodeRequestSender,
}

impl MessageHandler {
    async fn get_state(&mut self, _req: Request<Body>) -> GenericResult<Response<Body>> {
        let resp_body = match self.node_client.cur_state().await {
            Ok(s) => s,
            Err(e) => Err(e.compat())?,
        };
        Self::state_respond(resp_body)
    }
    /// handle post http req
    async fn handle_message(&mut self, req: Request<Body>) -> GenericResult<Response<Body>> {
        let body = req.into_body().try_concat().await?;
        let json_str = String::from_utf8(body.to_vec())?;
        let msg: Message = serde_json::from_str(&json_str)?;
        let raw_data = base64::decode_config(&msg.bytes, base64::URL_SAFE)?;

        let resp_body = match self.node_client.send_message(msg.nonce, raw_data).await {
            Ok(d) => d,
            Err(e) => Err(e.compat())?,
        };

        Self::state_respond(resp_body)
    }

    fn state_respond(state: Option<(u32, Vec<u8>)>) -> GenericResult<Response<Body>> {
        let msg_resp = match state {
            Some(state) => MessageResp {
                cur_nonce: state.0 as i32,
                cur_hash: hex::encode(state.1),
            },
            None => MessageResp {
                cur_nonce: -1,
                cur_hash: "".to_string(),
            },
        };
        let resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(serde_json::to_string(&msg_resp)?))?;
        Ok(resp)
    }
}
