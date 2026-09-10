//! The wire format between the VMM and an isolated renderer process
//! (ADR-0004's GPU-012 amendment).
//!
//! **Portable**: this module builds and is unit-tested on every host OS, which
//! is the point — the containment architecture must not be a Linux-shaped hole
//! in the design (ADR-0002). Only the transport (a Unix socket pair today) and
//! the server's renderer are OS-gated.
//!
//! # Shape
//!
//! Strict request/response over a stream, one reply per request, in order. No
//! server-initiated messages: fence retirement is *polled*
//! ([`Request::PollFences`]) exactly as the in-process renderer polls
//! virglrenderer, so the client never has to demultiplex.
//!
//! Every frame is `[u8 tag][u32 payload_len][payload]`, little-endian, with
//! `payload_len` bounded by [`MAX_FRAME_BYTES`] on **both** sides. That bound
//! is not politeness: the helper is the process that runs guest-derived GL
//! work, so the VMM treats everything it says as untrusted input — a hostile
//! or corrupted helper must not be able to make the VMM allocate without
//! limit, and a decode failure is reported as a lost renderer (GPU-012's
//! degrade path) rather than a panic.
//!
//! # Why bytes and not guest addresses
//!
//! The helper never learns a guest physical address. Transfers carry the
//! *bytes* the client read out of guest memory through the checked
//! `vm-memory` API, and the helper keeps a host-side shadow of each resource's
//! backing. The isolated process therefore has no window onto guest RAM, which
//! is the security half of the containment story.

use crate::blob::BlobSupport;
use crate::protocol::{Box3d, ResourceCreate3d, ResourceCreateBlob, Transfer3d};
use crate::renderer::CapsetInfo;

/// Largest payload either side will send or accept in one frame.
///
/// Sized by the two things that are actually big: a `SUBMIT_3D` stream
/// ([`crate::renderer::MAX_SUBMIT_BYTES`], 1 MiB) and a scanout rect
/// ([`crate::MAX_RESOURCE_PIXELS`] × 4 = 36 MiB at the 4096×2304 cap). 40 MiB
/// covers both with room for the fixed part.
pub const MAX_FRAME_BYTES: usize = 40 << 20;

/// Most bytes of a resource's backing store that travel in one transfer.
///
/// A transfer names a box, an offset and a stride, but *not* a
/// bytes-per-element — the format is a Gallium enum the renderer owns — so the
/// client cannot compute the exact span. It sends a bounded window from the
/// transfer's offset instead, which is correct for every real transfer (mesa's
/// uploads are far smaller) and bounds what one guest command can push through
/// the socket.
pub const REMOTE_XFER_WINDOW: usize = 8 << 20;

/// Largest backing store the helper will shadow for one resource. A guest that
/// attaches more than this gets `ERR_OUT_OF_MEMORY` in band, which is honest:
/// the isolated renderer really cannot hold it.
pub const REMOTE_MAX_BACKING: u64 = 64 << 20;

/// Largest total of all shadow backings the helper holds at once.
///
/// The shadow is the one place isolation *adds* host memory: in-process, a
/// resource's backing is guest RAM the guest already paid for, while here the
/// helper holds a copy. Without a total bound a guest could attach thousands
/// of resources and make the helper commit unbounded memory — so the same
/// MVP-1407 rule applies as everywhere else, and an attach past the budget is
/// refused in band (`ERR_OUT_OF_MEMORY`).
///
/// 512 MiB is far above what a composited desktop attaches (mesa's buffers
/// are small; the framebuffer goes through the 2D path) and far below what
/// would hurt a host running a 4 GiB VM.
pub const REMOTE_MAX_TOTAL_SHADOW: u64 = 512 << 20;

/// Message tags. Explicit values because this is a wire format between two
/// processes that may be different builds during a rolling upgrade — a
/// reordered enum must not silently become a different command.
pub mod tag {
    pub const HELLO: u8 = 0x01;
    pub const CAPSET: u8 = 0x02;
    pub const CTX_CREATE: u8 = 0x03;
    pub const CTX_DESTROY: u8 = 0x04;
    pub const RESOURCE_CREATE: u8 = 0x05;
    pub const RESOURCE_UNREF: u8 = 0x06;
    pub const CTX_ATTACH: u8 = 0x07;
    pub const CTX_DETACH: u8 = 0x08;
    pub const ATTACH_BACKING: u8 = 0x09;
    pub const DETACH_BACKING: u8 = 0x0a;
    pub const TRANSFER_TO_HOST: u8 = 0x0b;
    pub const TRANSFER_FROM_HOST: u8 = 0x0c;
    pub const SUBMIT: u8 = 0x0d;
    pub const READ_RECT: u8 = 0x0e;
    pub const RESET: u8 = 0x0f;
    pub const CREATE_FENCE: u8 = 0x10;
    pub const POLL_FENCES: u8 = 0x11;
    // Blob resources (VEN-2001), protocol version 2.
    pub const CREATE_BLOB: u8 = 0x12;
    pub const DESTROY_BLOB: u8 = 0x13;
    pub const MAP_BLOB: u8 = 0x14;
    pub const UNMAP_BLOB: u8 = 0x15;
    pub const BLOB_SUPPORT: u8 = 0x16;

