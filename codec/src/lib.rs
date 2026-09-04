//! encode and decode the frames for the mux protocol.
//! The frames include the length of a PDU as well as an identifier
//! that informs us how to decode it.  The length, ident and serial
//! number are encoded using a variable length integer encoding.
//! Rather than rely solely on serde to serialize and deserialize an
//! enum, we encode the enum variants with a version/identifier tag
//! for ourselves.  This will make it a little easier to manage
//! client and server instances that are built from different versions
//! of this code; in this way the client and server can more gracefully
//! manage unknown enum variants.
#![allow(dead_code)]
#![allow(clippy::range_plus_one)]

use anyhow::{bail, Context as _, Error};
use rangeset::*;
use serde::{Deserialize, Serialize};
use thinkterm_proto::{
    ClientId, ClientInfo, CommandSpec, PaneDirection, PaneId, PaneNode, RenderableDimensions,
    ScrollbackEraseMode, SerdeUrl, SplitRequest, StableCursorPosition, TabId, WindowId,
};
// smol's io/prelude are pure re-exports of these futures-lite modules, so
// this is the same set of traits -- minus smol's runtime (async-io, polling),
// which does not build for wasm and which nothing in this crate uses.
use futures_lite::io::AsyncWriteExt;
use futures_lite::prelude::*;
use std::collections::HashMap;
use std::convert::TryInto;
use std::io::Cursor;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use termwiz::hyperlink::Hyperlink;
use termwiz::image::{ImageData, TextureCoordinate};
use termwiz::input::KeyboardEncoding;
use wezterm_input_types::KittyKeyboardFlags;
use termwiz::surface::{Line, SequenceNo};
use thiserror::Error;
use wezterm_term::color::ColorPalette;
use wezterm_term::{Alert, ClipboardSelection, StableRowIndex, TerminalSize};

pub mod thinkterm_tree;
pub use thinkterm_tree::{
    apply_op, ensure_unique_thread_names, ThinkTermTree, TreeOp, TtProject, TtProjectId, TtSpace,
    TtSpaceId, TtThread, TtThreadId,
};

#[derive(Error, Debug)]
#[error("Corrupt Response: {0}")]
pub struct CorruptResponse(String);

/// Returns the encoded length of the leb128 representation of value
fn encoded_length(value: u64) -> usize {
    struct NullWrite {}
    impl std::io::Write for NullWrite {
        fn write(&mut self, buf: &[u8]) -> std::result::Result<usize, std::io::Error> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::result::Result<(), std::io::Error> {
            Ok(())
        }
    }

    leb128::write::unsigned(&mut NullWrite {}, value).unwrap()
}

const COMPRESSED_MASK: u64 = 1 << 63;

fn encode_raw_as_vec(
    ident: u64,
    serial: u64,
    data: &[u8],
    is_compressed: bool,
) -> anyhow::Result<Vec<u8>> {
    let len = data.len() + encoded_length(ident) + encoded_length(serial);
    let masked_len = if is_compressed {
        (len as u64) | COMPRESSED_MASK
    } else {
        len as u64
    };

    // Double-buffer the data; since we run with nodelay enabled, it is
    // desirable for the write to be a single packet (or at least, for
    // the header portion to go out in a single packet)
    let mut buffer = Vec::with_capacity(len + encoded_length(masked_len));

    leb128::write::unsigned(&mut buffer, masked_len).context("writing pdu len")?;
    leb128::write::unsigned(&mut buffer, serial).context("writing pdu serial")?;
    leb128::write::unsigned(&mut buffer, ident).context("writing pdu ident")?;
    buffer.extend_from_slice(data);

    if is_compressed {
        metrics::histogram!("pdu.encode.compressed.size").record(buffer.len() as f64);
    } else {
        metrics::histogram!("pdu.encode.size").record(buffer.len() as f64);
    }

    Ok(buffer)
}

/// Encode a frame.  If the data is compressed, the high bit of the length
/// is set to indicate that.  The data written out has the format:
/// tagged_len: leb128  (u64 msb is set if data is compressed)
/// serial: leb128
/// ident: leb128
/// data bytes
fn encode_raw<W: std::io::Write>(
    ident: u64,
    serial: u64,
    data: &[u8],
    is_compressed: bool,
    mut w: W,
) -> anyhow::Result<usize> {
    let buffer = encode_raw_as_vec(ident, serial, data, is_compressed)?;
    w.write_all(&buffer).context("writing pdu data buffer")?;
    Ok(buffer.len())
}

async fn encode_raw_async<W: Unpin + AsyncWriteExt>(
    ident: u64,
    serial: u64,
    data: &[u8],
    is_compressed: bool,
    w: &mut W,
) -> anyhow::Result<usize> {
    let buffer = encode_raw_as_vec(ident, serial, data, is_compressed)?;
    w.write_all(&buffer)
        .await
        .context("writing pdu data buffer")?;
    Ok(buffer.len())
}

/// Read a single leb128 encoded value from the stream
async fn read_u64_async<R>(r: &mut R) -> anyhow::Result<u64>
where
    R: Unpin + AsyncRead + std::fmt::Debug,
{
    let mut buf = vec![];
    loop {
        let mut byte = [0u8];
        let nread = r.read(&mut byte).await?;
        if nread == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "EOF while reading leb128 encoded value",
            )
            .into());
        }
        buf.push(byte[0]);

        match leb128::read::unsigned(&mut buf.as_slice()) {
            Ok(n) => {
                return Ok(n);
            }
            Err(leb128::read::Error::IoError(_)) => continue,
            Err(leb128::read::Error::Overflow) => anyhow::bail!("leb128 is too large"),
        }
    }
}

/// Read a single leb128 encoded value from the stream
fn read_u64<R: std::io::Read>(mut r: R) -> anyhow::Result<u64> {
    leb128::read::unsigned(&mut r)
        .map_err(|err| match err {
            leb128::read::Error::IoError(ioerr) => anyhow::Error::new(ioerr),
            err => anyhow::Error::new(err),
        })
        .context("reading leb128")
}

#[derive(Debug)]
struct Decoded {
    ident: u64,
    serial: u64,
    data: Vec<u8>,
    is_compressed: bool,
}

/// Decode a frame.
/// See encode_raw() for the frame format.
async fn decode_raw_async<R: Unpin + AsyncRead + std::fmt::Debug>(
    r: &mut R,
    max_serial: Option<u64>,
) -> anyhow::Result<Decoded> {
    let len = read_u64_async(r)
        .await
        .context("decode_raw_async failed to read PDU length")?;
    let (len, is_compressed) = if (len & COMPRESSED_MASK) != 0 {
        (len & !COMPRESSED_MASK, true)
    } else {
        (len, false)
    };
    let serial = read_u64_async(r)
        .await
        .context("decode_raw_async failed to read PDU serial")?;
    if let Some(max_serial) = max_serial {
        if serial > max_serial && max_serial > 0 {
            return Err(CorruptResponse(format!(
                "decode_raw_async: serial {serial} is implausibly large \
                (bigger than {max_serial})"
            ))
            .into());
        }
    }
    let ident = read_u64_async(r)
        .await
        .context("decode_raw_async failed to read PDU ident")?;
    let data_len =
        match (len as usize).overflowing_sub(encoded_length(ident) + encoded_length(serial)) {
            (_, true) => {
                return Err(CorruptResponse(format!(
                    "decode_raw_async: sizes don't make sense: \
                    len:{len} serial:{serial} (enc={}) ident:{ident} (enc={})",
                    encoded_length(serial),
                    encoded_length(ident)
                ))
                .into());
            }
            (data_len, false) => data_len,
        };

    if is_compressed {
        metrics::histogram!("pdu.decode.compressed.size").record(data_len as f64);
    } else {
        metrics::histogram!("pdu.decode.size").record(data_len as f64);
    }

    let mut data = vec![0u8; data_len];
    r.read_exact(&mut data).await.with_context(|| {
        format!(
            "decode_raw_async failed to read {} bytes of data \
            for PDU of length {} with serial={} ident={}",
            data_len, len, serial, ident
        )
    })?;
    Ok(Decoded {
        ident,
        serial,
        data,
        is_compressed,
    })
}

/// Decode a frame.
/// See encode_raw() for the frame format.
fn decode_raw<R: std::io::Read>(mut r: R) -> anyhow::Result<Decoded> {
    let len = read_u64(r.by_ref()).context("reading PDU length")?;
    let (len, is_compressed) = if (len & COMPRESSED_MASK) != 0 {
        (len & !COMPRESSED_MASK, true)
    } else {
        (len, false)
    };
    let serial = read_u64(r.by_ref()).context("reading PDU serial")?;
    let ident = read_u64(r.by_ref()).context("reading PDU ident")?;
    let data_len =
        match (len as usize).overflowing_sub(encoded_length(ident) + encoded_length(serial)) {
            (_, true) => {
                anyhow::bail!(
                    "sizes don't make sense: len:{} serial:{} (enc={}) ident:{} (enc={})",
                    len,
                    serial,
                    encoded_length(serial),
                    ident,
                    encoded_length(ident)
                );
            }
            (data_len, false) => data_len,
        };

    if is_compressed {
        metrics::histogram!("pdu.decode.compressed.size").record(data_len as f64);
    } else {
        metrics::histogram!("pdu.decode.size").record(data_len as f64);
    }

    let mut data = vec![0u8; data_len];
    r.read_exact(&mut data).with_context(|| {
        format!(
            "reading {} bytes of data for PDU of length {} with serial={} ident={}",
            data_len, len, serial, ident
        )
    })?;
    Ok(Decoded {
        ident,
        serial,
        data,
        is_compressed,
    })
}

#[derive(Debug, PartialEq)]
pub struct DecodedPdu {
    pub serial: u64,
    pub pdu: Pdu,
}

/// If the serialized size is larger than this, then we'll consider compressing it
#[cfg(not(target_family = "wasm"))]
const COMPRESS_THRESH: usize = 32;

