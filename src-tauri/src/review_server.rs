use std::{
    collections::VecDeque,
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

static REVIEW_SERVER: OnceLock<ReviewServer> = OnceLock::new();
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

struct ReviewServer {
    address: String,
    files: Arc<Mutex<VecDeque<ReviewFile>>>,
}

struct ReviewFile {
    path: PathBuf,
    token: String,
    content_type: &'static str,
}

pub fn publish(path: &Path) -> Result<String, String> {
    if !path.is_file() {
        return Err(format!(
            "Could not publish missing editor asset: {}",
            path.display()
        ));
    }
    let server = if let Some(server) = REVIEW_SERVER.get() {
        server
    } else {
        let candidate = start()?;
        let _ = REVIEW_SERVER.set(candidate);
        REVIEW_SERVER
            .get()
            .expect("review server should be available after startup")
    };
    let token = review_token();
    let mut files = server
        .files
        .lock()
        .map_err(|_| "Review server state became unavailable.".to_owned())?;
    if files.len() >= 32 {
        files.pop_front();
    }
    files.push_back(ReviewFile {
        path: path.to_path_buf(),
        token: token.clone(),
        content_type: content_type(path),
    });
    Ok(format!("http://{}/review/{token}", server.address))
}

fn start() -> Result<ReviewServer, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|error| format!("Could not start the local review server: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("Could not inspect the local review server: {error}"))?
        .to_string();
    let files = Arc::new(Mutex::new(VecDeque::new()));
    let files_for_thread = Arc::clone(&files);
    thread::spawn(move || {
        for connection in listener.incoming().flatten() {
            let files = Arc::clone(&files_for_thread);
            thread::spawn(move || serve(connection, files));
        }
    });
    Ok(ReviewServer { address, files })
}

fn serve(stream: TcpStream, current: Arc<Mutex<VecDeque<ReviewFile>>>) {
    let mut request = BufReader::new(&stream);
    let mut first_line = String::new();
    if request.read_line(&mut first_line).is_err() {
        return;
    }
    let mut range = None;
    loop {
        let mut line = String::new();
        if request.read_line(&mut line).is_err() || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(value) = line
            .strip_prefix("Range:")
            .or_else(|| line.strip_prefix("range:"))
        {
            range = Some(value.trim().to_owned());
        }
    }
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    if method != "GET" && method != "HEAD" {
        let _ = write_empty(stream, "405 Method Not Allowed", None);
        return;
    }
    let Some(token) = target.strip_prefix("/review/") else {
        let _ = write_empty(stream, "404 Not Found", None);
        return;
    };
    let review = match current.lock() {
        Ok(files) => files
            .iter()
            .find(|review| token == review.token)
            .map(|review| (review.path.clone(), review.content_type)),
        Err(_) => None,
    };
    let Some((path, content_type)) = review else {
        let _ = write_empty(stream, "404 Not Found", None);
        return;
    };
    let Ok(mut file) = File::open(path) else {
        let _ = write_empty(stream, "404 Not Found", None);
        return;
    };
    let Ok(total) = file.metadata().map(|metadata| metadata.len()) else {
        let _ = write_empty(stream, "500 Internal Server Error", None);
        return;
    };
    let Some((start, end, partial)) = parse_range(range.as_deref(), total) else {
        let _ = write_empty(stream, "416 Range Not Satisfiable", Some(total));
        return;
    };
    let length = end.saturating_sub(start).saturating_add(1);
    let status = if partial {
        "206 Partial Content"
    } else {
        "200 OK"
    };
    let mut stream = stream;
    let header = if partial {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nAccept-Ranges: bytes\r\nContent-Length: {length}\r\nContent-Range: bytes {start}-{end}/{total}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
        )
    } else {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nAccept-Ranges: bytes\r\nContent-Length: {length}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
        )
    };
    if stream.write_all(header.as_bytes()).is_err() || method == "HEAD" {
        return;
    }
    if file.seek(SeekFrom::Start(start)).is_ok() {
        let _ = io::copy(&mut file.take(length), &mut stream);
    }
}

fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("mp4" | "m4v") => "video/mp4",
        Some("mkv") => "video/x-matroska",
        Some("webm") => "video/webm",
        Some("m4a") => "audio/mp4",
        Some("wav") => "audio/wav",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