    // Replies.
    pub const OK: u8 = 0x80;
    pub const BYTES: u8 = 0x81;
    pub const CAPSETS: u8 = 0x82;
    pub const FENCE: u8 = 0x83;
    pub const FENCES: u8 = 0x84;
    pub const ERROR: u8 = 0x85;
    /// Reply to [`super::Request::MapBlob`]: the caching the guest must use.
    pub const MAPPING: u8 = 0x86;
    /// Reply to [`super::Request::BlobSupport`].
    pub const BLOB_SUPPORT_REPLY: u8 = 0x87;
}

/// Protocol version, exchanged in [`Request::Hello`]. A mismatch fails the
/// handshake instead of misparsing later.
///
/// Bumped to 2 by VEN-2001: `CtxCreate` grew a `capset_id` and four blob
/// messages appeared. There is deliberately no compatibility shim — the VMM
/// and its helper are the same build, `entangled gpu-renderer` is spawned from
/// the running binary's own path, and a mismatch is a packaging bug that
/// should fail loudly at handshake rather than quietly at the first venus
/// context.
pub const VERSION: u32 = 3;

/// A request from the VMM to the renderer process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Handshake: protocol version, and whether the helper must refuse to fall
    /// back to a non-drawing renderer (production always requires a real one).
    Hello {
        version: u32,
    },
    Capset {
        id: u32,
        version: u32,
    },
    CtxCreate {
        ctx_id: u32,
        /// Context type as a capset id, 0 for classic virgl (VEN-2002).
        capset_id: u32,
        name: String,
    },
    CtxDestroy {
        ctx_id: u32,
    },
    ResourceCreate(ResourceCreate3d),
    ResourceUnref {
        resource_id: u32,
    },
    CtxAttach {
        ctx_id: u32,
        resource_id: u32,
    },
    CtxDetach {
        ctx_id: u32,
        resource_id: u32,
    },
    /// Allocate a host-side shadow of this resource's guest backing.
    AttachBacking {
        resource_id: u32,
        len: u64,
    },
    DetachBacking {
        resource_id: u32,
    },
    /// Guest → host: `bytes` are the guest's backing contents starting at
    /// `shadow_offset`, followed by the transfer itself.
    TransferToHost {
        ctx_id: u32,
        xfer: Transfer3d,
        shadow_offset: u64,
        bytes: Vec<u8>,
    },
    /// Host → guest: perform the transfer, then return `len` bytes of the
    /// shadow from `shadow_offset` for the client to write into guest pages.
    TransferFromHost {
        ctx_id: u32,
        xfer: Transfer3d,
        shadow_offset: u64,
        len: u32,
    },
    Submit {
        ctx_id: u32,
        stream: Vec<u8>,
    },
    ReadRect {
        resource_id: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
    Reset,
    CreateFence {
        ctx_id: u32,
        fence_id: u32,
    },
    PollFences,

    // ------------------------------------ blob resources (VEN-2001)
    /// Create a host-side blob. Guest-memory blobs never cross this boundary:
    /// they are guest pages the device tracks itself, and the isolated helper
    /// has no window onto guest RAM by construction (GPU-012).
    CreateBlob {
        /// The context the blob is named on — see
        /// [`Renderer3d::create_blob`](crate::renderer::Renderer3d::create_blob).
        /// New in protocol version 3 (VEN-2003).
        ctx_id: u32,
        args: ResourceCreateBlob,
    },
    DestroyBlob {
        resource_id: u32,
    },
    /// Back `size` bytes of the shared-memory window at `offset` with this
    /// blob's host memory.
    MapBlob {
        resource_id: u32,
        offset: u64,
        size: u64,
    },
    UnmapBlob {
        resource_id: u32,
        offset: u64,
    },
    /// Asked once, right after the handshake: what the helper's renderer can
    /// do with blobs. The device needs the answer *before* it decides which
    /// feature bits to offer, and the client cannot guess it.
    BlobSupport,
}

