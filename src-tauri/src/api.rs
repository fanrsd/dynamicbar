//! Local HTTP endpoint so scripts, devices on this PC and coding agents can raise island notifications.
//!
//! - `POST /notify` `{"app": "…", "title": "…", "body": "…"}`
//! - `POST /claude` raw Claude Code hook input (`Stop`, `Notification` events)
//!
//! Bound to 127.0.0.1 only; requests carrying an `Origin` header are refused, so web pages
//! open in a browser can't post to it (curl, scripts and Bun/Node fetch don't send one).

use crate::sys::Notification;
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    thread,
    time::Duration,
};
use tauri::{AppHandle, Emitter};

pub const PORT: u16 = 47800;

pub fn spawn(app: AppHandle) {
    thread::spawn(move || {
        let listener = match TcpListener::bind(("127.0.0.1", PORT)) {
            Ok(l) => l,
            Err(e) => return eprintln!("api: can't listen on {PORT}: {e}"),
        };
        // ponytail: one request at a time; a stalled client holds the line for at most 2s
        for stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            if let Err(e) = handle(&app, &stream) {
                eprintln!("api: {e}");
            }
        }
    });
}

fn handle(app: &AppHandle, stream: &TcpStream) -> std::io::Result<()> {
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default().to_owned());
    let (mut len, mut origin) = (0usize, false);
    loop {
        let mut header = String::new();
        if r.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
        let (k, v) = header.split_once(':').unwrap_or((&header, ""));
        match k.trim().to_ascii_lowercase().as_str() {
            "content-length" => len = v.trim().parse().unwrap_or(0),
            "origin" => origin = true,
            _ => {}
        }
    }
    let status = if origin {
        "403 Forbidden"
    } else if method != "POST" || len > 64 * 1024 {
        "400 Bad Request"
    } else {
        let mut body = vec![0; len];
        r.read_exact(&mut body)?;
        match parse(&path, &body) {
            Some(n) => {
                let _ = app.emit("notification", n);
                "204 No Content"
            }
            None => "400 Bad Request",
        }
    };
    write!(&mut &*stream, "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
}

fn parse(path: &str, body: &[u8]) -> Option<Notification> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_owned();
    let (app, title, body) = match path {
        "/notify" => (Some(s("app")).filter(|a| !a.is_empty()).unwrap_or("Notification".into()), s("title"), s("body")),
        "/claude" => {
            let cwd = s("cwd");
            let project = Path::new(&cwd).file_name().map_or(cwd.clone(), |f| f.to_string_lossy().into_owned());
            let body = match v["hook_event_name"].as_str()? {
                "Stop" => "Finished".to_owned(),
                "Notification" => s("message"),
                _ => return None,
            };
            ("Claude Code".to_owned(), project, body)
        }
        _ => return None,
    };
    (!title.is_empty()).then_some(Notification { app, title, body, icon: None, aumid: None })
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn maps_claude_hooks_and_generic_posts() {
        let stop = parse("/claude", br#"{"hook_event_name":"Stop","cwd":"C:\\dev\\DynamicBar"}"#).unwrap();
        assert_eq!((stop.app.as_str(), stop.title.as_str(), stop.body.as_str()), ("Claude Code", "DynamicBar", "Finished"));
        let ask = parse("/claude", br#"{"hook_event_name":"Notification","cwd":"/tmp/x","message":"Claude needs your permission"}"#).unwrap();
        assert_eq!(ask.body, "Claude needs your permission");
        assert!(parse("/claude", br#"{"hook_event_name":"PreToolUse","cwd":"x"}"#).is_none());
        let n = parse("/notify", br#"{"title":"Flash done"}"#).unwrap();
        assert_eq!((n.app.as_str(), n.title.as_str()), ("Notification", "Flash done"));
        assert!(parse("/notify", br#"{"body":"no title"}"#).is_none());
        assert!(parse("/other", b"{}").is_none());
    }
}