fn write_empty(mut stream: TcpStream, status: &str, total: Option<u64>) -> io::Result<()> {
    let range = total
        .map(|total| format!("Content-Range: bytes */{total}\r\n"))
        .unwrap_or_default();
    stream.write_all(
        format!("HTTP/1.1 {status}\r\n{range}Content-Length: 0\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    )
}

fn parse_range(value: Option<&str>, total: u64) -> Option<(u64, u64, bool)> {
    if total == 0 {
        return None;
    }
    let Some(value) = value else {
        return Some((0, total - 1, false));
    };
    let bytes = value.strip_prefix("bytes=")?;
    let (start, end) = bytes.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    if start >= total {
        return None;
    }
    let end = if end.is_empty() {
        total - 1
    } else {
        end.parse::<u64>().ok()?.min(total - 1)
    };
    (end >= start).then_some((start, end, true))
}

fn review_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let serial = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:032x}{serial:016x}")
}

#[cfg(test)]
mod tests {
    use super::{content_type, parse_range, publish};
    use std::{
        fs,
        io::{Read, Write},
        net::TcpStream,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn parses_open_and_bounded_ranges() {
        assert_eq!(parse_range(None, 10), Some((0, 9, false)));
        assert_eq!(parse_range(Some("bytes=2-5"), 10), Some((2, 5, true)));
        assert_eq!(parse_range(Some("bytes=8-"), 10), Some((8, 9, true)));
        assert_eq!(parse_range(Some("bytes=20-30"), 10), None);
    }

    #[test]
    fn reports_media_types_for_editor_assets() {
        assert_eq!(
            content_type(std::path::Path::new("capture.mp4")),
            "video/mp4"
        );
        assert_eq!(
            content_type(std::path::Path::new("wallpaper.jpg")),
            "image/jpeg"
        );
        assert_eq!(
            content_type(std::path::Path::new("system.m4a")),
            "audio/mp4"
        );
    }

    #[test]
    fn serves_a_review_file_with_http_ranges() {
        let path = std::env::temp_dir().join(format!(
            "pikscreen-review-server-test-{}.mp4",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::write(&path, b"0123456789").expect("fixture should be writable");
        let url = publish(&path).expect("server should publish fixture");
        let request_target = url
            .strip_prefix("http://")
            .expect("review URL should be local HTTP");
        let (address, target) = request_target
            .split_once('/')
            .expect("review URL should include a route");
        let mut stream = TcpStream::connect(address).expect("review server should accept TCP");
        write!(
            stream,
            "GET /{target} HTTP/1.1\r\nHost: {address}\r\nRange: bytes=2-5\r\n\r\n"
        )
        .expect("request should be writable");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .expect("response should be readable");
        assert!(response.starts_with("HTTP/1.1 206 Partial Content"));
        assert!(response.contains("Content-Range: bytes 2-5/10"));
        assert!(response.ends_with("2345"));
        fs::remove_file(path).expect("fixture should be removable");
    }

    #[test]
    fn keeps_multiple_published_files_available() {
        let serial = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let first = std::env::temp_dir().join(format!("pikscreen-review-first-{serial}.mp4"));
        let second = std::env::temp_dir().join(format!("pikscreen-review-second-{serial}.mp4"));
        fs::write(&first, b"first").expect("first fixture should be writable");
        fs::write(&second, b"second").expect("second fixture should be writable");
        let first_url = publish(&first).expect("first fixture should publish");
        let second_url = publish(&second).expect("second fixture should publish");

        for (url, expected) in [(first_url, "first"), (second_url, "second")] {
            let request_target = url.strip_prefix("http://").expect("local HTTP URL");
            let (address, target) = request_target.split_once('/').expect("review route");
            let mut stream = TcpStream::connect(address).expect("review server connection");
            write!(stream, "GET /{target} HTTP/1.1\r\nHost: {address}\r\n\r\n")
                .expect("request should be writable");
            let mut response = String::new();
            stream
                .read_to_string(&mut response)
                .expect("response should be readable");
            assert!(response.ends_with(expected));
        }

        fs::remove_file(first).expect("first fixture should be removable");
        fs::remove_file(second).expect("second fixture should be removable");
    }
}
