use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use fmem::{memory::io, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{ErrorKind, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const VERSION: u32 = 1;
const MAX_FRAME_BYTES: usize = 16_384;
const ENDPOINT_NAME: &str = "endpoint";
type Secret = [u8; 32];

#[derive(Serialize, Deserialize)]
struct Endpoint {
    version: u32,
    port: u16,
    secret: Secret,
}

pub(super) struct Listener {
    socket: TcpListener,
    endpoint: PathBuf,
    secret: Secret,
    _lease: File,
}

impl Listener {
    pub(super) fn open(state: &Path) -> Result<Option<Self>> {
        let directory = private_directory(state, true)?;
        let lease_path = directory.join("lease.lock");
        io::file(&lease_path)?;
        let lease = io::options()
            .create(true)
            .read(true)
            .write(true)
            .open(lease_path)
            .map_err(io::err)?;
        match fs2::FileExt::try_lock_exclusive(&lease) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                return Ok(None);
            }
            Err(error) => return Err(io::err(error)),
        }
        let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(io::err)?;
        socket.set_nonblocking(true).map_err(io::err)?;
        let secret = random()?;
        let endpoint = Endpoint {
            version: VERSION,
            port: socket.local_addr().map_err(io::err)?.port(),
            secret,
        };
        let endpoint_text = serde_json::to_vec(&endpoint).map_err(io::err)?;
        let endpoint_text = URL_SAFE_NO_PAD.encode(platform::protect(&endpoint_text)?);
        let endpoint_path = directory.join(ENDPOINT_NAME);
        io::write(&endpoint_path, &endpoint_text, true, None)?;
        Ok(Some(Self {
            socket,
            endpoint: endpoint_path,
            secret,
            _lease: lease,
        }))
    }

    pub(super) fn accept<T: DeserializeOwned>(
        &self,
        budget: Duration,
    ) -> Result<Option<(Exchange, T)>> {
        let (stream, address) = match self.socket.accept() {
            Ok(connection) => connection,
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(io::err(error)),
        };
        if !address.ip().is_loopback() {
            return Ok(None);
        }
        let deadline = Instant::now() + budget;
        server_exchange(stream, self.secret, deadline)
            .and_then(|mut exchange| {
                let request = exchange.receive(b'C')?;
                Ok((exchange, request))
            })
            .map(Some)
            .or(Ok(None))
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = io::delete(&self.endpoint);
    }
}

pub(super) fn send<T: Serialize, U: DeserializeOwned>(
    state: &Path,
    request: &T,
    deadline: Instant,
) -> Result<U> {
    let directory = private_directory(state, false)?;
    let path = directory.join(ENDPOINT_NAME);
    platform::check_file(&path)?;
    let encoded = io::read_limit(&path, 8192)?.ok_or("Semantic worker is unavailable")?;
    let bytes = URL_SAFE_NO_PAD.decode(encoded).map_err(io::err)?;
    let endpoint: Endpoint =
        serde_json::from_slice(&platform::unprotect(&bytes)?).map_err(io::err)?;
    if endpoint.version != VERSION || endpoint.port == 0 {
        return Err("Unsupported semantic worker endpoint".into());
    }
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, endpoint.port));
    let stream = TcpStream::connect_timeout(&address, remaining(deadline)?).map_err(io::err)?;
    let mut exchange = Exchange::new(stream, endpoint.secret, deadline)?;
    fill(&mut exchange.context[..32])?;
    write_all(&mut exchange.stream, deadline, &exchange.context[..32])?;
    read_all(&mut exchange.stream, deadline, &mut exchange.context[32..])?;
    let mut proof = [0; 32];
    read_all(&mut exchange.stream, deadline, &mut proof)?;
    exchange.verify(b'H', &[], &proof)?;
    exchange.send(b'C', request)?;
    exchange.receive(b'S')
}

fn private_directory(state: &Path, create: bool) -> Result<PathBuf> {
    let directory = state.join("semantic-worker");
    if !io::profile_state(&directory, create)? {
        return Err("Semantic worker is unavailable".into());
    }
    platform::check_directory(&directory)?;
    Ok(directory)
}

pub(super) struct Exchange {
    stream: TcpStream,
    secret: Secret,
    context: [u8; 64],
    pub(super) deadline: Instant,
}

impl Exchange {
    fn new(stream: TcpStream, secret: Secret, deadline: Instant) -> Result<Self> {
        stream.set_nonblocking(false).map_err(io::err)?;
        stream.set_nodelay(true).map_err(io::err)?;
        Ok(Self {
            stream,
            secret,
            context: [0; 64],
            deadline,
        })
    }

    pub(super) fn reply<T: Serialize>(mut self, response: &T) {
        let _ = self.send(b'S', response);
    }

    fn send<T: Serialize>(&mut self, direction: u8, message: &T) -> Result<()> {
        let bytes = serde_json::to_vec(message).map_err(io::err)?;
        if bytes.len() > MAX_FRAME_BYTES {
            return Err("Semantic worker message is too large".into());
        }
        let proof = self.proof(direction, &bytes);
        write_all(
            &mut self.stream,
            self.deadline,
            &(bytes.len() as u32).to_be_bytes(),
        )?;
        write_all(&mut self.stream, self.deadline, &bytes)?;
        write_all(&mut self.stream, self.deadline, &proof)
    }

