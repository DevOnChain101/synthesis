//! Endpoint (unix socket / Windows named pipe), NDJSON frames and the mutual HMAC handshake
//! (messaging.md B4, B5). The key never crosses the wire; neither side sends anything but nonces
//! until the other has proven the key.

use std::io;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::Key;

/// Longest NDJSON line, either direction.
pub const MAX_FRAME: usize = 1 << 20;
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
/// Longest unix socket path accepted (sun_path is ~104 B on macOS); longer disables peers.
pub const MAX_SOCKET_PATH: usize = 100;

pub trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}

/// One connection speaking NDJSON frames.
pub struct Conn {
    r: BufReader<tokio::io::ReadHalf<Box<dyn Duplex>>>,
    w: tokio::io::WriteHalf<Box<dyn Duplex>>,
    // Persistent so `recv` can lose a `select!` race without dropping bytes already read.
    buf: Vec<u8>,
}

impl Conn {
    pub fn new(s: impl Duplex + 'static) -> Self {
        let (r, w) = tokio::io::split(Box::new(s) as Box<dyn Duplex>);
        Self {
            r: BufReader::new(r),
            w,
            buf: Vec::new(),
        }
    }

    pub async fn send(&mut self, v: &Value) -> io::Result<()> {
        let mut line = serde_json::to_vec(v).map_err(io::Error::other)?;
        line.push(b'\n');
        if line.len() > MAX_FRAME + 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame too large",
            ));
        }
        self.w.write_all(&line).await?;
        self.w.flush().await
    }

    /// Next frame; `None` on a clean EOF. Cancel-safe.
    pub async fn recv(&mut self) -> io::Result<Option<Value>> {
        loop {
            let room = (MAX_FRAME + 2).saturating_sub(self.buf.len()) as u64;
            let n = (&mut self.r)
                .take(room)
                .read_until(b'\n', &mut self.buf)
                .await?;
            if self.buf.last() == Some(&b'\n') {
                let line = std::mem::take(&mut self.buf);
                return serde_json::from_slice(&line)
                    .map(Some)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad frame"));
            }
            if self.buf.len() > MAX_FRAME {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "frame too large",
                ));
            }
            if n == 0 {
                return if self.buf.is_empty() {
                    Ok(None)
                } else {
                    Err(io::ErrorKind::UnexpectedEof.into())
                };
            }
        }
    }
}

fn mac(key: &Key, label: &[u8], a: &[u8], b: &[u8]) -> Hmac<Sha256> {
    let mut m = Hmac::<Sha256>::new_from_slice(&key.0).expect("hmac takes any key length");
    m.update(label);
    m.update(a);
    m.update(b);
    m
}

fn proof(key: &Key, label: &[u8], a: &[u8], b: &[u8]) -> String {
    hex(&mac(key, label, a, b).finalize().into_bytes())
}

/// Constant-time check of a hex proof.
fn verify(key: &Key, label: &[u8], a: &[u8], b: &[u8], got: Option<&str>) -> bool {
    let Some(bytes) = got.and_then(unhex) else {
        return false;
    };
    mac(key, label, a, b).verify_slice(&bytes).is_ok()
}

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

pub(crate) fn random<const N: usize>() -> io::Result<[u8; N]> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(b)
}

fn nonce(v: &Value) -> Option<[u8; 16]> {
    B64.decode(v.get("nonce")?.as_str()?).ok()?.try_into().ok()
}

fn bad(m: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, m)
}

async fn within<T>(f: impl Future<Output = io::Result<T>>) -> io::Result<T> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, f)
        .await
        .map_err(|_| bad("handshake timeout"))?
}

/// Server side. Returns only after the client proved the key.
pub async fn handshake_server(c: &mut Conn, key: &Key) -> io::Result<()> {
    within(async {
        let hello = c.recv().await?.ok_or_else(|| bad("closed"))?;
        let nc = (hello.get("t") == Some(&json!("hello")) && hello.get("v") == Some(&json!(1)))
            .then(|| nonce(&hello))
            .flatten()
            .ok_or_else(|| bad("bad hello"))?;
        let ns = random::<16>()?;
        c.send(&json!({"t":"hello","v":1,"nonce":B64.encode(ns),
            "proof":proof(key,b"s",&nc,&ns)}))
            .await?;
        let auth = c.recv().await?.ok_or_else(|| bad("closed"))?;
        let ok = auth.get("t") == Some(&json!("auth"))
            && verify(
                key,
                b"c",
                &ns,
                &nc,
                auth.get("proof").and_then(Value::as_str),
            );
        if ok { Ok(()) } else { Err(bad("bad proof")) }
    })
    .await
}

