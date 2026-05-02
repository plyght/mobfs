use crate::error::{MobfsError, Result};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpStream;
use subtle::ConstantTimeEq;
use x25519_dalek::{EphemeralSecret, PublicKey};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Serialize, Deserialize)]
enum HandshakeFrame {
    ClientHello {
        public_key: [u8; 32],
    },
    ServerHello {
        public_key: [u8; 32],
        auth: [u8; 32],
    },
    ClientAuth {
        auth: [u8; 32],
    },
}

pub struct SecureStream {
    stream: TcpStream,
    send_cipher: ChaCha20Poly1305,
    recv_cipher: ChaCha20Poly1305,
    send_counter: u64,
    recv_counter: u64,
}

impl SecureStream {
    pub fn client(mut stream: TcpStream, token: &str) -> Result<Self> {
        let secret = EphemeralSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret);
        write_plain(
            &mut stream,
            &HandshakeFrame::ClientHello {
                public_key: public.to_bytes(),
            },
        )?;
        let (server_public, server_auth) = match read_plain::<HandshakeFrame>(&mut stream)? {
            HandshakeFrame::ServerHello { public_key, auth } => (public_key, auth),
            _ => {
                return Err(MobfsError::Remote(
                    "invalid encrypted handshake".to_string(),
                ));
            }
        };
        let shared = secret.diffie_hellman(&PublicKey::from(server_public));
        let keys = derive_keys(shared.as_bytes(), token, &public.to_bytes(), &server_public)?;
        let expected = auth_tag(token, b"server", &public.to_bytes(), &server_public)?;
        if server_auth.ct_eq(&expected).unwrap_u8() != 1 {
            return Err(MobfsError::Remote(
                "encrypted handshake authentication failed".to_string(),
            ));
        }
        let client_auth = auth_tag(token, b"client", &public.to_bytes(), &server_public)?;
        write_plain(
            &mut stream,
            &HandshakeFrame::ClientAuth { auth: client_auth },
        )?;
        Ok(Self::new(
            stream,
            keys.client_to_server,
            keys.server_to_client,
        ))
    }

    pub fn server(mut stream: TcpStream, token: &str) -> Result<Self> {
        let client_public = match read_plain::<HandshakeFrame>(&mut stream)? {
            HandshakeFrame::ClientHello { public_key } => public_key,
            _ => {
                return Err(MobfsError::Remote(
                    "invalid encrypted handshake".to_string(),
                ));
            }
        };
        let secret = EphemeralSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret);
        let server_public = public.to_bytes();
        let shared = secret.diffie_hellman(&PublicKey::from(client_public));
        let keys = derive_keys(shared.as_bytes(), token, &client_public, &server_public)?;
        let server_auth = auth_tag(token, b"server", &client_public, &server_public)?;
        write_plain(
            &mut stream,
            &HandshakeFrame::ServerHello {
                public_key: server_public,
                auth: server_auth,
            },
        )?;
        let client_auth = match read_plain::<HandshakeFrame>(&mut stream)? {
            HandshakeFrame::ClientAuth { auth } => auth,
            _ => {
                return Err(MobfsError::Remote(
                    "invalid encrypted handshake".to_string(),
                ));
            }
        };
        let expected = auth_tag(token, b"client", &client_public, &server_public)?;
        if client_auth.ct_eq(&expected).unwrap_u8() != 1 {
            return Err(MobfsError::Remote(
                "encrypted handshake authentication failed".to_string(),
            ));
        }
        Ok(Self::new(
            stream,
            keys.server_to_client,
            keys.client_to_server,
        ))
    }

    fn new(stream: TcpStream, send_key: [u8; 32], recv_key: [u8; 32]) -> Self {
        let _ = stream.set_nodelay(true);
        Self {
            stream,
            send_cipher: ChaCha20Poly1305::new(Key::from_slice(&send_key)),
            recv_cipher: ChaCha20Poly1305::new(Key::from_slice(&recv_key)),
            send_counter: 0,
            recv_counter: 0,
        }
    }

    pub fn read_encrypted(&mut self) -> Result<Vec<u8>> {
        let data = read_raw(&mut self.stream)?;
        let nonce = nonce(self.recv_counter);
        self.recv_counter = self.recv_counter.saturating_add(1);
        self.recv_cipher
            .decrypt(&nonce, data.as_ref())
            .map_err(|_| MobfsError::Remote("encrypted frame authentication failed".to_string()))
    }

    pub fn write_encrypted(&mut self, data: &[u8]) -> Result<()> {
        let nonce = nonce(self.send_counter);
        self.send_counter = self.send_counter.saturating_add(1);
        let encrypted = self
            .send_cipher
            .encrypt(&nonce, data)
            .map_err(|_| MobfsError::Remote("encrypted frame failed".to_string()))?;
        write_raw(&mut self.stream, &encrypted)
    }
}