fn serialize<T: serde::Serialize>(t: &T) -> Result<(Vec<u8>, bool), Error> {
    let mut uncompressed = Vec::new();
    let mut encode = varbincode::Serializer::new(&mut uncompressed);
    t.serialize(&mut encode)?;

    // No zstd on wasm, so a wasm sender never compresses. The receiving side
    // handles both forms regardless, and the outbound traffic of a thin
    // client is mostly keystrokes, so there is nothing worth compressing.
    #[cfg(target_family = "wasm")]
    {
        Ok((uncompressed, false))
    }

    #[cfg(not(target_family = "wasm"))]
    {
        if uncompressed.len() <= COMPRESS_THRESH {
            return Ok((uncompressed, false));
        }
        // It's a little heavy; let's try compressing it
        let mut compressed = Vec::new();
        let mut compress = zstd::Encoder::new(&mut compressed, zstd::DEFAULT_COMPRESSION_LEVEL)?;
        let mut encode = varbincode::Serializer::new(&mut compress);
        t.serialize(&mut encode)?;
        drop(encode);
        compress.finish()?;

        log::debug!(
            "serialized+compress len {} vs {}",
            compressed.len(),
            uncompressed.len()
        );

        if compressed.len() < uncompressed.len() {
            Ok((compressed, true))
        } else {
            Ok((uncompressed, false))
        }
    }
}

fn deserialize<T: serde::de::DeserializeOwned, R: std::io::Read>(
    mut r: R,
    is_compressed: bool,
) -> Result<T, Error> {
    if is_compressed {
        #[cfg(not(target_family = "wasm"))]
        let mut decompress = zstd::Decoder::new(r)?;
        // The pure-Rust decoder stands in where the zstd C library cannot
        // build; it also implements std::io::Read, so the shape is identical.
        #[cfg(target_family = "wasm")]
        let mut decompress = ruzstd::decoding::StreamingDecoder::new(r)
            .map_err(|e| anyhow::anyhow!("ruzstd: {e}"))?;
        let mut decode = varbincode::Deserializer::new(&mut decompress);
        serde::Deserialize::deserialize(&mut decode).map_err(Into::into)
    } else {
        let mut decode = varbincode::Deserializer::new(&mut r);
        serde::Deserialize::deserialize(&mut decode).map_err(Into::into)
    }
}

macro_rules! pdu {
    ($( $name:ident:$vers:expr),* $(,)?) => {
        #[derive(PartialEq, Debug)]
        pub enum Pdu {
            Invalid{ident: u64},
            $(
                $name($name)
            ,)*
        }

        impl Pdu {
            pub fn encode<W: std::io::Write>(&self, w: W, serial: u64) -> Result<(), Error> {
                match self {
                    Pdu::Invalid{..} => bail!("attempted to serialize Pdu::Invalid"),
                    $(
                        Pdu::$name(s) => {
                            let (data, is_compressed) = serialize(s)?;
                            let encoded_size = encode_raw($vers, serial, &data, is_compressed, w)?;
                            log::debug!("encode {} size={encoded_size}", stringify!($name));
                            metrics::histogram!("pdu.size", "pdu" => stringify!($name)).record(encoded_size as f64);
                            metrics::histogram!("pdu.size.rate", "pdu" => stringify!($name)).record(encoded_size as f64);
                            Ok(())
                        }
                    ,)*
                }
            }

            pub async fn encode_async<W: Unpin + AsyncWriteExt>(&self, w: &mut W, serial: u64) -> Result<(), Error> {
                match self {
                    Pdu::Invalid{..} => bail!("attempted to serialize Pdu::Invalid"),
                    $(
                        Pdu::$name(s) => {
                            let (data, is_compressed) = serialize(s)?;
                            let encoded_size = encode_raw_async($vers, serial, &data, is_compressed, w).await?;
                            log::debug!("encode_async {} size={encoded_size}", stringify!($name));
                            metrics::histogram!("pdu.size", "pdu" => stringify!($name)).record(encoded_size as f64);
                            metrics::histogram!("pdu.size.rate", "pdu" => stringify!($name)).record(encoded_size as f64);
                            Ok(())
                        }
                    ,)*
                }
            }

            pub fn pdu_name(&self) -> &'static str {
                match self {
                    Pdu::Invalid{..} => "Invalid",
                    $(
                        Pdu::$name(_) => {
                            stringify!($name)
                        }
                    ,)*
                }
            }

            pub fn decode<R: std::io::Read>(r: R) -> Result<DecodedPdu, Error> {
                let decoded = decode_raw(r).context("decoding a PDU")?;
                match decoded.ident {
                    $(
                        $vers => {
                            metrics::histogram!("pdu.size", "pdu" => stringify!($name)).record(decoded.data.len() as f64);
                            metrics::histogram!("pdu.size.rate", "pdu" => stringify!($name)).record(decoded.data.len() as f64);
                            Ok(DecodedPdu {
                                serial: decoded.serial,
                                pdu: Pdu::$name(deserialize(decoded.data.as_slice(), decoded.is_compressed)?)
                            })
                        }
                    ,)*
                    _ => {
                        metrics::histogram!("pdu.size", "pdu" => "??").record(decoded.data.len() as f64);
                        metrics::histogram!("pdu.size.rate", "pdu" => "??").record(decoded.data.len() as f64);
                        Ok(DecodedPdu {
                            serial: decoded.serial,
                            pdu: Pdu::Invalid{ident:decoded.ident}
                        })
                    }
                }
            }

            pub async fn decode_async<R>(r: &mut R, max_serial: Option<u64>) -> Result<DecodedPdu, Error>
                where R: std::marker::Unpin,
                      R: AsyncRead,
                      R: std::fmt::Debug
            {
                let decoded = decode_raw_async(r, max_serial).await.context("decoding a PDU")?;
                match decoded.ident {
                    $(
                        $vers => {
                            metrics::histogram!("pdu.size", "pdu" => stringify!($name)).record(decoded.data.len() as f64);
                            Ok(DecodedPdu {
                                serial: decoded.serial,
                                pdu: Pdu::$name(deserialize(decoded.data.as_slice(), decoded.is_compressed)?)
                            })
                        }
                    ,)*
                    _ => {
                        metrics::histogram!("pdu.size", "pdu" => "??").record(decoded.data.len() as f64);
                        Ok(DecodedPdu {
                            serial: decoded.serial,
                            pdu: Pdu::Invalid{ident:decoded.ident}
                        })
                    }
                }
            }
        }
    }
}

/// The overall version of the codec.
/// This must be bumped when backwards incompatible changes
/// are made to the types and protocol.
/// 47: PaneStackEntry gained pane_stack_id; stack operation PDUs.
/// 48: MovePaneToStack moves an existing pane into another pane stack.
/// 49: Palette advisories are client-only, application palette state is
///     explicit, and SetFocusedPane carries the focusing client's palette.
/// 50: The server owns the ThinkTerm sidebar tree (Space/Project/Thread).
/// 51: Server-side tree revisions and authoritative compound mutations.
/// 52: ThinkTerm session snapshots, runtime server identity and per-tab
///     frontend viewport ownership.
/// 53: Explicit, server-authoritative frontend viewport claims.
/// 54: Server-authoritative ThinkTerm Thread landing and materialization.
/// 55: Panes report alternate screen state so renderers can route the wheel.
/// 56: Pane entries carry that state too, so a renderer knows it on arrival.
/// 57: Frontends publish and follow a shared per-tab scroll position.
/// 58: Connection-wide A/B access modes and atomic geometry-bearing claims.
/// 60: Spawn PDUs carry a portable CommandSpec instead of CommandBuilder;
///     argv, env and cwd travel as byte strings rather than OsString, and
///     the umask field exists on every platform instead of only unix.
/// 61: Agent status is classified by the mux that owns the pane and pushed
///     to clients (AgentStatusChanged), with a request/response pair for
///     cold-start delivery and for `thinkterm cli agent list`.
/// 62: Projects carry an archived state in the shared ThinkTerm tree
///     (TtProject.archived_at, TreeOp::SetProjectArchived).
/// 64: Image cells carry the image's generation, so a client notices when
///     an animation it already fetched has grown; GetImageCell can ask for
///     only the frames it lacks and is told the generation it received.
///     The server probes silent clients with Ping and a client answers
///     Pong; a remote pane reports its keyboard encoding.
pub const CODEC_VERSION: usize = 64;

// Defines the Pdu enum.
// Each struct has an explicit identifying number.
// This allows removal of obsolete structs,
// and defining newer structs as the protocol evolves.
pdu! {
    ErrorResponse: 0,
    Ping: 1,
    Pong: 2,
    ListPanes: 3,
    ListPanesResponse: 4,
    SpawnResponse: 8,
    WriteToPane: 9,
    UnitResponse: 10,
    SendKeyDown: 11,
    SendMouseEvent: 12,
    SendPaste: 13,
    Resize: 14,
    SetClipboard: 20,
    GetLines: 22,
    GetLinesResponse: 23,
    GetPaneRenderChanges: 24,
    GetPaneRenderChangesResponse: 25,
    GetCodecVersion: 26,
    GetCodecVersionResponse: 27,
    GetTlsCreds: 28,
    GetTlsCredsResponse: 29,
    LivenessResponse: 30,
    SearchScrollbackRequest: 31,
    SearchScrollbackResponse: 32,
    SetPaneZoomed: 33,
    SplitPane: 34,
    KillPane: 35,
    SpawnV2: 36,
    PaneRemoved: 37,
    SetPalette: 38,
    NotifyAlert: 39,
    SetClientId: 40,
    GetClientList: 41,
    GetClientListResponse: 42,
    SetWindowWorkspace: 43,
    WindowWorkspaceChanged: 44,
    SetFocusedPane: 45,
    GetImageCell: 46,
    GetImageCellResponse: 47,
    MovePaneToNewTab: 48,
    MovePaneToNewTabResponse: 49,
    ActivatePaneDirection: 50,
    GetPaneRenderableDimensions: 51,
    GetPaneRenderableDimensionsResponse: 52,
    PaneFocused: 53,
    TabResized: 54,
    TabAddedToWindow: 55,
    TabTitleChanged: 56,
    WindowTitleChanged: 57,
    RenameWorkspace: 58,
    EraseScrollbackRequest: 59,
    GetPaneDirection: 60,
    GetPaneDirectionResponse: 61,
    AdjustPaneSize: 62,
    SpawnPaneInStack: 63,
    ActivatePaneInStack: 64,
    MovePaneToStack: 65,
    SetApplicationPalette: 66,
    GetThinkTermTree: 67,
    MutateThinkTermTree: 68,
    ThinkTermTreeState: 69,
    GetThinkTermSessionState: 70,
    ThinkTermSessionState: 71,
    SetClientViewport: 72,
    ClientViewportState: 73,
    ClaimClientViewport: 74,
    EnsureThinkTermThread: 75,
    EnsureThinkTermThreadResponse: 76,
    SetClientView: 77,
    SetFrontendAccessMode: 78,
    FrontendAccessState: 79,
    AgentStatusChanged: 80,
    GetAgentStatuses: 81,
    GetAgentStatusesResponse: 82,
    GetServerOsRelease: 83,
    GetServerOsReleaseResponse: 84,
}

