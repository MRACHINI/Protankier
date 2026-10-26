use std::{net::SocketAddr, task::{Poll, Context, Waker}, io::Cursor, pin::Pin};
use std::io::Read as _;

use crate::{crypto::{Cipher, PlainCipher, XOrCipher, CipherMode}, packets, ProtocolError, ProtocolResult, ConnectionClosedError, Socket};
use crate::packets::{Packet, PacketRegistry, PacketDowncast};
use byteorder::{ReadBytesExt, BigEndian, WriteBytesExt};
use flate2::read::DeflateDecoder;
use futures::prelude::*;
use tracing::{warn, trace, debug};

pub trait PacketDebugFilter : Send {
    fn should_log(&self, is_send: bool, packet: &dyn Packet) -> bool;
}

#[derive(Debug, Clone)]
pub struct SimplePacketDebugFilter {
    enabled: bool,
}

impl SimplePacketDebugFilter {
    pub fn logging_enabled() -> Self {
        Self{ enabled: true }
    }

    pub fn logging_disabled() -> Self {
        Self{ enabled: false }
    }
}

impl PacketDebugFilter for SimplePacketDebugFilter {
    fn should_log(&self, _is_send: bool, _packet: &dyn Packet) -> bool {
        self.enabled
    }
}

#[derive(Debug, Clone)]
pub struct ModelPacketDebugFilter {
    model_ids: Vec<u32>,
    whitelist: bool,
}

impl ModelPacketDebugFilter {
    pub fn whitelist(model_ids: Vec<u32>) -> Self {
        Self { model_ids, whitelist: true }
    }
    
    pub fn blacklist(model_ids: Vec<u32>) -> Self {
        Self { model_ids, whitelist: false }
    }
}

impl PacketDebugFilter for ModelPacketDebugFilter {
    fn should_log(&self, _is_send: bool, packet: &dyn Packet) -> bool {
        if self.model_ids.contains(&packet.model_id()) {
            return self.whitelist;
        } else {
            return !self.whitelist;
        }
    }
}


pub type ConnectionStreamItem = std::result::Result<Box<dyn Packet>, ProtocolError>;

/// Simple tanks connection capeable of sending and receiving packets.
pub struct Connection {
    pub address: SocketAddr,
    socket: Box<dyn Socket + Send>,

    is_server: bool,
    crypt_context: Box<dyn Cipher>,

    disconnected: bool,
    log_filter: Box<dyn PacketDebugFilter>,

    recv_buffer: Vec<u8>,
    recv_buffer_index: usize,

    send_buffer: Vec<u8>,
    send_waker: Option<Waker>,

    packet_registry: PacketRegistry,
}

impl Connection {
    pub fn new(is_server: bool, address: SocketAddr, socket: Box<dyn Socket + Send>, log_filter: Box<dyn PacketDebugFilter>) -> Self {
        let mut instance = Self {
            address,
            socket,

            is_server,
            crypt_context: Box::new(PlainCipher::new(if is_server { CipherMode::Server } else { CipherMode::Client })),

            disconnected: false,
            log_filter,

            recv_buffer: Vec::with_capacity(1024 * 16),
            recv_buffer_index: 0,

            send_buffer: Vec::with_capacity(1024 * 16),
            send_waker: None,

            packet_registry: PacketRegistry::new()
        };

        /* register all packets */
        if is_server {
            packets::c2s::register_all_packets(&mut instance.packet_registry);
        } else {
            packets::s2c::register_all_packets(&mut instance.packet_registry);
        }
        
        instance
    }

    pub fn allow_unknown_packets(&mut self) {
        self.packet_registry.allow_unknown_packets();
    }

    pub fn send_packet(&mut self, packet: &dyn Packet) -> ProtocolResult<()> {
        let mut buffer = Vec::with_capacity(1024);
        let mut cursor = Cursor::new(&mut buffer);
        cursor.write_u32::<BigEndian>(0)?; /* total length can only be known after writing */
        cursor.write_u32::<BigEndian>(packet.packet_id())?;

        packet.encode(&mut cursor)?;
        
        let packet_length = cursor.position() as usize;
        cursor.set_position(0);
        cursor.write_u32::<BigEndian>(packet_length as u32)?;
        buffer.truncate(packet_length);

        if let Err(error) = self.crypt_context.encrypt(&mut buffer[8..]) {
            return Err(error.into());
        }

        self.send_buffer.append(&mut buffer);
        if let Some(waker) = self.send_waker.take() {
            waker.wake();
        }
        
        if self.log_filter.should_log(true, packet) {
            trace!("[OUT] {: >11} {: >2} {:?} ({} bytes)", packet.packet_id() as i32, packet.model_id(), packet, packet_length - 8);
        }
        Ok(())
    }