struct DirectionKeys {
    client_to_server: [u8; 32],
    server_to_client: [u8; 32],
}

fn derive_keys(
    shared: &[u8],
    token: &str,
    client_public: &[u8; 32],
    server_public: &[u8; 32],
) -> Result<DirectionKeys> {
    let salt = Sha256::digest(token.as_bytes());
    let hk = Hkdf::<Sha256>::new(Some(&salt), shared);
    let mut client_to_server = [0_u8; 32];
    let mut server_to_client = [0_u8; 32];
    let mut info = Vec::with_capacity(85);
    info.extend_from_slice(b"mobfs-e2ee-v2");
    info.extend_from_slice(client_public);
    info.extend_from_slice(server_public);
    info.extend_from_slice(b"client-to-server");
    hk.expand(&info, &mut client_to_server)
        .map_err(|_| MobfsError::Remote("encrypted key derivation failed".to_string()))?;
    info.truncate(b"mobfs-e2ee-v2".len() + 64);
    info.extend_from_slice(b"server-to-client");
    hk.expand(&info, &mut server_to_client)
        .map_err(|_| MobfsError::Remote("encrypted key derivation failed".to_string()))?;
    Ok(DirectionKeys {
        client_to_server,
        server_to_client,
    })
}

fn auth_tag(
    token: &str,
    label: &[u8],
    client_public: &[u8; 32],
    server_public: &[u8; 32],
) -> Result<[u8; 32]> {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(token.as_bytes())
        .map_err(|_| MobfsError::Remote("encrypted authentication failed".to_string()))?;
    mac.update(b"mobfs-e2ee-auth-v1");
    mac.update(label);
    mac.update(client_public);
    mac.update(server_public);
    Ok(mac.finalize().into_bytes().into())
}

fn nonce(counter: u64) -> Nonce {
    let mut nonce = [0_u8; 12];
    nonce[4..].copy_from_slice(&counter.to_be_bytes());
    *Nonce::from_slice(&nonce)
}

fn read_plain<T: for<'de> Deserialize<'de>>(stream: &mut TcpStream) -> Result<T> {
    Ok(serde_json::from_slice(&read_raw(stream)?)?)
}

fn write_plain<T: Serialize>(stream: &mut TcpStream, value: &T) -> Result<()> {
    write_raw(stream, &serde_json::to_vec(value)?)
}

fn read_raw(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut len = [0_u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > 128 * 1024 * 1024 {
        return Err(MobfsError::Remote("protocol frame too large".to_string()));
    }
    let mut data = vec![0_u8; len];
    stream.read_exact(&mut data)?;
    Ok(data)
}

fn write_raw(stream: &mut TcpStream, data: &[u8]) -> Result<()> {
    let len = u32::try_from(data.len())
        .map_err(|_| MobfsError::Remote("protocol frame too large".to_string()))?;
    let mut frame = Vec::with_capacity(4 + data.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(data);
    stream.write_all(&frame)?;
    Ok(())
}