impl Pdu {
    /// Returns true if this type of Pdu represents action taken
    /// directly by a user, rather than background traffic on
    /// a live connection
    /// A person doing something *to a terminal*, as opposed to a renderer
    /// managing its own geometry.
    ///
    /// This is what decides who owns a tab's viewport, so the line matters: a
    /// renderer tells the server its pane sizes on the first frame after it
    /// attaches, and counting that as interaction meant merely opening a second
    /// client took the grid away from the one being used. Resizing, zooming,
    /// spawning and stack bookkeeping are all things a frontend does to itself.
    pub fn is_terminal_interaction(&self) -> bool {
        matches!(
            self,
            Self::WriteToPane(_)
                | Self::SendKeyDown(_)
                | Self::SendMouseEvent(_)
                | Self::SendPaste(_)
        )
    }

    pub fn is_user_input(&self) -> bool {
        match self {
            Self::WriteToPane(_)
            | Self::SendKeyDown(_)
            | Self::SendMouseEvent(_)
            | Self::SendPaste(_)
            | Self::Resize(_)
            | Self::SetClipboard(_)
            | Self::SetPaneZoomed(_)
            | Self::ClaimClientViewport(_)
            | Self::SetClientView(_)
            | Self::EnsureThinkTermThread(_)
            | Self::SpawnV2(_)
            | Self::SpawnPaneInStack(_)
            | Self::ActivatePaneInStack(_)
            | Self::MovePaneToStack(_) => true,
            _ => false,
        }
    }

