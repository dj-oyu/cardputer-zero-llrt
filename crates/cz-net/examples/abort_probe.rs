//! Probe: does cancel_fetch() wake a worker blocked awaiting response headers?
use cz_net::fetch::{cancel_fetch, next_event, start_fetch, FetchSpec, RedirectMode};
use cz_net::tls::TlsClientOptions;
use std::io::Read;
use std::net::TcpListener;
use std::time::{Duration, Instant};

fn main() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        let mut buf = [0u8; 512];
        let _ = stream.read(&mut buf);
        println!("SERVER_READ_REQUEST, holding open");
        std::thread::sleep(Duration::from_secs(20));
    });

    let id = start_fetch(FetchSpec {
        url: format!("http://127.0.0.1:{port}/hang"),
        method: "GET".to_string(),
        headers: Vec::new(),
        body: None,
        redirect: RedirectMode::Follow,
        tls: TlsClientOptions::default(),
    })
    .expect("start");
    println!("FETCH_ID={id}");
    std::thread::sleep(Duration::from_millis(600));
    println!("BEFORE_CANCEL next={:?}", next_event(id).is_some());
    let started = Instant::now();
    println!("CANCELLED={}", cancel_fetch(id));
    loop {
        match next_event(id) {
            Some(ev) => {
                println!("EVENT {ev:?} after {:?}", started.elapsed());
                break;
            }
            None => {
                if started.elapsed() > Duration::from_secs(4) {
                    println!("NO_EVENT_WITHIN_4S");
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    println!("PROBE_DONE");
    std::process::exit(0);
}
