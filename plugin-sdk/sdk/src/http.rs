//! HTTP/1.1 over whatever carries bytes (a tarae connection, TLS over one): a request written out, a
//! response read as its bytes come — by length, chunked, or until the connection closes — and a streamed
//! body cut into lines.

/// A request, as bytes to send. `Host` is added; so is `Content-Length` when there's a body. Line ends (and
/// other control characters) in what's given are dropped, so a value — a token from a file — can't add a
/// header or a request of its own.
pub fn request(method: &str, path: &str, host: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let c = |s: &str| s.chars().filter(|c| !c.is_control()).collect::<String>();
    let mut out = format!("{} {} HTTP/1.1\r\nHost: {}\r\n", c(method), c(path), c(host));
    for (k, v) in headers {
        out.push_str(&format!("{}: {}\r\n", c(k), c(v)));
    }
    if !body.is_empty() {
        out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    out.push_str("\r\n");
    let mut out = out.into_bytes();
    out.extend_from_slice(body);
    out
}

/// A response's status line and headers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

impl Head {
    /// A header's value (the first, by name in any case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug)]
enum Framing {
    /// This many bytes left.
    Length(usize),
    Chunked(Chunk),
    /// Until the connection closes.
    UntilClose,
}

#[derive(Debug)]
enum Chunk {
    /// Reading a chunk's size line.
    Size,
    /// This many bytes of it left.
    Data(usize),
    /// The CRLF after its data.
    End,
    /// After the last chunk: its trailers, up to an empty line.
    Trailer,
}

/// A response, read as its bytes come.
#[derive(Debug, Default)]
pub struct Response {
    head: Option<Head>,
    framing: Option<Framing>,
    buf: Vec<u8>,
    done: bool,
}

impl Response {
    pub fn new() -> Response {
        Response::default()
    }

    /// Its head, once it's in.
    pub fn head(&self) -> Option<&Head> {
        self.head.as_ref()
    }

    /// Whether the whole body is in.
    pub fn done(&self) -> bool {
        self.done
    }

    /// The connection closed: a body read until then is done (else it was cut short).
    pub fn closed(&mut self) -> Result<(), String> {
        match self.framing {
            Some(Framing::UntilClose) => {
                self.done = true;
                Ok(())
            }
            _ if self.done => Ok(()),
            _ => Err("the connection closed before the response ended".into()),
        }
    }

    /// Takes bytes from the connection; gives back the body bytes they complete.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<u8>, String> {
        self.buf.extend_from_slice(bytes);
        let mut body = Vec::new();
        while self.head.is_none() {
            let mut headers = [httparse::EMPTY_HEADER; 128];
            let mut r = httparse::Response::new(&mut headers);
            let at = match r.parse(&self.buf).map_err(|e| format!("a malformed response: {e}"))? {
                httparse::Status::Complete(at) => at,
                httparse::Status::Partial => return Ok(body),
            };
            // `100 Continue` and the like: the response is still to come
            if r.code.is_some_and(|c| (100..200).contains(&c) && c != 101) {
                self.buf.drain(..at);
                continue;
            }
            let head = Head {
                status: r.code.unwrap_or(0),
                headers: r
                    .headers
                    .iter()
                    .map(|h| (h.name.to_string(), String::from_utf8_lossy(h.value).into_owned()))
                    .collect(),
            };
            // Chunked if that's the last coding (`gzip, chunked`: the chunks hold gzip — not undone here)
            let coding = head.header("transfer-encoding").map(|v| v.rsplit(',').next().unwrap_or("").trim().to_string());
            let chunked = coding.as_deref().is_some_and(|c| c.eq_ignore_ascii_case("chunked"));
            let length = match head.header("content-length") {
                Some(v) => Some(v.trim().parse::<usize>().map_err(|_| format!("a malformed Content-Length: {v:?}"))?),
                None => None,
            };
            self.framing = Some(match (chunked, length) {
                (true, _) => Framing::Chunked(Chunk::Size),
                // (another coding last: the body runs until the connection closes)
                (false, _) if coding.is_some() => Framing::UntilClose,
                (false, Some(n)) => Framing::Length(n),
                (false, None) if head.status == 204 || head.status == 304 => Framing::Length(0),
                (false, None) => Framing::UntilClose,
            });
            self.head = Some(head);
            self.buf.drain(..at);
        }
        loop {
            match self.framing.as_mut().expect("the head is in") {
                Framing::Length(left) => {
                    let n = (*left).min(self.buf.len());
                    body.extend(self.buf.drain(..n));
                    *left -= n;
                    self.done = *left == 0;
                    return Ok(body);
                }
                Framing::UntilClose => {
                    body.append(&mut self.buf);
                    return Ok(body);
                }
                Framing::Chunked(state) => match state {
                    Chunk::Size => {
                        let Some(i) = self.buf.windows(2).position(|w| w == b"\r\n") else { return Ok(body) };
                        let line = String::from_utf8_lossy(&self.buf[..i]).into_owned();
                        let size = line.split(';').next().unwrap_or("").trim();
                        let n = usize::from_str_radix(size, 16).map_err(|_| format!("a malformed chunk size: {line:?}"))?;
                        self.buf.drain(..i + 2);
                        *state = if n == 0 { Chunk::Trailer } else { Chunk::Data(n) };
                    }
                    Chunk::Data(left) => {
                        if self.buf.is_empty() {
                            return Ok(body);
                        }
                        let n = (*left).min(self.buf.len());
                        body.extend(self.buf.drain(..n));
                        *left -= n;
                        if *left == 0 {
                            *state = Chunk::End;
                        }
                    }
                    Chunk::End => {
                        if self.buf.len() < 2 {
                            return Ok(body);
                        }
                        self.buf.drain(..2);
                        *state = Chunk::Size;
                    }
                    Chunk::Trailer => {
                        let Some(i) = self.buf.windows(2).position(|w| w == b"\r\n") else { return Ok(body) };
                        self.buf.drain(..i + 2);
                        if i == 0 {
                            self.done = true;
                            return Ok(body);
                        }
                    }
                },
            }
        }
    }
}

/// A streamed body cut into lines (a Kubernetes watch sends one JSON event a line).
#[derive(Debug, Default)]
pub struct Lines {
    buf: Vec<u8>,
}

impl Lines {
    pub fn new() -> Lines {
        Lines::default()
    }

    /// Takes body bytes; gives back the lines they complete (without their line ends).
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            let line = line.strip_suffix(b"\n").unwrap_or(&line);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            out.push(String::from_utf8_lossy(line).into_owned());
        }
        out
    }

    /// What's left without a line end (at the stream's end).
    pub fn rest(&mut self) -> Option<String> {
        (!self.buf.is_empty()).then(|| String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `bytes` in pieces of `step`, collecting the body.
    fn read(bytes: &[u8], step: usize) -> (Response, Vec<u8>) {
        let mut r = Response::new();
        let mut body = Vec::new();
        for piece in bytes.chunks(step) {
            body.extend(r.feed(piece).unwrap());
        }
        (r, body)
    }

    #[test]
    fn a_request_is_written_out() {
        let r = request("GET", "/api/v1/pods", "k:6443", &[("Accept", "application/json")], b"");
        assert_eq!(r, b"GET /api/v1/pods HTTP/1.1\r\nHost: k:6443\r\nAccept: application/json\r\n\r\n");
        let r = request("POST", "/x", "h", &[], b"{}");
        assert!(r.ends_with(b"Content-Length: 2\r\n\r\n{}"));
    }

    #[test]
    fn bodies_by_length_chunked_or_until_close_in_any_pieces() {
        let fixed = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhelloEXTRA";
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nhel\r\n2;x=y\r\nlo\r\n0\r\nT: 1\r\n\r\n";
        for step in [1, 2, 7, 1000] {
            let (r, body) = read(fixed, step);
            assert_eq!((r.head().unwrap().status, &body[..], r.done()), (200, &b"hello"[..], true));
            let (r, body) = read(chunked, step);
            assert_eq!((&body[..], r.done()), (&b"hello"[..], true), "in pieces of {step}");
        }
        let (mut r, body) = read(b"HTTP/1.1 200 OK\r\n\r\nall of it", 4);
        assert_eq!((&body[..], r.done()), (&b"all of it"[..], false));
        r.closed().unwrap();
        assert!(r.done());
        let (mut r, _) = read(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort", 3);
        assert!(r.closed().is_err(), "cut short");
    }

    #[test]
    fn the_edges_interim_responses_codings_and_what_isnt_allowed() {
        // `100 Continue` first: skipped
        let (r, body) = read(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok", 3);
        assert_eq!((r.head().unwrap().status, &body[..], r.done()), (200, &b"ok"[..], true));
        // Chunked as the last coding
        let (r, body) =
            read(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n2\r\nzz\r\n0\r\n\r\n", 5);
        assert_eq!((&body[..], r.done()), (&b"zz"[..], true));
        let mut r = Response::new();
        assert!(r.feed(b"HTTP/1.1 200 OK\r\nContent-Length: 1, 2\r\n\r\n").is_err());
        // What's given can't add a line
        let req = request("GET", "/a\r\nX: 1", "h", &[("Authorization", "Bearer t\r\n\r\nGET /evil")], b"");
        assert_eq!(req, b"GET /aX: 1 HTTP/1.1\r\nHost: h\r\nAuthorization: Bearer tGET /evil\r\n\r\n");
    }

    #[test]
    fn a_stream_into_lines() {
        let mut l = Lines::new();
        assert_eq!(l.feed(b"{\"a\":1}\n{\"b\""), ["{\"a\":1}"]);
        assert_eq!(l.feed(b":2}\r\n"), ["{\"b\":2}"]);
        assert_eq!(l.feed(b"tail"), Vec::<String>::new());
        assert_eq!(l.rest().as_deref(), Some("tail"));
    }
}
