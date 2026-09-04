//! A hand-rolled HTTP endpoint, so anything that can `curl` can light a room.
//!
//! M6 asks for exactly this: *"`curl` to the HTTP endpoint turns the room red
//! for 30 seconds and it clears itself."* The clearing is the interesting half —
//! it needs no second request, because an alert is a source with an expiry and
//! the device removes it itself when the time passes.
//!
//! # Why no HTTP crate
//!
//! Sixty lines against a dependency tree with an async runtime in it, for a
//! server that answers three paths on a LAN. The device crates take no
//! third-party dependencies at all and this is the shell, but the same judgement
//! applies: the parsing here is a first line and a space, and anything more
//! elaborate would be more code, not less.
//!
//! It is **not** a public-facing server and must not become one. There is no
//! authentication, no TLS, no request size limit beyond the read buffer, and no
//! concurrency. It answers a home network, which is the same trust boundary the
//! rest of this protocol assumes — and if that boundary ever moves, this is the
//! first thing that has to go.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

/// What an HTTP request asked the mesh to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Request {
    /// Put an alert up for this many seconds.
    Alert(u64),
    /// Take the show back off.
    Off,
}

/// Listen on `port`, sending each understood request down `to`.
///
/// Spawns a thread and returns. Failing to bind is reported and not fatal: a
/// mesh node that cannot open a convenience port should carry on being a mesh
/// node.
pub fn serve(port: u16, to: Sender<Request>) {
    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("no HTTP on {port}: {e}");
            return;
        }
    };
    println!("http: curl http://<this machine>:{port}/alert");
    println!("      curl http://<this machine>:{port}/alert?seconds=30");
    println!("      curl http://<this machine>:{port}/off");

    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]);
            let target = head.split_whitespace().nth(1).unwrap_or("/");

            let (status, body, request) = route(target);
            if let Some(r) = request {
                // A closed channel means the mesh loop has gone; there is
                // nothing useful to tell the caller, so the request is answered
                // and dropped.
                let _ = to.send(r);
            }
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
}

/// Decide what a request target means.
///
/// Split out so it can be tested without a socket, which is most of what is
/// worth testing here.
fn route(target: &str) -> (&'static str, String, Option<Request>) {
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    match path {
        "/alert" => {
            // `?seconds=N`, clamped. Zero would be an alert nobody sees, and an
            // hour would be an alert nobody asked to live with - the expiry is
            // the safety property, so it is not something a query string gets to
            // remove.
            let seconds = query
                .split('&')
                .filter_map(|kv| kv.strip_prefix("seconds="))
                .filter_map(|v| v.parse::<u64>().ok())
                .next()
                .unwrap_or(30)
                .clamp(1, 300);
            (
                "200 OK",
                format!("alert for {seconds}s\n"),
                Some(Request::Alert(seconds)),
            )
        }
        "/off" => ("200 OK", "off\n".into(), Some(Request::Off)),
        "/" => (
            "200 OK",
            "lumen\n  GET /alert[?seconds=N]\n  GET /off\n".into(),
            None,
        ),
        _ => ("404 Not Found", "no\n".into(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_alert_defaults_to_thirty_seconds() {
        // The number M6 names.
        assert_eq!(route("/alert").2, Some(Request::Alert(30)));
    }

    #[test]
    fn seconds_can_be_asked_for_and_cannot_be_removed() {
        assert_eq!(route("/alert?seconds=5").2, Some(Request::Alert(5)));
        // The expiry is the safety property: an alert that never ends is what
        // stops a room being red for ever, so a query string does not get to
        // ask for zero or for an afternoon.
        assert_eq!(route("/alert?seconds=0").2, Some(Request::Alert(1)));
        assert_eq!(route("/alert?seconds=99999").2, Some(Request::Alert(300)));
        assert_eq!(route("/alert?seconds=nonsense").2, Some(Request::Alert(30)));
    }

    #[test]
    fn other_parameters_are_ignored_rather_than_refused() {
        assert_eq!(route("/alert?x=1&seconds=7&y=2").2, Some(Request::Alert(7)));
    }

    #[test]
    fn unknown_paths_do_nothing_at_all() {
        // Including ones that look close. A misspelled request that lit the room
        // would be worse than one that did not.
        assert_eq!(route("/alerts").2, None);
        assert_eq!(route("/ALERT").2, None);
        assert_eq!(route("/../alert").2, None);
        assert_eq!(route("/alerts").0, "404 Not Found");
    }

    #[test]
    fn the_root_explains_itself_without_doing_anything() {
        let (status, body, request) = route("/");
        assert_eq!(status, "200 OK");
        assert!(body.contains("/alert"));
        assert_eq!(request, None);
    }
}