/// A reply from the renderer process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Ok,
    /// Capset blob, transfer readback or scanout pixels.
    Bytes(Vec<u8>),
    /// The capsets the helper's renderer serves, sent once at handshake so the
    /// device can fill in `num_capsets` before any 3D command exists.
    Capsets(Vec<CapsetInfo>),
    /// Whether the created fence is pending on the host timeline.
    Fence {
        pending: bool,
    },
    /// The caching a mapped blob must be accessed with (VEN-2001).
    Mapping {
        map_info: u32,
    },
    /// What the helper's renderer can do with blobs (VEN-2001).
    BlobSupport(BlobSupport),
    /// Host fences that have retired, oldest first.
    Fences(Vec<u32>),
    /// The renderer refused this command; the device answers the guest in band.
    Error(String),
}

/// Why a frame could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// The tag byte is not one this build knows.
    UnknownTag(u8),
    /// The payload ended in the middle of a field.
    Truncated,
    /// `payload_len` exceeds [`MAX_FRAME_BYTES`].
    TooLarge(usize),
    /// A string field was not UTF-8.
    BadString,
    /// A field held a value this format does not produce — a boolean byte
    /// other than 0 or 1.
    ///
    /// Both ends of this protocol are ours, so the strict reading is the
    /// useful one: it makes the encoding canonical (every message has exactly
    /// one byte sequence), which is what lets the fuzzer assert that decode
    /// and encode are inverses. The fuzzer found this: `pending: 10` and
    /// `pending: 1` both meant "true" and re-encoded differently.
    NonCanonical,
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTag(tag) => write!(f, "unknown message tag {tag:#04x}"),
            Self::Truncated => write!(f, "message payload is truncated"),
            Self::TooLarge(len) => write!(f, "message payload of {len} bytes is too large"),
            Self::BadString => write!(f, "message contains a non-UTF-8 string"),
            Self::NonCanonical => write!(f, "message contains a non-canonical field value"),
        }
    }
}

impl std::error::Error for CodecError {}

// --------------------------------------------------------------- encoding

/// Little-endian writer. No serde: the format is a dozen fixed-width fields
/// and two byte blobs, and a hand-written codec is the whole reason this
/// module can be read in one sitting and fuzzed without a schema.
#[derive(Debug, Default)]
struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u32(u32::try_from(v.len()).unwrap_or(u32::MAX));
        self.0.extend_from_slice(v);
        self
    }
    fn box3d(&mut self, b: &Box3d) -> &mut Self {
        self.u32(b.x).u32(b.y).u32(b.z).u32(b.w).u32(b.h).u32(b.d)
    }
    fn xfer(&mut self, x: &Transfer3d) -> &mut Self {
        self.box3d(&x.region)
            .u64(x.offset)
            .u32(x.resource_id)
            .u32(x.level)
            .u32(x.stride)
            .u32(x.layer_stride)
    }
    fn create_blob(&mut self, c: &ResourceCreateBlob) -> &mut Self {
        self.u32(c.resource_id)
            .u32(c.blob_mem)
            .u32(c.blob_flags)
            .u32(c.nr_entries)
            .u64(c.blob_id)
            .u64(c.size)
    }
    fn create(&mut self, c: &ResourceCreate3d) -> &mut Self {
        self.u32(c.resource_id)
            .u32(c.target)
            .u32(c.format)
            .u32(c.bind)
            .u32(c.width)
            .u32(c.height)
            .u32(c.depth)
            .u32(c.array_size)
            .u32(c.last_level)
            .u32(c.nr_samples)
            .u32(c.flags)
    }
}