    pub fn stream_decode(buffer: &mut Vec<u8>) -> anyhow::Result<Option<DecodedPdu>> {
        let mut cursor = Cursor::new(buffer.as_slice());
        match Self::decode(&mut cursor) {
            Ok(decoded) => {
                let consumed = cursor.position() as usize;
                let remain = buffer.len() - consumed;
                // Remove `consumed` bytes from the start of the vec.
                // This is safe because the vec is just bytes and we are
                // constrained the offsets accordingly.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        buffer.as_ptr().add(consumed),
                        buffer.as_mut_ptr(),
                        remain,
                    );
                }
                buffer.truncate(remain);
                Ok(Some(decoded))
            }
            Err(err) => {
                if let Some(ioerr) = err.root_cause().downcast_ref::<std::io::Error>() {
                    match ioerr.kind() {
                        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::WouldBlock => {
                            return Ok(None);
                        }
                        _ => {}
                    }
                } else {
                    log::error!("not an ioerror in stream_decode: {:?}", err);
                }
                Err(err)
            }
        }
    }

    pub fn try_read_and_decode<R: std::io::Read>(
        r: &mut R,
        buffer: &mut Vec<u8>,
    ) -> anyhow::Result<Option<DecodedPdu>> {
        loop {
            if let Some(decoded) =
                Self::stream_decode(buffer).context("stream_decode of buffer for PDU")?
            {
                return Ok(Some(decoded));
            }

            let mut buf = [0u8; 4096];
            let size = match r.read(&mut buf) {
                Ok(size) => size,
                Err(err) => {
                    if err.kind() == std::io::ErrorKind::WouldBlock {
                        return Ok(None);
                    }
                    return Err(err.into());
                }
            };
            if size == 0 {
                return Err(
                    std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "End Of File").into(),
                );
            }

            buffer.extend_from_slice(&buf[0..size]);
        }
    }

    pub fn pane_id(&self) -> Option<PaneId> {
        match self {
            Pdu::GetPaneRenderChangesResponse(GetPaneRenderChangesResponse { pane_id, .. })
            | Pdu::SetPalette(SetPalette { pane_id, .. })
            | Pdu::SetApplicationPalette(SetApplicationPalette { pane_id, .. })
            | Pdu::NotifyAlert(NotifyAlert { pane_id, .. })
            | Pdu::SetClipboard(SetClipboard { pane_id, .. })
            | Pdu::PaneFocused(PaneFocused { pane_id })
            | Pdu::AgentStatusChanged(AgentStatusChanged { pane_id, .. })
            | Pdu::PaneRemoved(PaneRemoved { pane_id }) => Some(*pane_id),
            _ => None,
        }
    }
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct UnitResponse {}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct ErrorResponse {
    pub reason: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetCodecVersion {}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetCodecVersionResponse {
    pub codec_vers: usize,
    pub version_string: String,
    /// Unique to this running mux process. It deliberately does not survive a
    /// restart: the current server cannot preserve live PTYs across one.
    pub server_id: String,
    pub executable_path: PathBuf,
    pub config_file_path: Option<PathBuf>,
}

/// What the server is running on, asked for separately.
///
/// Deliberately its own request rather than another field on
/// `GetCodecVersionResponse`: varbincode is positional, so any field added to
/// that struct makes it undecodable by a peer built against a different
/// version -- and that is the one message whose whole job is to report a
/// version mismatch in the first place. Its shape has to stay frozen or a
/// skewed pair gets "failed to fill whole buffer" instead of being told which
/// versions they are.
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetServerOsRelease {}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetServerOsReleaseResponse {
    /// The `ID=` from the server's own `/etc/os-release`, when it has one.
    pub os_release_id: Option<String>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct Ping {}
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct Pong {}

/// Requests a client certificate to authenticate against
/// the TLS based server
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetTlsCreds {}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetTlsCredsResponse {
    /// The signing certificate
    pub ca_cert_pem: String,
    /// A client authentication certificate and private
    /// key, PEM encoded
    pub client_cert_pem: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct ListPanes {}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct ListPanesResponse {
    pub tabs: Vec<PaneNode>,
    pub tab_titles: Vec<String>,
    pub window_titles: HashMap<WindowId, String>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SplitPane {
    pub pane_id: PaneId,
    pub split_request: SplitRequest,
    pub command: Option<CommandSpec>,
    pub command_dir: Option<String>,
    pub domain: thinkterm_proto::SpawnTabDomain,
    /// Instead of spawning a command, move the specified
    /// pane into the new split target
    pub move_pane_id: Option<PaneId>,
}

/// Spawn a new pane as a level-2 tab in the pane stack that contains
/// `pane_id`. Responds with SpawnResponse describing the new pane.
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SpawnPaneInStack {
    pub pane_id: PaneId,
    pub command: Option<CommandSpec>,
    pub command_dir: Option<String>,
    pub domain: thinkterm_proto::SpawnTabDomain,
}

/// Make `pane_id` the visible pane of the stack that contains it.
/// Responds with UnitResponse.
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct ActivatePaneInStack {
    pub pane_id: PaneId,
}

/// Move an existing pane into the stack containing `target_pane_id`.
/// Both ids are in the mux server's pane-id space. Responds with
/// UnitResponse.
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct MovePaneToStack {
    pub source_pane_id: PaneId,
    pub target_pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct MovePaneToNewTab {
    pub pane_id: PaneId,
    pub window_id: Option<WindowId>,
    pub workspace_for_new_window: Option<String>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct MovePaneToNewTabResponse {
    pub tab_id: TabId,
    pub window_id: WindowId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SpawnV2 {
    pub domain: thinkterm_proto::SpawnTabDomain,
    /// If None, create a new window for this new tab
    pub window_id: Option<WindowId>,
    pub command: Option<CommandSpec>,
    pub command_dir: Option<String>,
    pub size: TerminalSize,
    pub workspace: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct PaneRemoved {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct KillPane {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SpawnResponse {
    pub tab_id: TabId,
    pub pane_id: PaneId,
    pub window_id: WindowId,
    pub size: TerminalSize,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct WriteToPane {
    pub pane_id: PaneId,
    pub data: Vec<u8>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SendPaste {
    pub pane_id: PaneId,
    pub data: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SendKeyDown {
    pub pane_id: TabId,
    pub event: termwiz::input::KeyEvent,
    pub input_serial: InputSerial,
}

/// InputSerial is used to sequence input requests with output events.
/// It started life as a monotonic sequence number but evolved into
/// the number of milliseconds since the unix epoch.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Copy, PartialOrd, Ord)]
pub struct InputSerial(u64);

impl InputSerial {
    pub const fn empty() -> Self {
        Self(0)
    }

    pub fn now() -> Self {
        std::time::SystemTime::now().into()
    }

    pub fn elapsed_millis(&self) -> u64 {
        let now = InputSerial::now();
        now.0 - self.0
    }
}

impl From<std::time::SystemTime> for InputSerial {
    fn from(val: std::time::SystemTime) -> Self {
        let duration = val
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .expect("SystemTime before unix epoch?");
        let millis: u64 = duration
            .as_millis()
            .try_into()
            .expect("millisecond count to fit in u64");
        InputSerial(millis)
    }
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SendMouseEvent {
    pub pane_id: PaneId,
    pub event: wezterm_term::input::MouseEvent,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SetClipboard {
    pub pane_id: PaneId,
    pub clipboard: Option<String>,
    pub selection: ClipboardSelection,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SetWindowWorkspace {
    pub window_id: WindowId,
    pub workspace: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct RenameWorkspace {
    pub old_workspace: String,
    pub new_workspace: String,
}

/// A client-to-server advisory carrying that client's configured palette.
/// The server uses this as the base for OSC color queries while that client
/// owns the pane's palette, but must never broadcast it as application state.
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SetPalette {
    pub pane_id: PaneId,
    pub palette: ColorPalette,
}

/// Server-to-client state of the palette override owned by the application
/// running in the pane. `None` means that the client must render using its own
/// configured palette; `Some` is authoritative until a later reset.
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SetApplicationPalette {
    pub pane_id: PaneId,
    pub palette: Option<ColorPalette>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct NotifyAlert {
    pub pane_id: PaneId,
    pub alert: Alert,
}

/// Fetch the server's ThinkTerm sidebar tree. Responds with
/// `ThinkTermTreeState`.
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetThinkTermTree {}

/// Apply mutations to the server's ThinkTerm sidebar tree, in order. Responds
/// with `ThinkTermTreeState` carrying the tree as it stands afterwards; when
/// anything changed the same state is also pushed unilaterally to every other
/// connected client.
///
/// A batch is what lets a client seed a fresh server — "here is everything I
/// have for you" — in one round trip and one broadcast instead of a storm of
/// single-op RPCs.
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct MutateThinkTermTree {
    pub ops: Vec<TreeOp>,
}

/// The whole tree. It is small enough (tens of KB) that shipping it in full on
/// every change is cheaper than maintaining a delta protocol, and it removes
/// an entire class of resync bugs. Doubles as the server's unilateral push.
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct ThinkTermTreeState {
    pub tree: ThinkTermTree,
}

/// Read-only view built by one authoritative mux server from its tree and its
/// live mux topology. It intentionally excludes credentials and
/// device-private presentation data.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Default)]
pub struct ThinkTermSessionState {
    /// Identity of the running server that produced this snapshot.
    pub server_id: String,
    /// Revision of the authoritative Space/Project/Thread tree.
    pub tree_revision: u64,
    /// Monotonic within one server process. Consumers establish a new
    /// comparison baseline when `server_id` changes.
    pub generation: u64,
    pub spaces: Vec<ThinkTermSessionSpace>,
    pub projects: Vec<ThinkTermSessionProject>,
}

#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Default)]
pub struct ThinkTermSessionSpace {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    /// The mux client domain that owns this Space. `None` means local to the
    /// running ThinkTerm frontend.
    pub domain: Option<String>,
}

#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Default)]
pub struct ThinkTermSessionProject {
    pub id: String,
    pub space_id: String,
    pub name: String,
    /// A path on the machine that owns this project.
    pub path: String,
    pub threads: Vec<ThinkTermSessionThread>,
}

#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Default)]
pub struct ThinkTermSessionThread {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub planned_workspace_name: Option<String>,
    pub materialized_workspace_name: Option<String>,
    pub is_pinned: bool,
    pub is_unread: bool,
    pub work_status: ThinkTermSessionWorkStatus,
    /// Exact live mux objects owned by this thread. IDs are in the producing
    /// server's namespace; a client domain translates them into local mirror
    /// IDs before rendering.
    pub tabs: Vec<ThinkTermSessionTab>,
}

#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Default)]
pub struct ThinkTermSessionTab {
    pub window_id: WindowId,
    pub tab_id: TabId,
    pub pane_ids: Vec<PaneId>,
    pub title: String,
    pub is_active: bool,
}

#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Copy, Default)]
pub enum ThinkTermSessionWorkStatus {
    #[default]
    Idle,
    Running,
    NeedsAttention,
    FinishedUnseen,
}

/// Ask a mux server for its authoritative ThinkTerm session view.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug)]
pub struct GetThinkTermSessionState {}

/// Unilateral: the mux that owns `pane_id` re-classified its agent status.
/// `None` means the pane is no longer running a recognized agent.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct AgentStatusChanged {
    pub pane_id: PaneId,
    pub status: Option<thinkterm_proto::AgentStatus>,
}

/// Ask a mux for the agent status of every pane it knows about — detected
/// locally or mirrored from a chained server. Serves cold-start delivery
/// on attach and `thinkterm cli agent list`.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug)]
pub struct GetAgentStatuses {}

#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct AgentStatusEntry {
    pub pane_id: PaneId,
    pub status: thinkterm_proto::AgentStatus,
    /// Server-side title and workspace, so a one-shot client (the CLI)
    /// needs no second ListPanes round trip. The GUI ignores both: it
    /// already mirrors the pane objects these were read from.
    pub title: String,
    pub workspace: String,
}

#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct GetAgentStatusesResponse {
    pub statuses: Vec<AgentStatusEntry>,
}

/// Select an existing authoritative Thread, or create the canonical
/// Default/Home/main landing when the server has no usable Thread, and make
/// sure that the selected Thread has a live terminal. The server validates the
/// preferred ID against its current tree and owns both fallback selection and
/// materialization.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct EnsureThinkTermThread {
    pub preferred_thread_id: Option<TtThreadId>,
    pub size: TerminalSize,
}

/// The Thread and workspace chosen by the server. A client resyncs mux
/// topology after this response before attempting to render the live tab.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct EnsureThinkTermThreadResponse {
    pub thread_id: TtThreadId,
    pub workspace: String,
    pub spawned: bool,
}

/// A complete renderer viewport. Native GUI clients include their per-pane
/// targets so that font scaling and pane chrome are restored when ownership
/// changes; cell-grid clients have a single canonical grid. Native clients
/// may include every member of a level-2 pane stack: those entries share one
/// `frame` but retain independently scaled `size` values. The wire shape is
/// unchanged; older servers safely treat the extra members as a partial frame
/// set and skip split-tree reconstruction.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub enum ClientViewport {
    CellGrid {
        size: TerminalSize,
    },
    Native {
        size: TerminalSize,
        panes: Vec<ClientPaneViewport>,
    },
}

impl ClientViewport {
    pub fn size(&self) -> TerminalSize {
        match self {
            Self::CellGrid { size } | Self::Native { size, .. } => *size,
        }
    }
}

#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct ClientPaneViewport {
    pub pane_id: PaneId,
    /// The PTY/render surface after subtracting frontend-only pane chrome and
    /// snapping to this pane's (possibly independently scaled) cell size.
    pub size: TerminalSize,
    /// The pane's containing rectangle in the root viewport's common cell
    /// and pixel coordinate system.  Split geometry must be rebuilt from this
    /// value rather than attempting to infer it from `size`.
    pub frame: TerminalSize,
}

/// Advertise one renderer's desired viewport for a remote tab. Advertising
/// does not steal ownership; the first renderer seeds an owner, and later
/// ownership changes only on real user interaction.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct SetClientViewport {
    pub tab_id: TabId,
    pub viewport: ClientViewport,
}

/// Claim access after a real terminal-area interaction and install the exact
/// geometry used for that interaction as one atomic server operation.
///
/// In shared mode this moves only `tab_id`'s layout lease. In handoff mode it
/// moves the connection-wide visibility/input lease. The registered transport
/// supplies the identity; no owner identity is accepted from the wire.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct ClaimClientViewport {
    pub tab_id: TabId,
    pub viewport: ClientViewport,
}

/// A is the tmux-like mode: every renderer may see and interact, while the
/// last terminal-area interaction on each tab chooses its canonical grid.
/// B is an exclusive handoff: only one renderer may see or interact with any
/// tab on this mux connection.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Copy, Default)]
pub enum FrontendAccessMode {
    TmuxLatest,
    #[default]
    Handoff,
}

/// Runtime connection-wide visibility/input ownership. The selected mode is
/// persisted by the server; owner and generation deliberately are not.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct FrontendAccessState {
    pub mode: FrontendAccessMode,
    pub owner: Option<ClientId>,
    pub generation: u64,
}

/// Change the server-wide mode. Supplying the active tab and its current
/// geometry lets the authorization check and resulting ownership/layout change
/// happen without an observable intermediate state.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct SetFrontendAccessMode {
    pub mode: FrontendAccessMode,
    pub tab_id: TabId,
    pub viewport: ClientViewport,
}

/// Authoritative viewport state returned by `SetClientViewport` and pushed
/// whenever the owner or canonical server grid changes.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct ClientViewportState {
    pub tab_id: TabId,
    /// Per-tab layout owner used by `TmuxLatest` mode.
    pub owner: Option<ClientId>,
    pub canonical_size: TerminalSize,
    /// What the owner is looking at, when the owner is a renderer that says.
    /// `None` means nobody offered one, and a follower keeps its own view.
    pub view: Option<ClientView>,
    pub generation: u64,
    /// A same-response snapshot of the connection-wide access state. This is
    /// what makes a handoff claim atomic from a renderer's point of view.
    pub access: FrontendAccessState,
}

/// What the renderer holding a tab's viewport is currently looking at.
///
/// Sharing the size makes two attached devices the same shape; it does not make
/// them the same view. Without this a phone and a desktop on one tab sit at
/// different points in the same scrollback, which reads as two sessions that
/// merely share a name.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Default)]
pub struct ClientView {
    /// Lines above the bottom of each pane's scrollback. A pane that is absent
    /// is following its output.
    pub scroll: Vec<ClientPaneScroll>,
}

#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct ClientPaneScroll {
    pub pane_id: PaneId,
    pub lines_from_bottom: u32,
}

/// Offer what this renderer is looking at, for the other renderers on the same
/// tab to follow. Ignored unless this client owns the tab's viewport — a
/// renderer nobody is using does not get to move everyone else.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct SetClientView {
    pub tab_id: TabId,
    pub view: ClientView,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct TabAddedToWindow {
    pub tab_id: TabId,
    pub window_id: WindowId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct TabResized {
    pub tab_id: TabId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct TabTitleChanged {
    pub tab_id: TabId,
    pub title: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct WindowTitleChanged {
    pub window_id: WindowId,
    pub title: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct PaneFocused {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct WindowWorkspaceChanged {
    pub window_id: WindowId,
    pub workspace: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SetClientId {
    pub client_id: ClientId,
    pub is_proxy: bool,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SetFocusedPane {
    pub pane_id: PaneId,
    /// GUI mux clients include their configured palette so that focus and OSC
    /// palette ownership change atomically. Non-rendering CLI clients use None.
    pub configured_palette: Option<ColorPalette>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetClientList;

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetClientListResponse {
    pub clients: Vec<ClientInfo>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct Resize {
    pub containing_tab_id: TabId,
    pub pane_id: PaneId,
    pub size: TerminalSize,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SetPaneZoomed {
    pub containing_tab_id: TabId,
    pub pane_id: PaneId,
    pub zoomed: bool,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetPaneDirection {
    pub pane_id: PaneId,
    pub direction: PaneDirection,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct AdjustPaneSize {
    pub pane_id: PaneId,
    pub direction: PaneDirection,
    pub amount: usize,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetPaneDirectionResponse {
    pub pane_id: Option<PaneId>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct ActivatePaneDirection {
    pub pane_id: PaneId,
    pub direction: PaneDirection,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetPaneRenderChanges {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetPaneRenderableDimensions {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetPaneRenderableDimensionsResponse {
    pub pane_id: PaneId,
    pub cursor_position: StableCursorPosition,
    pub dimensions: RenderableDimensions,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct LivenessResponse {
    pub pane_id: PaneId,
    pub is_alive: bool,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetPaneRenderChangesResponse {
    pub pane_id: PaneId,
    pub mouse_grabbed: bool,
    /// Whether the pane is showing the alternate screen. A renderer needs this
    /// to know that a wheel notch has no scrollback to travel through and
    /// belongs to the full-screen program instead.
    pub alt_screen: bool,
    /// How the program in the pane wants keys encoded. Without it a client
    /// treats every remote pane as plain xterm and never speaks the kitty
    /// keyboard protocol, or win32-input-mode, to a program that asked.
    pub keyboard_encoding: WireKeyboardEncoding,
    pub cursor_position: StableCursorPosition,
    pub dimensions: RenderableDimensions,
    pub dirty_lines: Vec<Range<StableRowIndex>>,
    pub title: String,
    pub working_dir: Option<SerdeUrl>,
    /// Lines that the server thought we'd almost certainly
    /// want to fetch as soon as we received this response
    pub bonus_lines: SerializedLines,

    pub input_serial: Option<InputSerial>,
    pub seqno: SequenceNo,
}

/// `termwiz::input::KeyboardEncoding` for the wire: the termwiz type has no
/// serde, and the kitty flags travel as their bits.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Copy, Default)]
pub enum WireKeyboardEncoding {
    #[default]
    Xterm,
    CsiU,
    Win32,
    Kitty(u16),
}

impl From<KeyboardEncoding> for WireKeyboardEncoding {
    fn from(encoding: KeyboardEncoding) -> Self {
        match encoding {
            KeyboardEncoding::Xterm => Self::Xterm,
            KeyboardEncoding::CsiU => Self::CsiU,
            KeyboardEncoding::Win32 => Self::Win32,
            KeyboardEncoding::Kitty(flags) => Self::Kitty(flags.bits()),
        }
    }
}

impl From<WireKeyboardEncoding> for KeyboardEncoding {
    fn from(encoding: WireKeyboardEncoding) -> Self {
        match encoding {
            WireKeyboardEncoding::Xterm => Self::Xterm,
            WireKeyboardEncoding::CsiU => Self::CsiU,
            WireKeyboardEncoding::Win32 => Self::Win32,
            // Bits this build does not know are dropped rather than refused:
            // a newer program's extra flag must not turn the whole protocol
            // off.
            WireKeyboardEncoding::Kitty(bits) => {
                Self::Kitty(KittyKeyboardFlags::from_bits_truncate(bits))
            }
        }
    }
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetLines {
    pub pane_id: PaneId,
    pub lines: Vec<Range<StableRowIndex>>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
struct CellCoordinates {
    line_idx: usize,
    cols: Range<usize>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
struct LineHyperlink {
    link: Hyperlink,
    coords: Vec<CellCoordinates>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct SerializedImageCell {
    pub line_idx: StableRowIndex,
    pub cell_idx: usize,
    // The following fields are taken from termwiz::image::ImageCell
    pub top_left: TextureCoordinate,
    pub bottom_right: TextureCoordinate,
    /// Image::data::hash() for the ImageCell::data field
    pub data_hash: [u8; 32],
    /// `ImageData::generation()` at serialization: the payload behind a
    /// hash changes in place as an animation grows, and this is how a
    /// client that has the hash learns its copy is behind.
    pub data_generation: u64,
    pub z_index: i32,
    pub padding_left: u16,
    pub padding_top: u16,
    pub padding_right: u16,
    pub padding_bottom: u16,
    pub image_id: Option<u32>,
    pub placement_id: Option<u32>,
}

/// What's all this?
/// Cells hold references to Arc<Hyperlink> and it is important to us to
/// maintain identity of the hyperlinks in the individual cells, while also
/// only sending a single copy of the associated URL.
/// This section of code extracts the hyperlinks from the cells and builds
/// up a mapping that can be used to restore the identity when the `lines()`
/// method is called.
#[derive(Deserialize, Serialize, PartialEq, Debug, Default)]
pub struct SerializedLines {
    lines: Vec<(StableRowIndex, Line)>,
    hyperlinks: Vec<LineHyperlink>,
    images: Vec<SerializedImageCell>,
}

impl SerializedLines {
    /// Reconsitute hyperlinks or other attributes that were decomposed for
    /// serialization, and return the line data.
    pub fn extract_data(self) -> (Vec<(StableRowIndex, Line)>, Vec<SerializedImageCell>) {
        let lines = if self.hyperlinks.is_empty() {
            self.lines
        } else {
            let mut lines = self.lines;

            for link in self.hyperlinks {
                let url = Arc::new(link.link);

                for coord in link.coords {
                    if let Some((_, line)) = lines.get_mut(coord.line_idx) {
                        if let Some(cells) =
                            line.cells_mut_for_attr_changes_only().get_mut(coord.cols)
                        {
                            for cell in cells {
                                cell.attrs_mut().set_hyperlink(Some(Arc::clone(&url)));
                            }
                        }
                    }
                }
            }

            lines
        };
        (lines, self.images)
    }
}

impl From<Vec<(StableRowIndex, Line)>> for SerializedLines {
    fn from(mut lines: Vec<(StableRowIndex, Line)>) -> Self {
        let mut hyperlinks = vec![];
        let mut images = vec![];

        for (line_idx, (stable_row_idx, line)) in lines.iter_mut().enumerate() {
            // The mutable pass below coerces the line to per-cell storage,
            // undoing the compression scrollback just applied and shipping
            // the fat form to the client. Only lines that actually carry
            // links or images have anything to extract; skip the rest.
            if !line.has_hyperlinks_or_images() {
                continue;
            }
            let mut current_link: Option<Arc<Hyperlink>> = None;
            let mut current_range = 0..0;

            for (x, cell) in line
                .cells_mut_for_attr_changes_only()
                .iter_mut()
                .enumerate()
            {
                // Unset the hyperlink on the cell, if any, and record that
                // in the hyperlinks data for later restoration.
                if let Some(link) = cell.attrs_mut().hyperlink().map(Arc::clone) {
                    cell.attrs_mut().set_hyperlink(None);
                    match current_link.as_ref() {
                        Some(current) if Arc::ptr_eq(&current, &link) => {
                            // Continue the current streak
                            current_range = range_union(current_range, x..x + 1);
                        }
                        Some(prior) => {
                            // It's a different URL, push the current data and start a new one
                            hyperlinks.push(LineHyperlink {
                                link: (**prior).clone(),
                                coords: vec![CellCoordinates {
                                    line_idx,
                                    cols: current_range,
                                }],
                            });
                            current_range = x..x + 1;
                            current_link = Some(link);
                        }
                        None => {
                            // Starting a new streak
                            current_range = x..x + 1;
                            current_link = Some(link);
                        }
                    }
                } else if let Some(link) = current_link.take() {
                    // Wrap up a prior streak
                    hyperlinks.push(LineHyperlink {
                        link: (*link).clone(),
                        coords: vec![CellCoordinates {
                            line_idx,
                            cols: current_range,
                        }],
                    });
                    current_range = 0..0;
                }

                if let Some(cell_images) = cell.attrs().images() {
                    for imcell in cell_images {
                        let (padding_left, padding_top, padding_right, padding_bottom) =
                            imcell.padding();
                        images.push(SerializedImageCell {
                            line_idx: *stable_row_idx,
                            cell_idx: x,
                            top_left: imcell.top_left(),
                            bottom_right: imcell.bottom_right(),
                            z_index: imcell.z_index(),
                            padding_left,
                            padding_top,
                            padding_right,
                            padding_bottom,
                            image_id: imcell.image_id(),
                            placement_id: imcell.placement_id(),
                            data_hash: imcell.image_data().hash(),
                            data_generation: imcell.image_data().generation(),
                        });
                    }
                }
                cell.attrs_mut().clear_images();
            }
            if let Some(link) = current_link.take() {
                // Wrap up final streak
                hyperlinks.push(LineHyperlink {
                    link: (*link).clone(),
                    coords: vec![CellCoordinates {
                        line_idx,
                        cols: current_range,
                    }],
                });
            }
        }

        Self {
            lines,
            hyperlinks,
            images,
        }
    }
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetLinesResponse {
    pub pane_id: PaneId,
    pub lines: SerializedLines,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct EraseScrollbackRequest {
    pub pane_id: PaneId,
    pub erase_mode: ScrollbackEraseMode,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SearchScrollbackRequest {
    pub pane_id: PaneId,
    pub pattern: thinkterm_proto::Pattern,
    pub range: Range<StableRowIndex>,
    pub limit: Option<u32>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SearchScrollbackResponse {
    pub results: Vec<thinkterm_proto::SearchResult>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetImageCell {
    pub pane_id: PaneId,
    pub line_idx: StableRowIndex,
    pub cell_idx: usize,
    pub data_hash: [u8; 32],
    /// The generation the client saw on the cell; informational.
    pub data_generation: u64,
    /// Animation frames the client already holds for this hash, so the
    /// server can send only the ones after them. 0 asks for everything.
    pub have_frames: u32,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetImageCellResponse {
    pub pane_id: PaneId,
    pub data: Option<Arc<ImageData>>,
    /// The generation `data` was taken from.
    pub data_generation: u64,
    /// 0: `data` is the whole image. Otherwise `data` is an AnimRgba8
    /// holding only the frames from this index on, together with the
    /// durations and per-frame hashes of *every* frame, so the client can
    /// check the frames it holds are still the ones in front.
    pub frames_from: u32,
}

#[cfg(test)]
mod golden {
    //! Byte-level fixtures for every type that is moving into
    //! thinkterm-proto. varbincode is positional -- no field names, no field
    //! counts -- so reordering fields during the move corrupts the wire
    //! without any error. These literals were captured before the move;
    //! they must stay green, byte for byte, after it.
    use super::*;
    use std::sync::Arc;
    use thinkterm_proto::{
        PaneEntry, PaneStackEntry, Pattern, SplitDirection, SplitDirectionAndSize, SplitSize,
    };

    fn varbincode_bytes<T: serde::Serialize>(t: &T) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut ser = varbincode::Serializer::new(&mut buf);
        t.serialize(&mut ser).unwrap();
        buf
    }

    fn size(rows: usize, cols: usize) -> TerminalSize {
        TerminalSize {
            rows,
            cols,
            pixel_width: cols * 8,
            pixel_height: rows * 16,
            dpi: 96,
        }
    }

    fn pane_entry(pane_id: PaneId, title: &str) -> PaneEntry {
        PaneEntry {
            window_id: 1,
            tab_id: 2,
            pane_id,
            title: title.to_string(),
            size: size(24, 80),
            working_dir: Some(
                std::convert::TryFrom::try_from("file:///tmp/x".to_string()).unwrap(),
            ),
            is_active_pane: true,
            is_zoomed_pane: false,
            alt_screen: true,
            workspace: "default".to_string(),
            cursor_pos: StableCursorPosition::default(),
            physical_top: -3,
            top_row: 0,
            left_col: 0,
            tty_name: Some("/dev/ttys001".to_string()),
        }
    }

    fn list_panes_response() -> ListPanesResponse {
        let mut window_titles = HashMap::new();
        window_titles.insert(1usize, "win".to_string());
        ListPanesResponse {
            tabs: vec![
                PaneNode::Empty,
                PaneNode::Split {
                    left: Box::new(PaneNode::Leaf(pane_entry(3, "left"))),
                    right: Box::new(PaneNode::Stack(PaneStackEntry {
                        active: 0,
                        panes: vec![pane_entry(4, "stacked")],
                        pane_stack_id: Some(7),
                    })),
                    node: SplitDirectionAndSize {
                        direction: SplitDirection::Horizontal,
                        first: size(24, 40),
                        second: size(24, 39),
                    },
                },
            ],
            tab_titles: vec!["main".to_string()],
            window_titles,
        }
    }

    fn split_pane() -> SplitPane {
        SplitPane {
            pane_id: 5,
            split_request: SplitRequest {
                direction: SplitDirection::Vertical,
                target_is_second: true,
                top_level: false,
                size: SplitSize::Percent(30),
            },
            command: None,
            command_dir: Some("/home".to_string()),
            domain: thinkterm_proto::SpawnTabDomain::DomainName("dom".to_string()),
            move_pane_id: Some(2),
        }
    }

    fn search_request() -> SearchScrollbackRequest {
        SearchScrollbackRequest {
            pane_id: 3,
            pattern: Pattern::Regex("a+".to_string()),
            range: -5..10,
            limit: Some(100),
        }
    }

    fn client_list_response() -> GetClientListResponse {
        let when = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        GetClientListResponse {
            clients: vec![ClientInfo {
                client_id: Arc::new(ClientId {
                    hostname: "host".to_string(),
                    username: "user".to_string(),
                    pid: 100,
                    epoch: 2,
                    id: 3,
                    ssh_auth_sock: None,
                }),
                connected_at: when,
                active_workspace: Some("default".to_string()),
                last_input: when,
                focused_pane_id: Some(4),
            }],
        }
    }

    fn render_changes_response() -> GetPaneRenderChangesResponse {
        GetPaneRenderChangesResponse {
            pane_id: 1,
            mouse_grabbed: false,
            alt_screen: true,
            keyboard_encoding: WireKeyboardEncoding::Kitty(3),
            cursor_position: StableCursorPosition::default(),
            dimensions: RenderableDimensions {
                cols: 80,
                viewport_rows: 24,
                scrollback_rows: 100,
                physical_top: -7,
                scrollback_top: -76,
                dpi: 96,
                pixel_width: 640,
                pixel_height: 384,
                reverse_video: false,
            },
            dirty_lines: vec![0..2, 5..6],
            title: "title".to_string(),
            working_dir: None,
            bonus_lines: Vec::new().into(),
            input_serial: None,
            seqno: 42,
        }
    }

    // Captured from the code as it stood before the type move; regenerating
    // them after a change defeats their purpose.
    const LIST_PANES: &[u8] = &[
        2, 0, 1, 2, 1, 2, 3, 4, 108, 101, 102, 116, 24, 80, 128, 5, 128, 3, 96, 1, 13, 102, 105,
        108, 101, 58, 47, 47, 47, 116, 109, 112, 47, 120, 1, 0, 1, 7, 100, 101, 102, 97, 117, 108,
        116, 0, 0, 0, 1, 125, 0, 0, 1, 12, 47, 100, 101, 118, 47, 116, 116, 121, 115, 48, 48, 49,
        3, 0, 1, 1, 2, 4, 7, 115, 116, 97, 99, 107, 101, 100, 24, 80, 128, 5, 128, 3, 96, 1, 13,
        102, 105, 108, 101, 58, 47, 47, 47, 116, 109, 112, 47, 120, 1, 0, 1, 7, 100, 101, 102, 97,
        117, 108, 116, 0, 0, 0, 1, 125, 0, 0, 1, 12, 47, 100, 101, 118, 47, 116, 116, 121, 115, 48,
        48, 49, 1, 7, 0, 24, 40, 192, 2, 128, 3, 96, 24, 39, 184, 2, 128, 3, 96, 1, 4, 109, 97,
        105, 110, 1, 1, 3, 119, 105, 110,
    ];
    const SPLIT_PANE: &[u8] = &[
        5, 1, 1, 0, 1, 30, 0, 1, 5, 47, 104, 111, 109, 101, 2, 3, 100, 111, 109, 1, 2,
    ];
    const SEARCH: &[u8] = &[3, 2, 2, 97, 43, 123, 10, 1, 100];
    const CLIENT_LIST: &[u8] = &[
        1, 4, 104, 111, 115, 116, 4, 117, 115, 101, 114, 100, 2, 3, 0, 128, 226, 207, 170, 6, 1, 7,
        100, 101, 102, 97, 117, 108, 116, 128, 226, 207, 170, 6, 1, 4,
    ];
    // Re-captured for codec 64, when `keyboard_encoding` (the `3, 3`
    // after `alt_screen`: variant Kitty, flag bits 3) joined the response.
    const RENDER_CHANGES: &[u8] = &[
        1, 0, 1, 3, 3, 0, 0, 0, 1, 80, 24, 100, 121, 180, 127, 96, 128, 5, 128, 3, 0, 2, 0, 2, 5,
        6, 5, 116, 105, 116, 108, 101, 0, 0, 0, 0, 0, 42,
    ];
    const LAYOUT_JSON: &str = r#"["Empty",{"Split":{"left":{"Leaf":{"window_id":1,"tab_id":2,"pane_id":3,"title":"left","size":{"rows":24,"cols":80,"pixel_width":640,"pixel_height":384,"dpi":96},"working_dir":"file:///tmp/x","is_active_pane":true,"is_zoomed_pane":false,"alt_screen":true,"workspace":"default","cursor_pos":{"x":0,"y":0,"shape":"Default","visibility":"Visible"},"physical_top":-3,"top_row":0,"left_col":0,"tty_name":"/dev/ttys001"}},"right":{"Stack":{"active":0,"panes":[{"window_id":1,"tab_id":2,"pane_id":4,"title":"stacked","size":{"rows":24,"cols":80,"pixel_width":640,"pixel_height":384,"dpi":96},"working_dir":"file:///tmp/x","is_active_pane":true,"is_zoomed_pane":false,"alt_screen":true,"workspace":"default","cursor_pos":{"x":0,"y":0,"shape":"Default","visibility":"Visible"},"physical_top":-3,"top_row":0,"left_col":0,"tty_name":"/dev/ttys001"}],"pane_stack_id":7}},"node":{"direction":"Horizontal","first":{"rows":24,"cols":40,"pixel_width":320,"pixel_height":384,"dpi":96},"second":{"rows":24,"cols":39,"pixel_width":312,"pixel_height":384,"dpi":96}}}}]"#;

    fn command_spec() -> CommandSpec {
        use thinkterm_proto::EnvVar;
        CommandSpec {
            args: vec![b"prog".to_vec(), vec![0x66, 0x80, 0x6f]],
            env: vec![
                EnvVar {
                    key: b"PATH".to_vec(),
                    value: b"/usr/bin".to_vec(),
                    is_from_base_env: true,
                },
                EnvVar {
                    key: b"FOO".to_vec(),
                    value: b"bar".to_vec(),
                    is_from_base_env: false,
                },
            ],
            cwd: Some(b"/tmp".to_vec()),
            umask: Some(0o022),
            controlling_tty: false,
        }
    }

    // The v60 baseline for CommandSpec, captured when the type was
    // introduced. The other literals guard against changing an old format;
    // this one guards against accidentally changing the new one.
    const COMMAND_SPEC: &[u8] = &[
        2, 4, 112, 114, 111, 103, 3, 102, 128, 111, 2, 4, 80, 65, 84, 72, 8, 47, 117, 115, 114, 47,
        98, 105, 110, 1, 3, 70, 79, 79, 3, 98, 97, 114, 0, 1, 4, 47, 116, 109, 112, 1, 18, 0,
    ];

    #[test]
    fn command_spec_bytes_are_stable() {
        assert_eq!(varbincode_bytes(&command_spec()), COMMAND_SPEC);
        let mut r = COMMAND_SPEC;
        let mut de = varbincode::Deserializer::new(&mut r);
        let back: CommandSpec = serde::Deserialize::deserialize(&mut de).unwrap();
        assert_eq!(back, command_spec());
    }

    #[test]
    fn wire_bytes_are_stable() {
        assert_eq!(varbincode_bytes(&list_panes_response()), LIST_PANES);
        assert_eq!(varbincode_bytes(&split_pane()), SPLIT_PANE);
        assert_eq!(varbincode_bytes(&search_request()), SEARCH);
        assert_eq!(varbincode_bytes(&client_list_response()), CLIENT_LIST);
        assert_eq!(varbincode_bytes(&render_changes_response()), RENDER_CHANGES);
    }

    #[test]
    fn wire_bytes_decode_back() {
        fn back<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> T {
            let mut r = bytes;
            let mut de = varbincode::Deserializer::new(&mut r);
            serde::Deserialize::deserialize(&mut de).unwrap()
        }
        assert_eq!(back::<ListPanesResponse>(LIST_PANES), list_panes_response());
        assert_eq!(back::<SplitPane>(SPLIT_PANE), split_pane());
        assert_eq!(back::<SearchScrollbackRequest>(SEARCH), search_request());
        assert_eq!(
            back::<GetClientListResponse>(CLIENT_LIST),
            client_list_response()
        );
        assert_eq!(
            back::<GetPaneRenderChangesResponse>(RENDER_CHANGES),
            render_changes_response()
        );
    }

    /// PaneNode/PaneEntry are also the on-disk serde_json format of saved
    /// Thread layouts; JSON is keyed by field *name*, so the move must not
    /// rename anything either. The last accidental format change here made
    /// every Thread open as a single empty pane and overwrite its own saved
    /// layout (see the comment on PaneEntry::alt_screen).
    #[test]
    fn layout_json_is_stable() {
        let tabs: Vec<PaneNode> = serde_json::from_str(LAYOUT_JSON).unwrap();
        assert_eq!(tabs, list_panes_response().tabs);
        assert_eq!(serde_json::to_string(&tabs).unwrap(), LAYOUT_JSON);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use termwiz::cell::CellAttributes;

    #[test]
    fn serializing_lines_keeps_plain_lines_compressed() {
        let mut line = Line::from_text("hello world", &CellAttributes::default(), 1, None);
        line.compress_for_scrollback();

        let serialized = SerializedLines::from(vec![(0, line)]);
        assert!(serialized.hyperlinks.is_empty());

        let (lines, images) = serialized.extract_data();
        assert!(images.is_empty());
        assert!(
            lines[0].1.is_compressed_for_scrollback(),
            "a line with nothing to extract must not be coerced to per-cell storage"
        );
        assert_eq!(lines[0].1.as_str(), "hello world");
    }

    #[test]
    fn serializing_lines_still_extracts_and_restores_hyperlinks() {
        let mut attrs = CellAttributes::default();
        attrs.set_hyperlink(Some(Arc::new(Hyperlink::new("https://example.com"))));
        let mut line = Line::from_text("link", &attrs, 1, None);
        line.compress_for_scrollback();

        let serialized = SerializedLines::from(vec![(0, line)]);
        assert!(!serialized.hyperlinks.is_empty());

        let (lines, _) = serialized.extract_data();
        let cell = lines[0].1.visible_cells().next().unwrap();
        assert_eq!(
            cell.attrs().hyperlink().map(|link| link.uri()),
            Some("https://example.com")
        );
    }

    /// First coverage of the compressed path: every pre-existing test payload
    /// sits under COMPRESS_THRESH and never touches zstd.
    #[test]
    fn compressed_serialize_round_trips() {
        let payload: Vec<u8> = std::iter::repeat(b"thinkterm ".as_slice())
            .take(100)
            .flatten()
            .copied()
            .collect();
        let (bytes, is_compressed) = serialize(&payload).unwrap();
        assert!(is_compressed, "1000 repetitive bytes must compress");
        let back: Vec<u8> = deserialize(bytes.as_slice(), is_compressed).unwrap();
        assert_eq!(back, payload);
    }

    /// The wasm client's receive path: a native server compresses with the
    /// zstd C library, the wasm side decodes with pure-Rust ruzstd. Proven
    /// here on native, where both libraries are available.
    #[test]
    fn ruzstd_decodes_zstd_output() {
        let payload: Vec<u8> = (0u32..500).flat_map(|v| v.to_le_bytes()).collect();
        let (bytes, is_compressed) = serialize(&payload).unwrap();
        assert!(is_compressed);

        let mut decompress =
            ruzstd::decoding::StreamingDecoder::new(bytes.as_slice()).expect("ruzstd frame");
        let mut decode = varbincode::Deserializer::new(&mut decompress);
        let back: Vec<u8> = serde::Deserialize::deserialize(&mut decode).unwrap();
        assert_eq!(back, payload);
    }

    #[test]
    fn test_frame() {
        let mut encoded = Vec::new();
        encode_raw(0x81, 0x42, b"hello", false, &mut encoded).unwrap();
        assert_eq!(&encoded, b"\x08\x42\x81\x01hello");
        let decoded = decode_raw(encoded.as_slice()).unwrap();
        assert_eq!(decoded.ident, 0x81);
        assert_eq!(decoded.serial, 0x42);
        assert_eq!(decoded.data, b"hello");
    }

    #[test]
    fn test_frame_lengths() {
        let mut serial = 1;
        for target_len in &[128, 247, 256, 65536, 16777216] {
            let mut payload = Vec::with_capacity(*target_len);
            payload.resize(*target_len, b'a');
            let mut encoded = Vec::new();
            encode_raw(0x42, serial, payload.as_slice(), false, &mut encoded).unwrap();
            let decoded = decode_raw(encoded.as_slice()).unwrap();
            assert_eq!(decoded.ident, 0x42);
            assert_eq!(decoded.serial, serial);
            assert_eq!(decoded.data, payload);
            serial += 1;
        }
    }

    /// All three spawn-class PDUs, populated with a CommandSpec that
    /// exercises every field: non-utf8 argv bytes, base and explicit env
    /// entries, a cwd, a umask and a lowered tty flag. Pure codec -- no pty
    /// dependency -- which also proves the crate is self-contained.
    #[test]
    fn spawn_pdus_round_trip_at_version_60() {
        use thinkterm_proto::{EnvVar, SpawnTabDomain};

        let spec = CommandSpec {
            args: vec![b"htop".to_vec(), vec![0x66, 0x80, 0x6f]],
            env: vec![
                EnvVar {
                    key: b"PATH".to_vec(),
                    value: b"/usr/bin".to_vec(),
                    is_from_base_env: true,
                },
                EnvVar {
                    key: b"FOO".to_vec(),
                    value: b"bar".to_vec(),
                    is_from_base_env: false,
                },
            ],
            cwd: Some(b"/tmp".to_vec()),
            umask: Some(0o022),
            controlling_tty: false,
        };

        let pdus = [
            Pdu::SpawnV2(SpawnV2 {
                domain: SpawnTabDomain::DefaultDomain,
                window_id: Some(1),
                command: Some(spec.clone()),
                command_dir: None,
                size: TerminalSize::default(),
                workspace: "default".to_string(),
            }),
            Pdu::SplitPane(SplitPane {
                pane_id: 2,
                split_request: SplitRequest::default(),
                command: Some(spec.clone()),
                command_dir: Some("/home".to_string()),
                domain: SpawnTabDomain::CurrentPaneDomain,
                move_pane_id: None,
            }),
            Pdu::SpawnPaneInStack(SpawnPaneInStack {
                pane_id: 3,
                command: None,
                command_dir: None,
                domain: SpawnTabDomain::DomainName("dom".to_string()),
            }),
        ];

        for pdu in pdus {
            let mut encoded = Vec::new();
            pdu.encode(&mut encoded, 0x11).unwrap();
            let decoded = Pdu::decode(encoded.as_slice()).unwrap();
            assert_eq!(decoded.pdu, pdu);
            assert_eq!(decoded.serial, 0x11);
        }
    }

    #[test]
    fn test_pdu_ping() {
        let mut encoded = Vec::new();
        Pdu::Ping(Ping {}).encode(&mut encoded, 0x40).unwrap();
        assert_eq!(&encoded, &[2, 0x40, 1]);
        assert_eq!(
            DecodedPdu {
                serial: 0x40,
                pdu: Pdu::Ping(Ping {})
            },
            Pdu::decode(encoded.as_slice()).unwrap()
        );
    }

    #[test]
    fn move_pane_to_stack_round_trip() {
        let request = MovePaneToStack {
            source_pane_id: 17,
            target_pane_id: 29,
        };
        let mut encoded = Vec::new();
        Pdu::MovePaneToStack(request)
            .encode(&mut encoded, 0x41)
            .unwrap();
        assert_eq!(
            Pdu::decode(encoded.as_slice()).unwrap(),
            DecodedPdu {
                serial: 0x41,
                pdu: Pdu::MovePaneToStack(MovePaneToStack {
                    source_pane_id: 17,
                    target_pane_id: 29,
                }),
            }
        );
    }

    #[test]
    fn thinkterm_tree_round_trip() {
        let mut tree = ThinkTermTree::default();
        for op in [
            TreeOp::CreateSpace {
                space_id: "s1".into(),
                name: "Work".into(),
            },
            TreeOp::CreateProject {
                project_id: "p1".into(),
                space_id: "s1".into(),
                name: "thinkterm".into(),
                path: "/srv/projects/example".into(),
            },
            TreeOp::CreateThread {
                thread_id: "t1".into(),
                project_id: "p1".into(),
                name: "main".into(),
                workspace: Some("thinkterm:p1:t1".into()),
                created_at: 1_700_000_000,
            },
            TreeOp::SetThreadPinned {
                thread_id: "t1".into(),
                pinned: true,
                last_active_at: 1_700_000_001,
            },
            // A second live project so archiving p1 passes the Space's
            // last-live-project guard.
            TreeOp::CreateProject {
                project_id: "p2".into(),
                space_id: "s1".into(),
                name: "notes".into(),
                path: "/srv/projects/notes".into(),
            },
            TreeOp::SetProjectArchived {
                project_id: "p1".into(),
                archived_at: Some(1_700_000_002),
            },
        ] {
            assert!(apply_op(&mut tree, &op));
        }

        let mut encoded = Vec::new();
        Pdu::ThinkTermTreeState(ThinkTermTreeState { tree: tree.clone() })
            .encode(&mut encoded, 0x11)
            .unwrap();
        assert_eq!(
            Pdu::decode(encoded.as_slice()).unwrap(),
            DecodedPdu {
                serial: 0x11,
                pdu: Pdu::ThinkTermTreeState(ThinkTermTreeState { tree }),
            }
        );

        // The ops travel in the other direction, in batches.
        let ops = vec![
            TreeOp::MoveThreadBefore {
                project_id: "p1".into(),
                thread_id: "t2".into(),
                before: None,
            },
            TreeOp::SetProjectArchived {
                project_id: "p1".into(),
                archived_at: None,
            },
            TreeOp::DeleteSpace {
                space_id: "s1".into(),
            },
        ];
        let mut encoded = Vec::new();
        Pdu::MutateThinkTermTree(MutateThinkTermTree { ops: ops.clone() })
            .encode(&mut encoded, 0x12)
            .unwrap();
        assert_eq!(
            Pdu::decode(encoded.as_slice()).unwrap(),
            DecodedPdu {
                serial: 0x12,
                pdu: Pdu::MutateThinkTermTree(MutateThinkTermTree { ops }),
            }
        );
    }

    #[test]
    fn agent_status_protocol_round_trip_at_version_61() {
        // The agent-status protocol arrived at 61 and is unchanged since.
        // The exact assertion is the tripwire: whoever bumps the codec must
        // come here, confirm the round-trips still cover the new version,
        // and advance it deliberately.
        assert_eq!(CODEC_VERSION, 64);
        use thinkterm_proto::{AgentEvidence, AgentState, AgentStatus};

        fn round_trip(pdu: Pdu) {
            let mut encoded = Vec::new();
            pdu.encode(&mut encoded, 0x61).unwrap();
            let decoded = Pdu::decode(encoded.as_slice()).unwrap();
            assert_eq!(decoded.serial, 0x61);
            assert_eq!(decoded.pdu, pdu);
        }

        let status = AgentStatus {
            agent_id: "claude".to_string(),
            state: AgentState::Blocked,
            evidence: AgentEvidence::Screen,
            session_id: Some("abc-123".to_string()),
            since_unix: 1_756_000_000,
            ended: false,
        };
        round_trip(Pdu::AgentStatusChanged(AgentStatusChanged {
            pane_id: 7,
            status: Some(status.clone()),
        }));
        round_trip(Pdu::AgentStatusChanged(AgentStatusChanged {
            pane_id: 7,
            status: None,
        }));
        round_trip(Pdu::GetAgentStatuses(GetAgentStatuses {}));
        round_trip(Pdu::GetAgentStatusesResponse(GetAgentStatusesResponse {
            statuses: vec![AgentStatusEntry {
                pane_id: 7,
                status,
                title: "claude".to_string(),
                workspace: "default".to_string(),
            }],
        }));
    }

    #[test]
    fn thinkterm_session_viewport_and_landing_protocol_round_trip() {
        let size = TerminalSize {
            rows: 40,
            cols: 132,
            pixel_width: 1056,
            pixel_height: 640,
            dpi: 96,
        };
        let viewport = SetClientViewport {
            tab_id: 17,
            viewport: ClientViewport::Native {
                size,
                panes: vec![ClientPaneViewport {
                    pane_id: 23,
                    size: TerminalSize {
                        rows: 38,
                        cols: 129,
                        pixel_width: 1032,
                        pixel_height: 608,
                        dpi: 96,
                    },
                    frame: size,
                }],
            },
        };
        let mut encoded = Vec::new();
        Pdu::SetClientViewport(viewport.clone())
            .encode(&mut encoded, 0x52)
            .unwrap();
        assert_eq!(
            Pdu::decode(encoded.as_slice()).unwrap(),
            DecodedPdu {
                serial: 0x52,
                pdu: Pdu::SetClientViewport(viewport),
            }
        );

        let claim = ClaimClientViewport {
            tab_id: 17,
            viewport: ClientViewport::CellGrid { size },
        };
        let mut encoded = Vec::new();
        Pdu::ClaimClientViewport(claim.clone())
            .encode(&mut encoded, 0x53)
            .unwrap();
        assert_eq!(
            Pdu::decode(encoded.as_slice()).unwrap(),
            DecodedPdu {
                serial: 0x53,
                pdu: Pdu::ClaimClientViewport(claim),
            }
        );

        let mode = SetFrontendAccessMode {
            mode: FrontendAccessMode::TmuxLatest,
            tab_id: 17,
            viewport: ClientViewport::CellGrid { size },
        };
        encoded.clear();
        Pdu::SetFrontendAccessMode(mode.clone())
            .encode(&mut encoded, 0x54)
            .unwrap();
        assert_eq!(
            Pdu::decode(encoded.as_slice()).unwrap(),
            DecodedPdu {
                serial: 0x54,
                pdu: Pdu::SetFrontendAccessMode(mode),
            }
        );

        let access = FrontendAccessState {
            mode: FrontendAccessMode::Handoff,
            owner: None,
            generation: 27,
        };
        encoded.clear();
        Pdu::FrontendAccessState(access.clone())
            .encode(&mut encoded, 0x55)
            .unwrap();
        assert_eq!(
            Pdu::decode(encoded.as_slice()).unwrap(),
            DecodedPdu {
                serial: 0x55,
                pdu: Pdu::FrontendAccessState(access),
            }
        );

        let session = ThinkTermSessionState {
            server_id: "server-runtime-id".into(),
            tree_revision: 9,
            generation: 11,
            spaces: vec![ThinkTermSessionSpace {
                id: "space".into(),
                name: "Space".into(),
                ..Default::default()
            }],
            projects: vec![ThinkTermSessionProject {
                id: "project".into(),
                space_id: "space".into(),
                name: "Project".into(),
                threads: vec![ThinkTermSessionThread {
                    id: "thread".into(),
                    project_id: "project".into(),
                    name: "main".into(),
                    tabs: vec![ThinkTermSessionTab {
                        window_id: 3,
                        tab_id: 17,
                        pane_ids: vec![21, 22],
                        title: "shell".into(),
                        is_active: true,
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        encoded.clear();
        Pdu::ThinkTermSessionState(session.clone())
            .encode(&mut encoded, 0x53)
            .unwrap();
        assert_eq!(
            Pdu::decode(encoded.as_slice()).unwrap(),
            DecodedPdu {
                serial: 0x53,
                pdu: Pdu::ThinkTermSessionState(session),
            }
        );

        let request = EnsureThinkTermThread {
            preferred_thread_id: Some("thread".into()),
            size,
        };
        encoded.clear();
        Pdu::EnsureThinkTermThread(request.clone())
            .encode(&mut encoded, 0x54)
            .unwrap();
        assert_eq!(
            Pdu::decode(encoded.as_slice()).unwrap(),
            DecodedPdu {
                serial: 0x54,
                pdu: Pdu::EnsureThinkTermThread(request),
            }
        );

        let response = EnsureThinkTermThreadResponse {
            thread_id: "thread".into(),
            workspace: "thinkterm:project:thread".into(),
            spawned: true,
        };
        encoded.clear();
        Pdu::EnsureThinkTermThreadResponse(response.clone())
            .encode(&mut encoded, 0x55)
            .unwrap();
        assert_eq!(
            Pdu::decode(encoded.as_slice()).unwrap(),
            DecodedPdu {
                serial: 0x55,
                pdu: Pdu::EnsureThinkTermThreadResponse(response),
            }
        );
    }

    #[test]
    fn application_palette_state_round_trip() {
        for palette in [None, Some(ColorPalette::default())] {
            let mut encoded = Vec::new();
            Pdu::SetApplicationPalette(SetApplicationPalette {
                pane_id: 17,
                palette: palette.clone(),
            })
            .encode(&mut encoded, 0x42)
            .unwrap();
            assert_eq!(
                Pdu::decode(encoded.as_slice()).unwrap(),
                DecodedPdu {
                    serial: 0x42,
                    pdu: Pdu::SetApplicationPalette(SetApplicationPalette {
                        pane_id: 17,
                        palette,
                    }),
                }
            );
        }
    }

    #[test]
    fn focused_pane_palette_round_trip() {
        let mut encoded = Vec::new();
        Pdu::SetFocusedPane(SetFocusedPane {
            pane_id: 29,
            configured_palette: Some(ColorPalette::default()),
        })
        .encode(&mut encoded, 0x43)
        .unwrap();
        assert_eq!(
            Pdu::decode(encoded.as_slice()).unwrap(),
            DecodedPdu {
                serial: 0x43,
                pdu: Pdu::SetFocusedPane(SetFocusedPane {
                    pane_id: 29,
                    configured_palette: Some(ColorPalette::default()),
                }),
            }
        );
    }

    #[test]
    fn stream_decode() {
        let mut encoded = Vec::new();
        Pdu::Ping(Ping {}).encode(&mut encoded, 0x1).unwrap();
        Pdu::Pong(Pong {}).encode(&mut encoded, 0x2).unwrap();
        assert_eq!(encoded.len(), 6);

        let mut cursor = Cursor::new(encoded.as_slice());
        let mut read_buffer = Vec::new();

        assert_eq!(
            Pdu::try_read_and_decode(&mut cursor, &mut read_buffer).unwrap(),
            Some(DecodedPdu {
                serial: 1,
                pdu: Pdu::Ping(Ping {})
            })
        );
        assert_eq!(
            Pdu::try_read_and_decode(&mut cursor, &mut read_buffer).unwrap(),
            Some(DecodedPdu {
                serial: 2,
                pdu: Pdu::Pong(Pong {})
            })
        );
        let err = Pdu::try_read_and_decode(&mut cursor, &mut read_buffer).unwrap_err();
        assert_eq!(
            err.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn test_pdu_ping_base91() {
        let mut encoded = Vec::new();
        {
            let mut encoder = base91::Base91Encoder::new(&mut encoded);
            Pdu::Ping(Ping {}).encode(&mut encoder, 0x41).unwrap();
        }
        assert_eq!(&encoded, &[60, 67, 75, 65]);
        let decoded = base91::decode(&encoded);
        assert_eq!(
            DecodedPdu {
                serial: 0x41,
                pdu: Pdu::Ping(Ping {})
            },
            Pdu::decode(decoded.as_slice()).unwrap()
        );
    }

    #[test]
    fn test_pdu_pong() {
        let mut encoded = Vec::new();
        Pdu::Pong(Pong {}).encode(&mut encoded, 0x42).unwrap();
        assert_eq!(&encoded, &[2, 0x42, 2]);
        assert_eq!(
            DecodedPdu {
                serial: 0x42,
                pdu: Pdu::Pong(Pong {})
            },
            Pdu::decode(encoded.as_slice()).unwrap()
        );
    }

    #[test]
    fn test_bogus_pdu() {
        let mut encoded = Vec::new();
        encode_raw(0xdeadbeef, 0x42, b"hello", false, &mut encoded).unwrap();
        assert_eq!(
            DecodedPdu {
                serial: 0x42,
                pdu: Pdu::Invalid { ident: 0xdeadbeef }
            },
            Pdu::decode(encoded.as_slice()).unwrap()
        );
    }
}

#[cfg(test)]
mod keyboard_encoding_tests {
    use super::*;

    #[test]
    fn the_keyboard_encoding_survives_the_wire_both_ways() {
        for encoding in [
            KeyboardEncoding::Xterm,
            KeyboardEncoding::CsiU,
            KeyboardEncoding::Win32,
            KeyboardEncoding::Kitty(
                KittyKeyboardFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KittyKeyboardFlags::REPORT_EVENT_TYPES,
            ),
        ] {
            let wire: WireKeyboardEncoding = encoding.into();
            let back: KeyboardEncoding = wire.into();
            assert_eq!(back, encoding);
        }
    }

    #[test]
    fn unknown_kitty_bits_are_dropped_not_fatal() {
        let back: KeyboardEncoding = WireKeyboardEncoding::Kitty(0x8000 | 1).into();
        assert_eq!(
            back,
            KeyboardEncoding::Kitty(KittyKeyboardFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
}
