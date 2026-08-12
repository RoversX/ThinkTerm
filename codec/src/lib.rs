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

use anyhow::{Context as _, Error, bail};
use config::keyassignment::{PaneDirection, ScrollbackEraseMode};
use mux::client::{ClientId, ClientInfo};
use mux::pane::PaneId;
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::{PaneNode, SerdeUrl, SplitRequest, TabId};
use mux::window::WindowId;
use portable_pty::CommandBuilder;
use rangeset::*;
use serde::{Deserialize, Serialize};
use smol::io::AsyncWriteExt;
use smol::prelude::*;
use std::collections::HashMap;
use std::convert::TryInto;
use std::io::Cursor;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use termwiz::hyperlink::Hyperlink;
use termwiz::image::{ImageData, TextureCoordinate};
use termwiz::surface::{Line, SequenceNo};
use thiserror::Error;
use wezterm_term::color::ColorPalette;
use wezterm_term::{Alert, ClipboardSelection, StableRowIndex, TerminalSize};

pub mod thinkterm_tree;
pub use thinkterm_tree::{
    ThinkTermTree, TreeOp, TtProject, TtProjectId, TtSpace, TtSpaceId, TtThread, TtThreadId,
    apply_op, ensure_unique_thread_names,
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
const COMPRESS_THRESH: usize = 32;

fn serialize<T: serde::Serialize>(t: &T) -> Result<(Vec<u8>, bool), Error> {
    let mut uncompressed = Vec::new();
    let mut encode = varbincode::Serializer::new(&mut uncompressed);
    t.serialize(&mut encode)?;

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

fn deserialize<T: serde::de::DeserializeOwned, R: std::io::Read>(
    mut r: R,
    is_compressed: bool,
) -> Result<T, Error> {
    if is_compressed {
        let mut decompress = zstd::Decoder::new(r)?;
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
pub const CODEC_VERSION: usize = 59;

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
    pub command: Option<CommandBuilder>,
    pub command_dir: Option<String>,
    pub domain: config::keyassignment::SpawnTabDomain,
    /// Instead of spawning a command, move the specified
    /// pane into the new split target
    pub move_pane_id: Option<PaneId>,
}

/// Spawn a new pane as a level-2 tab in the pane stack that contains
/// `pane_id`. Responds with SpawnResponse describing the new pane.
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SpawnPaneInStack {
    pub pane_id: PaneId,
    pub command: Option<CommandBuilder>,
    pub command_dir: Option<String>,
    pub domain: config::keyassignment::SpawnTabDomain,
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
    pub domain: config::keyassignment::SpawnTabDomain,
    /// If None, create a new window for this new tab
    pub window_id: Option<WindowId>,
    pub command: Option<CommandBuilder>,
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
    pub pattern: mux::pane::Pattern,
    pub range: Range<StableRowIndex>,
    pub limit: Option<u32>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SearchScrollbackResponse {
    pub results: Vec<mux::pane::SearchResult>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetImageCell {
    pub pane_id: PaneId,
    pub line_idx: StableRowIndex,
    pub cell_idx: usize,
    pub data_hash: [u8; 32],
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetImageCellResponse {
    pub pane_id: PaneId,
    pub data: Option<Arc<ImageData>>,
}

#[cfg(test)]
mod test {
    use super::*;

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
    fn thinkterm_session_viewport_and_landing_protocol_round_trip_at_version_59() {
        assert_eq!(CODEC_VERSION, 59);
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