/// Client side. Aborts, having sent only a nonce, when the server cannot prove the key.
pub async fn handshake_client(c: &mut Conn, key: &Key) -> io::Result<()> {
    within(async {
        let nc = random::<16>()?;
        c.send(&json!({"t":"hello","v":1,"nonce":B64.encode(nc)}))
            .await?;
        let hello = c.recv().await?.ok_or_else(|| bad("closed"))?;
        let ns = nonce(&hello).ok_or_else(|| bad("bad hello"))?;
        if !verify(
            key,
            b"s",
            &nc,
            &ns,
            hello.get("proof").and_then(Value::as_str),
        ) {
            return Err(bad("server did not prove the key"));
        }
        c.send(&json!({"t":"auth","proof":proof(key,b"c",&ns,&nc)}))
            .await
    })
    .await
}

#[cfg(unix)]
pub use unix::{Listener, connect, prepare_run_dir};
#[cfg(windows)]
pub use windows::{Listener, connect, prepare_run_dir};

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    use tokio::net::{UnixListener, UnixStream};

    use super::*;

    /// 0700 directory for sockets, key and registry.
    pub fn prepare_run_dir(dir: &Path) -> io::Result<()> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let bad = |m: &'static str| io::Error::new(io::ErrorKind::PermissionDenied, m);
        let meta = std::fs::symlink_metadata(dir)?;
        if !meta.is_dir() {
            return Err(bad("the run directory is a link or not a directory"));
        }
        // A file we create is ours: its owner is the effective uid (no libc, no unsafe).
        let probe = dir.join(format!(".probe{}", hex(&random::<4>()?)));
        std::fs::File::create(&probe)?;
        let mine = std::fs::symlink_metadata(&probe).map(|m| m.uid());
        let _ = std::fs::remove_file(&probe);
        if mine? != meta.uid() {
            return Err(bad("the run directory is not owned by this user"));
        }
        if meta.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    pub struct Listener {
        inner: UnixListener,
        owner_uid: u32,
        path: PathBuf,
    }

    impl Listener {
        pub fn bind(run_dir: &Path) -> io::Result<Self> {
            let path = run_dir.join(format!("{}.sock", hex(&random::<8>()?)));
            if path.as_os_str().len() > MAX_SOCKET_PATH {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "socket path too long",
                ));
            }
            let inner = UnixListener::bind(&path)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            Ok(Self {
                inner,
                owner_uid: std::fs::metadata(run_dir)?.uid(),
                path,
            })
        }

        /// Opaque to callers; goes into the registry entry only.
        pub fn endpoint(&self) -> String {
            self.path.to_string_lossy().into_owned()
        }

        pub fn kind(&self) -> &'static str {
            "unix"
        }

        /// Next connection whose peer uid owns the run dir; others are dropped.
        pub async fn accept(&mut self) -> io::Result<Conn> {
            loop {
                let (s, _) = self.inner.accept().await?;
                if s.peer_cred()
                    .is_ok_and(|c| uid_allowed(c.uid(), self.owner_uid))
                {
                    return Ok(Conn::new(s));
                }
            }
        }
    }

    pub(crate) fn uid_allowed(peer: u32, owner: u32) -> bool {
        peer == owner
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    pub async fn connect(endpoint: &str) -> io::Result<Conn> {
        Ok(Conn::new(UnixStream::connect(endpoint).await?))
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn uid_comparison() {
            assert!(super::uid_allowed(501, 501));
            assert!(!super::uid_allowed(0, 501));
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::path::Path;

    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};

    use super::*;

    pub fn prepare_run_dir(dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)
    }

    pub struct Listener {
        name: String,
        next: NamedPipeServer,
    }

    impl Listener {
        pub fn bind(_run_dir: &Path) -> io::Result<Self> {
            let name = format!(r"\\.\pipe\synthesis-peer-{}", hex(&random::<16>()?));
            let next = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(&name)?;
            Ok(Self { name, next })
        }

        pub fn endpoint(&self) -> String {
            self.name.clone()
        }

        pub fn kind(&self) -> &'static str {
            "pipe"
        }

        pub async fn accept(&mut self) -> io::Result<Conn> {
            self.next.connect().await?;
            let fresh = ServerOptions::new()
                .reject_remote_clients(true)
                .create(&self.name)?;
            Ok(Conn::new(std::mem::replace(&mut self.next, fresh)))
        }
    }

    pub async fn connect(endpoint: &str) -> io::Result<Conn> {
        Ok(Conn::new(ClientOptions::new().open(endpoint)?))
    }
}
