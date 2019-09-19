use hyper::server::conn::AddrStream;
use hyper::{
    service::{
        self,
        make_service_fn,
        service_fn,
        Service
    },
    Error, Body, Chunk, Client, Method, Request, Response, Server, StatusCode, header
};
use futures::{
    Stream, StreamExt, TryStreamExt
};

use std::net::SocketAddr;
use serde::{Deserialize, Serialize};
use crate::p2p::NodeRequestSender;
use failure::Fail;

/// data posted from performance-client
#[derive(Serialize, Deserialize)]
struct Message {
    nonce: u32,
    data: String,
}

#[derive(Serialize, Deserialize)]
struct MessageResp {
    cur_nonce: u32,
    cur_hash: String
}

pub async fn run_http_server(addr: SocketAddr, node_client: NodeRequestSender) -> hyper::error::Result<()> {
    let handler = MessageHandler {
        node_client
    };

    let server = Server::bind(&addr).serve(make_service_fn(move |sock| {
        let handler = handler.clone();
        async {
            let svr = service_fn(move |req| {
                request_handler(req, handler.clone())
            });
            Ok::<_, GenericError>(svr)
        }
    }));

    server.await
}

/// dispatch http request
async fn request_handler(req: Request<Body>, mut handler: MessageHandler) -> GenericResult<Response<Body>> {
    match (req.method(), req.uri().path()) {
        (&Method::POST, "/") => {
            handler.handle_message(req).await
        }
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
    node_client: NodeRequestSender
}

impl MessageHandler {
    /// handle post http req
    async fn handle_message(&mut self, req: Request<Body>) -> GenericResult<Response<Body>> {
        let body = req.into_body().try_concat().await?;
        let json_str = String::from_utf8(body.to_vec())?;
        let mut msg: Message = serde_json::from_str(&json_str)?;
        let resp_body = match self.node_client.send_message(msg.nonce, msg.data).await {
            Ok(d) => d,
            Err(e) => Err(e.compat())?
        };
        let msg_resp = MessageResp {
            cur_nonce: resp_body.0,
            cur_hash: resp_body.1,
        };
        let resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(serde_json::to_string(&msg_resp)?))?;
        Ok(resp)
    }
}


type GenericError = Box<dyn std::error::Error + Send + Sync>;
type GenericResult<T> = std::result::Result<T, GenericError>;






