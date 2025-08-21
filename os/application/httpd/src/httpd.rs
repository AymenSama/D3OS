//! ping – answer HTTP requests
#![no_std]
extern crate alloc;

use core::net::{IpAddr, Ipv6Addr, SocketAddr};

use httparse::{Request, EMPTY_HEADER};
use network::TcpListener;
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
                    Ok(_body_start) => {
                        println!("handling request {:?}", request);
                        match request.method.expect("failed to get method") {
                            "GET" => {
                                let _ = client.write(b"HTTP/1.1 200 OK\n");
                                let _ = client.write(b"Server: D3OS httpd\n");
                                let _ = client.write(b"Connection: close\n");
                                let _ = client.write(b"Content-Type: text/plain; charset=UTF-8\n\n");
                                let _ = client.write(b"Hello from D3OS!\n\n");
                            },
                            method => unimplemented!("unknown method {method}"),
                        };
                        
                    },
                    Err(e) => println!("couldn't parse client request: {:?}", e),
                }
            }
        }
    }
}