    fn receive<T: DeserializeOwned>(&mut self, direction: u8) -> Result<T> {
        let mut length = [0; 4];
        read_all(&mut self.stream, self.deadline, &mut length)?;
        let length = u32::from_be_bytes(length) as usize;
        if !(1..=MAX_FRAME_BYTES).contains(&length) {
            return Err("Invalid semantic worker message size".into());
        }
        let mut bytes = vec![0; length];
        let mut proof = [0; 32];
        read_all(&mut self.stream, self.deadline, &mut bytes)?;
        read_all(&mut self.stream, self.deadline, &mut proof)?;
        self.verify(direction, &bytes, &proof)?;
        serde_json::from_slice(&bytes).map_err(io::err)
    }

    fn proof(&self, direction: u8, bytes: &[u8]) -> Secret {
        hmac(
            &self.secret,
            &[b"fmem-semantic-v1", &[direction], &self.context, bytes],
        )
    }

    fn verify(&self, direction: u8, bytes: &[u8], proof: &Secret) -> Result<()> {
        let expected = self.proof(direction, bytes);
        let difference = proof
            .iter()
            .zip(expected)
            .fold(0, |different, (actual, expected)| {
                different | (actual ^ expected)
            });
        if difference == 0 {
            Ok(())
        } else {
            Err("Semantic worker authentication failed".into())
        }
    }
}

fn server_exchange(stream: TcpStream, secret: Secret, deadline: Instant) -> Result<Exchange> {
    let mut exchange = Exchange::new(stream, secret, deadline)?;
    read_all(&mut exchange.stream, deadline, &mut exchange.context[..32])?;
    fill(&mut exchange.context[32..])?;
    let proof = exchange.proof(b'H', &[]);
    write_all(&mut exchange.stream, deadline, &exchange.context[32..])?;
    write_all(&mut exchange.stream, deadline, &proof)?;
    Ok(exchange)
}

fn random() -> Result<Secret> {
    let mut key = [0; 32];
    fill(&mut key)?;
    Ok(key)
}

fn fill(buffer: &mut [u8]) -> Result<()> {
    getrandom::fill(buffer).map_err(io::err)
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let time = deadline.saturating_duration_since(Instant::now());
    if time.is_zero() {
        Err("Semantic worker deadline expired".into())
    } else {
        Ok(time)
    }
}

fn read_all(stream: &mut TcpStream, deadline: Instant, mut buffer: &mut [u8]) -> Result<()> {
    while !buffer.is_empty() {
        stream
            .set_read_timeout(Some(remaining(deadline)?))
            .map_err(io::err)?;
        match stream.read(buffer) {
            Ok(0) => return Err("Semantic worker closed the connection".into()),
            Ok(read) => buffer = &mut buffer[read..],
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(io::err(error)),
        }
    }
    Ok(())
}

fn write_all(stream: &mut TcpStream, deadline: Instant, mut buffer: &[u8]) -> Result<()> {
    while !buffer.is_empty() {
        stream
            .set_write_timeout(Some(remaining(deadline)?))
            .map_err(io::err)?;
        match stream.write(buffer) {
            Ok(0) => return Err("Semantic worker closed the connection".into()),
            Ok(written) => buffer = &buffer[written..],
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(io::err(error)),
        }
    }
    Ok(())
}

fn hmac(key: &Secret, parts: &[&[u8]]) -> Secret {
    let mut inner_pad = [0x36; 64];
    let mut outer_pad = [0x5c; 64];
    for index in 0..key.len() {
        inner_pad[index] ^= key[index];
        outer_pad[index] ^= key[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    for part in parts {
        inner.update(part);
    }
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner.finalize());
    outer.finalize().into()
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn check(path: &Path, directory: bool) -> Result<()> {
        let metadata = io::info(path)?.ok_or("Semantic worker is unavailable")?;
        if metadata.is_dir() != directory
            || (!directory && !metadata.is_file())
            || metadata.mode() & 0o077 != 0
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err("Semantic worker endpoint is not private to the current user".into());
        }
        Ok(())
    }

    pub(super) fn check_directory(path: &Path) -> Result<()> {
        check(path, true)
    }

    pub(super) fn check_file(path: &Path) -> Result<()> {
        check(path, false)
    }

    pub(super) fn protect(bytes: &[u8]) -> Result<Vec<u8>> {
        Ok(bytes.to_vec())
    }

    pub(super) fn unprotect(bytes: &[u8]) -> Result<Vec<u8>> {
        Ok(bytes.to_vec())
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::{ptr, slice};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        },
    };

    pub(super) fn check_directory(_path: &Path) -> Result<()> {
        Ok(())
    }

    pub(super) fn check_file(path: &Path) -> Result<()> {
        if !io::file(path)? {
            return Err("Semantic worker is unavailable".into());
        }
        Ok(())
    }

    pub(super) fn protect(bytes: &[u8]) -> Result<Vec<u8>> {
        dpapi(bytes, true)
    }

    pub(super) fn unprotect(bytes: &[u8]) -> Result<Vec<u8>> {
        dpapi(bytes, false)
    }

    fn dpapi(bytes: &[u8], protect: bool) -> Result<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: bytes.len().try_into().map_err(io::err)?,
            pbData: bytes.as_ptr().cast_mut(),
        };
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: ptr::null_mut(),
        };
        let success = unsafe {
            if protect {
                CryptProtectData(
                    &input,
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            } else {
                CryptUnprotectData(
                    &input,
                    ptr::null_mut(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            }
        };
        if success == 0 || output.pbData.is_null() {
            return Err("Cannot authenticate semantic worker for the current Windows user".into());
        }
        let protected =
            unsafe { slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
        unsafe { LocalFree(output.pbData.cast()) };
        Ok(protected)
    }
}

#[cfg(test)]
mod tests;