/// Little-endian reader that never panics and never over-reads.
#[derive(Debug)]
struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, at: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], CodecError> {
        let end = self.at.checked_add(len).ok_or(CodecError::Truncated)?;
        let slice = self.buf.get(self.at..end).ok_or(CodecError::Truncated)?;
        self.at = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, CodecError> {
        let raw: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| CodecError::Truncated)?;
        Ok(u32::from_le_bytes(raw))
    }

    fn u64(&mut self) -> Result<u64, CodecError> {
        let raw: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| CodecError::Truncated)?;
        Ok(u64::from_le_bytes(raw))
    }

    fn bytes(&mut self) -> Result<Vec<u8>, CodecError> {
        let len = self.u32()? as usize;
        if len > MAX_FRAME_BYTES {
            return Err(CodecError::TooLarge(len));
        }
        Ok(self.take(len)?.to_vec())
    }

    fn string(&mut self) -> Result<String, CodecError> {
        let raw = self.bytes()?;
        String::from_utf8(raw).map_err(|_| CodecError::BadString)
    }

    fn box3d(&mut self) -> Result<Box3d, CodecError> {
        Ok(Box3d {
            x: self.u32()?,
            y: self.u32()?,
            z: self.u32()?,
            w: self.u32()?,
            h: self.u32()?,
            d: self.u32()?,
        })
    }

    fn xfer(&mut self) -> Result<Transfer3d, CodecError> {
        Ok(Transfer3d {
            region: self.box3d()?,
            offset: self.u64()?,
            resource_id: self.u32()?,
            level: self.u32()?,
            stride: self.u32()?,
            layer_stride: self.u32()?,
        })
    }

    fn create_blob(&mut self) -> Result<ResourceCreateBlob, CodecError> {
        Ok(ResourceCreateBlob {
            resource_id: self.u32()?,
            blob_mem: self.u32()?,
            blob_flags: self.u32()?,
            nr_entries: self.u32()?,
            blob_id: self.u64()?,
            size: self.u64()?,
        })
    }
    fn create(&mut self) -> Result<ResourceCreate3d, CodecError> {
        Ok(ResourceCreate3d {
            resource_id: self.u32()?,
            target: self.u32()?,
            format: self.u32()?,
            bind: self.u32()?,
            width: self.u32()?,
            height: self.u32()?,
            depth: self.u32()?,
            array_size: self.u32()?,
            last_level: self.u32()?,
            nr_samples: self.u32()?,
            flags: self.u32()?,
        })
    }
}

impl Request {
    /// The message tag this request encodes as.
    pub fn tag(&self) -> u8 {
        match self {
            Self::Hello { .. } => tag::HELLO,
            Self::Capset { .. } => tag::CAPSET,
            Self::CtxCreate { .. } => tag::CTX_CREATE,
            Self::CtxDestroy { .. } => tag::CTX_DESTROY,
            Self::ResourceCreate(_) => tag::RESOURCE_CREATE,
            Self::ResourceUnref { .. } => tag::RESOURCE_UNREF,
            Self::CtxAttach { .. } => tag::CTX_ATTACH,
            Self::CtxDetach { .. } => tag::CTX_DETACH,
            Self::AttachBacking { .. } => tag::ATTACH_BACKING,
            Self::DetachBacking { .. } => tag::DETACH_BACKING,
            Self::TransferToHost { .. } => tag::TRANSFER_TO_HOST,
            Self::TransferFromHost { .. } => tag::TRANSFER_FROM_HOST,
            Self::Submit { .. } => tag::SUBMIT,
            Self::ReadRect { .. } => tag::READ_RECT,
            Self::Reset => tag::RESET,
            Self::CreateFence { .. } => tag::CREATE_FENCE,
            Self::PollFences => tag::POLL_FENCES,
            Self::CreateBlob { .. } => tag::CREATE_BLOB,
            Self::DestroyBlob { .. } => tag::DESTROY_BLOB,
            Self::MapBlob { .. } => tag::MAP_BLOB,
            Self::UnmapBlob { .. } => tag::UNMAP_BLOB,
            Self::BlobSupport => tag::BLOB_SUPPORT,
        }
    }