    pub async fn init_encryption(&mut self) -> ProtocolResult<()> {
        if self.is_server {
            let key = XOrCipher::generate_key();
            let crypt_context = XOrCipher::new(CipherMode::Server, &key);
            self.send_packet(&packets::s2c::ResourceLoaderInitializeEncryption{ protection_data: key })?;

            self.crypt_context = Box::new(crypt_context);
            Ok(())
        } else {
            let packet = match self.next().await {
                Some(Ok(packet)) => packet,
                Some(Err(event)) => return Err(event),
                None => return Err(ProtocolError::ConnectionAborted),
            };
    
            let packet = match packet.downcast_ref::<packets::s2c::ResourceLoaderInitializeEncryption>() {
                Some(packet) => packet,
                None => return Err(ProtocolError::UnexpectedPacket),
            };
    
            self.crypt_context = Box::new(XOrCipher::new(CipherMode::Client, &packet.protection_data));
            Ok(())
        }
    }

    pub fn is_send_buffer_clear(&self) -> bool {
        self.send_buffer.is_empty()
    }

    /// Drops the first `packet_length` bytes, which must already be known to be cached.
    fn consume_frame(&mut self, packet_length: usize) {
        self.recv_buffer.copy_within(packet_length.., 0);
        self.recv_buffer_index -= packet_length;
    }

    fn try_parse_read_buffer(&mut self) -> Poll<ProtocolResult<Box<dyn Packet>>> {
        /* Each iteration consumes exactly one whole frame, so a frame we throw away still
           makes progress: the loop always terminates. */
        loop {
            if self.recv_buffer_index < 8 {
                return Poll::Pending
            }

            /* The header is the only thing we cannot recover from. Once its length field has
               been read and sanity-checked, the frame boundary is known exactly, so a payload
               we fail to decode can be dropped without losing the stream. */
            let mut reader = Cursor::new(&self.recv_buffer[0..self.recv_buffer_index]);
            let header = reader.read_u32::<BigEndian>()?;
            let packet_id = reader.read_u32::<BigEndian>()?;

            // The first byte carries flags (0x40 = raw-deflate compressed); only the
            // lower 24 bits are the packet length, including the 8-byte header.
            let packet_is_compressed = header & 0x4000_0000 != 0;
            let packet_length = (header & 0x00FF_FFFF) as usize;

            if packet_length > 1024 * 1024 * 64 {
                return Poll::Ready(Err(ProtocolError::PacketTooLarge(packet_length)));
            } else if packet_length < 8 {
                return Poll::Ready(Err(ProtocolError::PacketTooSmall(packet_length)));
            }

            if self.recv_buffer_index < packet_length {
                if self.recv_buffer.len() < packet_length {
                    self.recv_buffer.resize(packet_length, 0);
                }

                return Poll::Pending;
            }

            let payload_offset = reader.position() as usize;
            let payload_range = payload_offset..packet_length;
            let trace_frames = std::env::var_os("FROST_TRACE_FRAMES").is_some();
            let raw_head: Vec<u8> = if trace_frames {
                self.recv_buffer[payload_range.clone()].iter().take(16).copied().collect()
            } else {
                Vec::new()
            };

            /* Decryption, decompression and decoding can each fail on one bad frame. The
               server drops us otherwise: a session must not end just because a single packet
               does not match our codec table. The frame boundary is already known, so every
               failure below skips exactly one frame and keeps the stream in sync. */

            /* The cipher owns stream state, so a frame must be decrypted even if it is then
               discarded; `recv_buffer` and `crypt_context` are disjoint fields. */
            let decrypt_result = {
                let payload = &mut self.recv_buffer[payload_range.clone()];
                self.crypt_context.decrypt(payload)
            };
            if let Err(error) = decrypt_result {
                warn!(
                    "Skipped packet id {} ({} bytes) that failed to decrypt: {}",
                    packet_id as i32, packet_length, error
                );
                self.consume_frame(packet_length);
                continue;
            }

            let decompressed_payload: Vec<u8> = if packet_is_compressed {
                let payload = &self.recv_buffer[payload_range.clone()];
                let mut decoder = DeflateDecoder::new(payload);
                let mut decompressed = Vec::with_capacity(payload.len() * 4);
                match decoder.read_to_end(&mut decompressed) {
                    Ok(_) => decompressed,
                    Err(error) => {
                        warn!(
                            "Skipped packet id {} ({} bytes) that failed to decompress: {}",
                            packet_id as i32, packet_length, error
                        );
                        self.consume_frame(packet_length);
                        continue;
                    }
                }
            } else {
                self.recv_buffer[payload_range.clone()].to_vec()
            };

            let mut packet_reader = Cursor::new(&decompressed_payload[..]);
            let packet = match self.packet_registry.decode(&mut packet_reader, packet_id) {
                Ok(packet) => packet,
                Err(error) => {
                    /* Framing is intact, so skip this frame only. Logging the id makes the
                       broken codec directly identifiable. */
                    warn!(
                        "Skipped undecodable packet id {} ({} bytes): {}",
                        packet_id as i32, packet_length, error
                    );
                    self.consume_frame(packet_length);
                    continue;
                }
            };
            let consumed = packet_reader.position();

            if trace_frames {
                let full_hex: String = if std::env::var_os("FROST_TRACE_FULL").is_some() && decompressed_payload.len() <= 512 {
                    decompressed_payload.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ")
                } else {
                    String::new()
                };
                eprintln!("[FRAME] len={} id={} compressed={} raw_head={:02x?} decrypted_head={:02x?} decompressed_len={} full=[{}]", packet_length, packet_id as i32, packet_is_compressed, raw_head, &decompressed_payload[..decompressed_payload.len().min(16)], decompressed_payload.len(), full_hex);
            }

            /* Leftover bytes mean the packet decoded but our layout is missing a field. That
               is the silent predecessor of a hard decode failure, so report it: it points at
               the codec that needs fixing. */
            if consumed < decompressed_payload.len() as u64 {
                warn!(
                    "Packet id {} decoded {} of {} bytes ({} bytes left over; codec layout may be out of date).",
                    packet_id as i32,
                    consumed,
                    decompressed_payload.len(),
                    decompressed_payload.len() as u64 - consumed
                );
            }

            if self.log_filter.should_log(false, Box::as_ref(&packet)) {
                trace!("[IN ] {: >11} {: >2} {:?} ({} bytes)", packet.packet_id() as i32, packet.model_id(), packet, decompressed_payload.len());
            }

            self.consume_frame(packet_length);
            return Poll::Ready(Ok(packet));
        }
    }

