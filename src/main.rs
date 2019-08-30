use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error as StdError;
use std::net::SocketAddr;

#[derive(Serialize, Deserialize)]
struct Message {
    nonce: u32,
    data: String,
}

fn main() {}