    /// Encodes the payload (without tag or length; see [`super::frame`]).
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        match self {
            Self::Hello { version } => {
                w.u32(*version);
            }
            Self::Capset { id, version } => {
                w.u32(*id).u32(*version);
            }
            Self::CtxCreate {
                ctx_id,
                capset_id,
                name,
            } => {
                w.u32(*ctx_id).u32(*capset_id).bytes(name.as_bytes());
            }
            Self::CtxDestroy { ctx_id } => {
                w.u32(*ctx_id);
            }
            Self::ResourceCreate(args) => {
                w.create(args);
            }
            Self::ResourceUnref { resource_id } => {
                w.u32(*resource_id);
            }
            Self::CtxAttach {
                ctx_id,
                resource_id,
            }
            | Self::CtxDetach {
                ctx_id,
                resource_id,
            } => {
                w.u32(*ctx_id).u32(*resource_id);
            }
            Self::AttachBacking { resource_id, len } => {
                w.u32(*resource_id).u64(*len);
            }
            Self::DetachBacking { resource_id } => {
                w.u32(*resource_id);
            }
            Self::TransferToHost {
                ctx_id,
                xfer,
                shadow_offset,
                bytes,
            } => {
                w.u32(*ctx_id).xfer(xfer).u64(*shadow_offset).bytes(bytes);
            }
            Self::TransferFromHost {
                ctx_id,
                xfer,
                shadow_offset,
                len,
            } => {
                w.u32(*ctx_id).xfer(xfer).u64(*shadow_offset).u32(*len);
            }
            Self::Submit { ctx_id, stream } => {
                w.u32(*ctx_id).bytes(stream);
            }
            Self::ReadRect {
                resource_id,
                x,
                y,
                width,
                height,
            } => {
                w.u32(*resource_id).u32(*x).u32(*y).u32(*width).u32(*height);
            }
            Self::Reset => (),
            Self::CreateFence { ctx_id, fence_id } => {
                w.u32(*ctx_id).u32(*fence_id);
            }
            Self::PollFences => (),
            Self::CreateBlob { ctx_id, args } => {
                w.u32(*ctx_id).create_blob(args);
            }
            Self::DestroyBlob { resource_id } => {
                w.u32(*resource_id);
            }
            Self::MapBlob {
                resource_id,
                offset,
                size,
            } => {
                w.u32(*resource_id).u64(*offset).u64(*size);
            }
            Self::UnmapBlob {
                resource_id,
                offset,
            } => {
                w.u32(*resource_id).u64(*offset);
            }
            Self::BlobSupport => (),
        }
        w.0
    }

    /// Decodes a request. Trailing bytes are an error: a frame that does not
    /// consume exactly is a protocol mismatch, not something to guess at.
    pub fn decode(tag: u8, payload: &[u8]) -> Result<Self, CodecError> {
        let mut r = Reader::new(payload);
        let request = match tag {
            tag::HELLO => Self::Hello { version: r.u32()? },
            tag::CAPSET => Self::Capset {
                id: r.u32()?,
                version: r.u32()?,
            },
            tag::CTX_CREATE => Self::CtxCreate {
                ctx_id: r.u32()?,
                capset_id: r.u32()?,
                name: r.string()?,
            },
            tag::CTX_DESTROY => Self::CtxDestroy { ctx_id: r.u32()? },
            tag::RESOURCE_CREATE => Self::ResourceCreate(r.create()?),
            tag::RESOURCE_UNREF => Self::ResourceUnref {
                resource_id: r.u32()?,
            },
            tag::CTX_ATTACH => Self::CtxAttach {
                ctx_id: r.u32()?,
                resource_id: r.u32()?,
            },
            tag::CTX_DETACH => Self::CtxDetach {
                ctx_id: r.u32()?,
                resource_id: r.u32()?,
            },
            tag::ATTACH_BACKING => Self::AttachBacking {
                resource_id: r.u32()?,
                len: r.u64()?,
            },
            tag::DETACH_BACKING => Self::DetachBacking {
                resource_id: r.u32()?,
            },
            tag::TRANSFER_TO_HOST => Self::TransferToHost {
                ctx_id: r.u32()?,
                xfer: r.xfer()?,
                shadow_offset: r.u64()?,
                bytes: r.bytes()?,
            },
            tag::TRANSFER_FROM_HOST => Self::TransferFromHost {
                ctx_id: r.u32()?,
                xfer: r.xfer()?,
                shadow_offset: r.u64()?,
                len: r.u32()?,
            },
            tag::SUBMIT => Self::Submit {
                ctx_id: r.u32()?,
                stream: r.bytes()?,
            },
            tag::READ_RECT => Self::ReadRect {
                resource_id: r.u32()?,
                x: r.u32()?,
                y: r.u32()?,
                width: r.u32()?,
                height: r.u32()?,
            },
            tag::RESET => Self::Reset,
            tag::CREATE_FENCE => Self::CreateFence {
                ctx_id: r.u32()?,
                fence_id: r.u32()?,
            },
            tag::POLL_FENCES => Self::PollFences,
            tag::CREATE_BLOB => Self::CreateBlob {
                ctx_id: r.u32()?,
                args: r.create_blob()?,
            },
            tag::DESTROY_BLOB => Self::DestroyBlob {
                resource_id: r.u32()?,
            },
            tag::MAP_BLOB => Self::MapBlob {
                resource_id: r.u32()?,
                offset: r.u64()?,
                size: r.u64()?,
            },
            tag::UNMAP_BLOB => Self::UnmapBlob {
                resource_id: r.u32()?,
                offset: r.u64()?,
            },
            tag::BLOB_SUPPORT => Self::BlobSupport,
            other => return Err(CodecError::UnknownTag(other)),
        };
        if r.at != payload.len() {
            return Err(CodecError::Truncated);
        }
        Ok(request)
    }
}