    fn poll_incoming(&mut self, cx: &mut Context) -> Poll<ConnectionStreamItem> {
        if self.recv_buffer.len() - self.recv_buffer_index < 1024 {
            self.recv_buffer.resize(self.recv_buffer.len() + 1024, 0);
        }

        loop {
            match self.try_parse_read_buffer() {
                Poll::Ready(Ok(packet)) => return Poll::Ready(Ok(packet)),
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => { /* not yet enough data */}
            }

            match self.socket.poll_recv(cx, &mut self.recv_buffer[self.recv_buffer_index..]) {
                Poll::Ready(Ok(length)) => {
                    if length == 0 {
                        return Poll::Ready(Err(ConnectionClosedError::Disconnected.into()))
                    }
                    
                    self.recv_buffer_index += length;
                },
                Poll::Ready(Err(error)) => return Poll::Ready(Err(ConnectionClosedError::ReadError(error).into())),
                Poll::Pending => return Poll::Pending
            }
        }
    }

    fn poll_outgoing(&mut self, cx: &mut Context) -> Poll<ConnectionStreamItem> {
        self.send_waker.replace(cx.waker().clone());
        while !self.send_buffer.is_empty() {
            match self.socket.poll_send(cx, &self.send_buffer) {
                Poll::Ready(Ok(length)) => {
                    self.send_buffer.copy_within(length.., 0);
                    self.send_buffer.truncate(self.send_buffer.len() - length);
                },
                Poll::Ready(Err(error)) => return Poll::Ready(Err(ConnectionClosedError::WriteError(error).into())),
                Poll::Pending => return Poll::Pending
            }
        }
        return Poll::Pending;
    }

    fn poll_io(&mut self, cx: &mut Context) -> Poll<ConnectionStreamItem> {
        match self.poll_outgoing(cx) {
            Poll::Ready(item) => return Poll::Ready(item),
            Poll::Pending => { }
        }

        match self.poll_incoming(cx) {
            Poll::Ready(item) => return Poll::Ready(item),
            Poll::Pending => {}
        }

        return Poll::Pending;
    }
}

impl Stream for Connection {
	type Item = ConnectionStreamItem;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.disconnected {
            return Poll::Ready(None);
        }

        match self.poll_io(cx) {
            Poll::Ready(Ok(item)) => Poll::Ready(Some(Ok(item))),
            Poll::Ready(Err(error)) => {
                debug!("connection error: {:?}", error);
                self.disconnected = true;
                return Poll::Ready(Some(Err(error)));
            },
            Poll::Pending => Poll::Pending
        }
    }
}