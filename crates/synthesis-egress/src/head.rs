//! Reads and parses the request head of one proxy connection.

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};

pub const MAX_HEAD: usize = 8 * 1024;
pub const HEAD_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Debug, PartialEq, Eq)]
pub struct Head {
    /// `host:port` of a CONNECT.
    pub authority: String,
    /// Raw `Proxy-Authorization` value (never logged).
    pub auth: Option<String>,
    /// Bytes the client sent after the head.
    pub rest: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum HeadError {
    Bad(&'static str),
}

/// Read until the head is complete (8 KiB cap, 10 s deadline). Only CONNECT is understood.
/// shortcut: absolute-form forwarding (`GET http://host/...`) for the browser plugs in here
/// by returning another variant; today it is refused as a bad request.
pub async fn read_head<R: AsyncRead + Unpin>(r: &mut R) -> Result<Head, HeadError> {
    tokio::time::timeout(HEAD_DEADLINE, async {
        let mut buf = Vec::with_capacity(512);
        loop {
            let mut chunk = [0u8; 1024];
            let n = r
                .read(&mut chunk)
                .await
                .map_err(|_| HeadError::Bad("read failed"))?;
            if n == 0 {
                return Err(HeadError::Bad("closed before the head ended"));
            }
            buf.extend_from_slice(&chunk[..n]);
            let mut headers = [httparse::EMPTY_HEADER; 32];
            let mut req = httparse::Request::new(&mut headers);
            match req.parse(&buf) {
                Ok(httparse::Status::Complete(used)) => return finish(&req, &buf, used),
                Ok(httparse::Status::Partial) if buf.len() >= MAX_HEAD => {
                    return Err(HeadError::Bad("head too large"));
                }
                Ok(httparse::Status::Partial) => {}
                Err(_) => return Err(HeadError::Bad("malformed request")),
            }
        }
    })
    .await
    .unwrap_or(Err(HeadError::Bad("head timed out")))
}

fn finish(req: &httparse::Request, buf: &[u8], used: usize) -> Result<Head, HeadError> {
    if used > MAX_HEAD {
        return Err(HeadError::Bad("head too large"));
    }
    if req.method != Some("CONNECT") {
        return Err(HeadError::Bad("only CONNECT is supported"));
    }
    let authority = req.path.ok_or(HeadError::Bad("no target"))?.to_owned();
    let auth = req
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("proxy-authorization"))
        .and_then(|h| std::str::from_utf8(h.value).ok())
        .map(str::to_owned);
    Ok(Head {
        authority,
        auth,
        rest: buf[used..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parse(b: &[u8]) -> Result<Head, HeadError> {
        let mut r = b;
        read_head(&mut r).await
    }

    #[tokio::test]
    async fn parses_connect_and_keeps_the_rest() {
        let h = parse(b"CONNECT a.test:443 HTTP/1.1\r\nHost: a.test:443\r\nProxy-Authorization: Basic abc\r\n\r\nXYZ")
            .await
            .unwrap();
        assert_eq!(h.authority, "a.test:443");
        assert_eq!(h.auth.as_deref(), Some("Basic abc"));
        assert_eq!(h.rest, b"XYZ");
    }

    #[tokio::test]
    async fn refuses_oversized_malformed_and_other_methods() {
        let mut big = b"CONNECT a.test:443 HTTP/1.1\r\nX: ".to_vec();
        big.resize(MAX_HEAD + 100, b'a');
        assert!(parse(&big).await.is_err());
        assert!(
            parse(b"CONNECT a.test:443 HTTP/1.1\r\nBad\x00: x\r\n\r\n")
                .await
                .is_err()
        );
        assert!(parse(b"CONNECT a\r\nb:443 HTTP/1.1\r\n\r\n").await.is_err());
        assert!(parse(b"GET http://a.test/ HTTP/1.1\r\n\r\n").await.is_err());
        assert!(parse(b"CONNECT a.test:443 HTTP/1.1\r\n").await.is_err()); // closed early
    }
}