impl Reply {
    pub fn tag(&self) -> u8 {
        match self {
            Self::Ok => tag::OK,
            Self::Bytes(_) => tag::BYTES,
            Self::Capsets(_) => tag::CAPSETS,
            Self::Fence { .. } => tag::FENCE,
            Self::Fences(_) => tag::FENCES,
            Self::Mapping { .. } => tag::MAPPING,
            Self::BlobSupport(_) => tag::BLOB_SUPPORT_REPLY,
            Self::Error(_) => tag::ERROR,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        match self {
            Self::Ok => (),
            Self::Bytes(bytes) => {
                w.bytes(bytes);
            }
            Self::Capsets(capsets) => {
                w.u32(u32::try_from(capsets.len()).unwrap_or(u32::MAX));
                for capset in capsets {
                    w.u32(capset.id)
                        .u32(capset.max_version)
                        .u32(capset.max_size);
                }
            }
            Self::Fence { pending } => {
                w.u8(u8::from(*pending));
            }
            Self::Fences(ids) => {
                w.u32(u32::try_from(ids.len()).unwrap_or(u32::MAX));
                for id in ids {
                    w.u32(*id);
                }
            }
            Self::Mapping { map_info } => {
                w.u32(*map_info);
            }
            Self::BlobSupport(support) => {
                w.u8(u8::from(support.guest))
                    .u8(u8::from(support.host3d))
                    .u64(support.host_visible_bytes.unwrap_or(0));
            }
            Self::Error(message) => {
                w.bytes(message.as_bytes());
            }
        }
        w.0
    }

    pub fn decode(tag: u8, payload: &[u8]) -> Result<Self, CodecError> {
        let mut r = Reader::new(payload);
        let reply = match tag {
            tag::OK => Self::Ok,
            tag::BYTES => Self::Bytes(r.bytes()?),
            tag::CAPSETS => {
                let count = r.u32()? as usize;
                // Three u32 per capset: refuse a count the payload cannot
                // possibly hold *before* reserving anything for it.
                if count.saturating_mul(12) > payload.len() {
                    return Err(CodecError::Truncated);
                }
                let mut capsets = Vec::with_capacity(count);
                for _ in 0..count {
                    capsets.push(CapsetInfo {
                        id: r.u32()?,
                        max_version: r.u32()?,
                        max_size: r.u32()?,
                    });
                }
                Self::Capsets(capsets)
            }
            tag::MAPPING => Self::Mapping { map_info: r.u32()? },
            tag::BLOB_SUPPORT_REPLY => {
                let flag = |v: u8| match v {
                    0 => Ok(false),
                    1 => Ok(true),
                    _ => Err(CodecError::NonCanonical),
                };
                let guest = flag(r.u8()?)?;
                let host3d = flag(r.u8()?)?;
                let bytes = r.u64()?;
                Self::BlobSupport(BlobSupport {
                    guest,
                    host3d,
                    // Zero means "no window", which is how `None` is encoded;
                    // there is no such thing as a zero-length window, so the
                    // mapping is canonical in both directions.
                    host_visible_bytes: (bytes != 0).then_some(bytes),
                    // Not on the wire, and deliberately: both of these hand
                    // the renderer something an *isolated* one must never
                    // have — a host pointer into the guest's window, and the
                    // guest's own pages. A helper that claimed them would be
                    // claiming its way out of the containment GPU-012 exists
                    // for, so the client decides, not the helper.
                    host_mapped: false,
                })
            }
            tag::FENCE => Self::Fence {
                pending: match r.u8()? {
                    0 => false,
                    1 => true,
                    _ => return Err(CodecError::NonCanonical),
                },
            },
            tag::FENCES => {
                let count = r.u32()? as usize;
                if count.saturating_mul(4) > payload.len() {
                    return Err(CodecError::Truncated);
                }
                let mut ids = Vec::with_capacity(count);
                for _ in 0..count {
                    ids.push(r.u32()?);
                }
                Self::Fences(ids)
            }
            tag::ERROR => {
                Self::Error(String::from_utf8(r.bytes()?).map_err(|_| CodecError::BadString)?)
            }
            other => return Err(CodecError::UnknownTag(other)),
        };
        if r.at != payload.len() {
            return Err(CodecError::Truncated);
        }
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip_request(request: Request) {
        let payload = request.encode();
        let decoded = Request::decode(request.tag(), &payload).expect("decodes");
        assert_eq!(decoded, request);
    }

    fn round_trip_reply(reply: Reply) {
        let payload = reply.encode();
        let decoded = Reply::decode(reply.tag(), &payload).expect("decodes");
        assert_eq!(decoded, reply);
    }

    fn sample_create() -> ResourceCreate3d {
        ResourceCreate3d {
            resource_id: 7,
            target: 2,
            format: 2,
            bind: 1 << 18,
            width: 1920,
            height: 1080,
            depth: 1,
            array_size: 1,
            last_level: 0,
            nr_samples: 0,
            flags: 0,
        }
    }

    fn sample_xfer() -> Transfer3d {
        Transfer3d {
            region: Box3d {
                x: 1,
                y: 2,
                z: 3,
                w: 4,
                h: 5,
                d: 6,
            },
            offset: 0xdead_beef,
            resource_id: 7,
            level: 1,
            stride: 8192,
            layer_stride: 0,
        }
    }

    #[test]
    fn every_request_round_trips() {
        for request in [
            Request::Hello { version: VERSION },
            Request::Capset { id: 2, version: 2 },
            Request::CtxCreate {
                ctx_id: 3,
                capset_id: 0,
                name: "glxgears".into(),
            },
            Request::CtxCreate {
                ctx_id: 4,
                capset_id: crate::CAPSET_VENUS,
                name: "vkcube".into(),
            },
            Request::CtxDestroy { ctx_id: 3 },
            Request::ResourceCreate(sample_create()),
            Request::ResourceUnref { resource_id: 7 },
            Request::CtxAttach {
                ctx_id: 3,
                resource_id: 7,
            },
            Request::CtxDetach {
                ctx_id: 3,
                resource_id: 7,
            },
            Request::AttachBacking {
                resource_id: 7,
                len: 1 << 20,
            },
            Request::DetachBacking { resource_id: 7 },
            Request::TransferToHost {
                ctx_id: 3,
                xfer: sample_xfer(),
                shadow_offset: 4096,
                bytes: vec![0xab; 1000],
            },
            Request::TransferFromHost {
                ctx_id: 3,
                xfer: sample_xfer(),
                shadow_offset: 4096,
                len: 1000,
            },
            Request::Submit {
                ctx_id: 3,
                stream: vec![1, 2, 3, 4],
            },
            Request::ReadRect {
                resource_id: 7,
                x: 5,
                y: 6,
                width: 640,
                height: 480,
            },
            Request::Reset,
            Request::CreateFence {
                ctx_id: 3,
                fence_id: 0x4242,
            },
            Request::PollFences,
            // Blob resources (VEN-2001).
            Request::CreateBlob {
                ctx_id: 7,
                args: ResourceCreateBlob {
                    resource_id: 9,
                    blob_mem: crate::protocol::BLOB_MEM_HOST3D,
                    blob_flags: crate::protocol::BLOB_FLAG_USE_MAPPABLE,
                    nr_entries: 0,
                    blob_id: 0xdead_beef_cafe,
                    size: 1 << 20,
                },
            },
            Request::DestroyBlob { resource_id: 9 },
            Request::MapBlob {
                resource_id: 9,
                offset: 0x1_0000,
                size: 1 << 20,
            },
            Request::UnmapBlob {
                resource_id: 9,
                offset: 0x1_0000,
            },
            Request::BlobSupport,
        ] {
            round_trip_request(request);
        }
    }

    #[test]
    fn every_reply_round_trips() {
        for reply in [
            Reply::Ok,
            Reply::Bytes(vec![9; 308]),
            Reply::Bytes(Vec::new()),
            Reply::Capsets(vec![
                CapsetInfo {
                    id: 1,
                    max_version: 1,
                    max_size: 308,
                },
                CapsetInfo {
                    id: 2,
                    max_version: 2,
                    max_size: 696,
                },
            ]),
            Reply::Capsets(Vec::new()),
            Reply::Fence { pending: true },
            Reply::Fence { pending: false },
            Reply::Fences(vec![1, 2, 3]),
            Reply::Fences(Vec::new()),
            Reply::Error("no such context".into()),
            Reply::Mapping {
                map_info: crate::protocol::MAP_CACHE_WC,
            },
            Reply::BlobSupport(BlobSupport::NONE),
            Reply::BlobSupport(BlobSupport {
                guest: true,
                host3d: true,
                host_visible_bytes: Some(8 << 30),
                host_mapped: false,
            }),
        ] {
            round_trip_reply(reply);
        }
    }

    /// The helper is untrusted input: nothing it can send may panic the VMM,
    /// and nothing may make it allocate on a promise the payload cannot keep.
    #[test]
    fn malformed_frames_are_refused_not_panicked_on() {
        // Unknown tags.
        assert_eq!(
            Request::decode(0x7f, &[]),
            Err(CodecError::UnknownTag(0x7f))
        );
        assert_eq!(Reply::decode(0x00, &[]), Err(CodecError::UnknownTag(0x00)));

        // Truncated fixed fields.
        assert_eq!(
            Request::decode(tag::CREATE_FENCE, &[1, 2, 3]),
            Err(CodecError::Truncated)
        );
        assert_eq!(Reply::decode(tag::FENCE, &[]), Err(CodecError::Truncated));

        // A length field promising more than the payload holds.
        let mut evil = 0u32.to_le_bytes().to_vec(); // ctx_id
        evil.extend_from_slice(&u32::MAX.to_le_bytes()); // stream length
        assert!(matches!(
            Request::decode(tag::SUBMIT, &evil),
            Err(CodecError::TooLarge(_) | CodecError::Truncated)
        ));

        // A capset count that would reserve gigabytes.
        let evil = u32::MAX.to_le_bytes();
        assert_eq!(
            Reply::decode(tag::CAPSETS, &evil),
            Err(CodecError::Truncated)
        );
        let evil = u32::MAX.to_le_bytes();
        assert_eq!(
            Reply::decode(tag::FENCES, &evil),
            Err(CodecError::Truncated)
        );

        // Trailing bytes: a frame must consume exactly.
        let mut extra = Request::PollFences.encode();
        extra.push(0);
        assert_eq!(
            Request::decode(tag::POLL_FENCES, &extra),
            Err(CodecError::Truncated)
        );

        // A boolean byte that is neither 0 nor 1: the encoding is canonical,
        // so this is refused rather than read as "true" (found by the
        // gpu_remote_protocol fuzz target).
        assert_eq!(
            Reply::decode(tag::FENCE, &[2]),
            Err(CodecError::NonCanonical)
        );
        assert_eq!(
            Reply::decode(tag::FENCE, &[0xff]),
            Err(CodecError::NonCanonical)
        );
        assert_eq!(
            Reply::decode(tag::FENCE, &[1]),
            Ok(Reply::Fence { pending: true })
        );
        assert_eq!(
            Reply::decode(tag::FENCE, &[0]),
            Ok(Reply::Fence { pending: false })
        );

        // Non-UTF-8 context name (ctx_id, capset_id, length, bytes).
        let mut bad = 1u32.to_le_bytes().to_vec();
        bad.extend_from_slice(&0u32.to_le_bytes());
        bad.extend_from_slice(&2u32.to_le_bytes());
        bad.extend_from_slice(&[0xff, 0xfe]);
        assert_eq!(
            Request::decode(tag::CTX_CREATE, &bad),
            Err(CodecError::BadString)
        );
    }

    /// Arbitrary bytes under every tag: no panic, no hang, ever.
    #[test]
    fn arbitrary_payloads_under_every_tag_are_survivable() {
        let payloads: [&[u8]; 6] = [
            &[],
            &[0xff],
            &[0; 7],
            &[0xff; 64],
            &0xffff_ffffu32.to_le_bytes(),
            &[0x80; 33],
        ];
        for tag in 0u8..=0x90 {
            for payload in payloads {
                let _ = Request::decode(tag, payload);
                let _ = Reply::decode(tag, payload);
            }
        }
    }

    /// The bounds are the ones the device already enforces, so a legitimate
    /// command can never be too large for the socket.
    /// The bounds are the ones the device already enforces, so a legitimate
    /// command can never be too large for the socket. Compile-time, because a
    /// violation here would be a build-breaking design error rather than a
    /// test failure.
    const _: () = {
        assert!(crate::renderer::MAX_SUBMIT_BYTES < MAX_FRAME_BYTES);
        // A full-size scanout rect: MAX_RESOURCE_PIXELS BGRA pixels.
        assert!(crate::MAX_RESOURCE_PIXELS * 4 < MAX_FRAME_BYTES as u64);
        assert!(REMOTE_XFER_WINDOW < MAX_FRAME_BYTES);
        // A whole backing is deliberately *not* covered: it never travels in
        // one frame, only a REMOTE_XFER_WINDOW span of it does.
    };
}
