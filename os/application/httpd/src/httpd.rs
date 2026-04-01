//! ping – answer HTTP requests
#![no_std]
extern crate alloc;

use core::{fmt::Display, net::{IpAddr, Ipv6Addr, SocketAddr}};

use alloc::{collections::btree_map::BTreeMap, format, string::String, vec::Vec};
use httparse::{EMPTY_HEADER, Request};
use network::{NetworkError, TcpListener, TcpStream};
#[allow(unused_imports)]
use runtime::*;
use terminal::println;

#[unsafe(no_mangle)]
fn main() {
    // ignore all args for now
    let port = 1797;
    let ip = IpAddr::V6(Ipv6Addr::UNSPECIFIED);
    println!("listening to [{}]:{}", ip, port);
    
    let mut listener = TcpListener::bind(SocketAddr::new(ip, port))
        .expect("failed to bind socket");
    let mut buffer: [u8; 4096] = [0; 4096];
    loop {
        if let Ok(client) = listener.accept() {
            println!("got a connection from {}", client.peer_addr());
            buffer.fill(0);
            if let Ok(len) = client.read(&mut buffer) {
                let mut headers = [EMPTY_HEADER; 64];
                let mut request = Request::new(&mut headers);
                match request.parse(&buffer[0..len]) {
                    Ok(_body_start) => if let Err(e) = handle(request).send_to(client) {
                        println!("couldn't send reponse to client: {:?}", e);
                    },
                    Err(e) => println!("couldn't parse client request: {:?}", e),
                }
            }
        }
    }
}

struct Response {
    status: StatusCode,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl Response {
    /// Create a new response.
    fn new(status: StatusCode) -> Self {
        let mut headers = BTreeMap::new();
        headers.insert("Server".into(), "D3OS httpd".into());
        headers.insert("Connection".into(), "close".into());
        Self { status, headers, body: Vec::new() }
    }
    
    /// Send this response to a client.
    fn send_to(self, client: TcpStream) -> Result<(),  NetworkError> {
        client.write(format!("HTTP/1.1 {}\n", self.status).as_bytes())?;
        for (header_name, header_value) in self.headers {
            client.write(format!("{}: {}\n", header_name, header_value).as_bytes())?;
        }
        client.write(b"\n")?;
        client.write(&self.body)?;
        client.write(b"\n\n")?;
        Ok(())
    }
}

/// HTTP Status Codes as in <https://datatracker.ietf.org/doc/html/rfc9110#name-status-codes>
enum StatusCode {
    Ok,
    MethodNotAllowed,
}

impl Display for StatusCode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", match self {
            Self::Ok => "200 OK",
            Self::MethodNotAllowed => "405 Method Not Allowed",
        })
    }
}

/// Handle this request, returning a response.
fn handle(request: Request) -> Response {
    println!("handling request {:?}", request);
    match request.method.expect("failed to get method") {
        "GET" => {
            let mut r = Response::new(StatusCode::Ok);
            r.headers.insert("Content-Type".into(), "text/plain; charset=UTF-8".into());
            r.body.extend("Hello from D3OS!".as_bytes());
            r
        },
        method => {
            let mut r = Response::new(StatusCode::MethodNotAllowed);
            r.headers.insert("Allowed".into(), "GET".into());
            r.body.extend(format!("Method not allowed: {method}").as_bytes());
            r
        },
    }
}
