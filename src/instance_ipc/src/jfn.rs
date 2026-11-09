use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "message")]
pub enum Request {
    Ping,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "message")]
pub enum Response {
    Pong,
}

pub fn handle(req: &Request) -> Response {
    match req {
        Request::Ping => Response::Pong,
    }
}
