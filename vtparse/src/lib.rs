//! An implementation of the state machine described by
//! [DEC ANSI Parser](https://vt100.net/emu/dec_ansi_parser), modified to support UTF-8.
//!
//! This is sufficient to broadly categorize ANSI/ECMA-48 escape sequences that are
//! commonly used in terminal emulators.  It does not ascribe semantic meaning to
//! those escape sequences; for example, if you wish to parse the SGR sequence
//! that makes text bold, you will need to know which codes correspond to bold
//! in your implementation of `VTActor`.
//!
//! You may wish to use `termwiz::escape::parser::Parser` in the
//! [termwiz](https://docs.rs/termwiz/) crate if you don't want to have to research
//! all those possible escape sequences for yourself.
#![allow(clippy::upper_case_acronyms)]
#![cfg_attr(not(feature = "std"), no_std)]
use utf8parse::Parser as Utf8Parser;
mod enums;
use crate::enums::*;
mod transitions;

#[cfg(any(feature = "std", feature = "alloc"))]
extern crate alloc;

#[cfg(any(feature = "std", feature = "alloc"))]
use alloc::vec::Vec;
#[cfg(all(not(feature = "std"), not(feature = "alloc")))]
use heapless::Vec;

use transitions::{ENTRY, EXIT, TRANSITIONS};

#[inline(always)]
fn lookup(state: State, b: u8) -> (Action, State) {
    let v = unsafe {
        TRANSITIONS
            .get_unchecked(state as usize)
            .get_unchecked(b as usize)
    };
    (Action::from_u16(v >> 8), State::from_u16(v & 0xff))
}

#[inline(always)]
#[cfg(not(test))]
fn lookup_entry(state: State) -> Action {
    unsafe { *ENTRY.get_unchecked(state as usize) }
}

#[inline(always)]
#[cfg(test)]
fn lookup_entry(state: State) -> Action {
    *ENTRY
        .get(state as usize)
        .unwrap_or_else(|| panic!("State {:?} has no entry in ENTRY", state))
}

#[inline(always)]
#[cfg(test)]
fn lookup_exit(state: State) -> Action {
    *EXIT
        .get(state as usize)
        .unwrap_or_else(|| panic!("State {:?} has no entry in EXIT", state))
}

#[inline(always)]
#[cfg(not(test))]
fn lookup_exit(state: State) -> Action {
    unsafe { *EXIT.get_unchecked(state as usize) }
}

/// `VTActor` is a trait that allows the host application to process
/// the different kinds of sequence as they are parsed from the input
/// stream.
///
/// The functions defined by this trait correspond to the actions defined
/// in the [state machine](https://vt100.net/emu/dec_ansi_parser).
///
/// ## Terminology:
/// An intermediate is a character in the range 0x20-0x2f that
/// occurs before the final character in an escape sequence.
///
/// `ignored_excess_intermediates` is a boolean that is set in the case
/// where there were more than two intermediate characters; no standard
/// defines any codes with more than two.  Intermediates after
/// the second will set this flag and are discarded.
///
/// `params` in most of the functions of this trait are decimal integer parameters in escape
/// sequences.  They are separated by semicolon characters.  An omitted parameter is returned in
/// this interface as a zero, which represents the default value for that parameter.
///
/// Other jargon used here is defined in
/// [ECMA-48](http://www.ecma-international.org/publications/files/ECMA-ST/ECMA-48,%202nd%20Edition,%20August%201979.pdf).
pub trait VTActor {
    /// The current code should be mapped to a glyph according to the character set mappings and
    /// shift states in effect, and that glyph should be displayed.
    ///
    /// If the input was UTF-8 then it will have been mapped to a unicode code point.  Invalid
    /// sequences are represented here using the unicode REPLACEMENT_CHARACTER.
    ///
    /// Otherwise the parameter will be a 7-bit printable value and may be subject to mapping
    /// depending on other state maintained by the embedding application.
    ///
    /// ## Some commentary from the state machine documentation:
    /// GL characters (20 to 7F) are
    /// printed. 20 (SP) and 7F (DEL) are included in this area, although both codes have special
    /// behaviour. If a 94-character set is mapped into GL, 20 will cause a space to be displayed,
    /// and 7F will be ignored. When a 96-character set is mapped into GL, both 20 and 7F may cause
    /// a character to be displayed. Later models of the VT220 included the DEC Multinational
    /// Character Set (MCS), which has 94 characters in its supplemental set (i.e. the characters
    /// supplied in addition to ASCII), so terminals only claiming VT220 compatibility can always
    /// ignore 7F. The VT320 introduced ISO Latin-1, which has 96 characters in its supplemental
    /// set, so emulators with a VT320 compatibility mode need to treat 7F as a printable
    /// character.
    fn print(&mut self, b: char);

    /// Print a non-empty run of bytes in the range 0x20..=0x7e.
    /// The default preserves the per-character callback for existing actors.
    fn print_ascii(&mut self, text: &str) {
        for byte in text.bytes() {
            self.print(byte as char);
        }
    }

    /// Print a non-empty run of valid UTF-8 without C0, DEL or C1 controls.
    /// Runs need not end at a grapheme boundary. The default retains the
    /// per-character callback for actors that do not support borrowed text.
    fn print_utf8(&mut self, text: &str) {
        for c in text.chars() {
            self.print(c);
        }
    }

    /// The C0 or C1 control function should be executed, which may have any one of a variety of
    /// effects, including changing the cursor position, suspending or resuming communications or
    /// changing the shift states in effect.
    ///
    /// See [ECMA-48](http://www.ecma-international.org/publications/files/ECMA-ST/ECMA-48,%202nd%20Edition,%20August%201979.pdf)
    /// for more information on C0 and C1 control functions.
    fn execute_c0_or_c1(&mut self, control: u8);

    /// invoked when a final character arrives in the first part of a device control string. It
    /// determines the control function from the private marker, intermediate character(s) and
    /// final character, and executes it, passing in the parameter list. It also selects a handler
    /// function for the rest of the characters in the control string.
    ///
    /// See [ECMA-48](http://www.ecma-international.org/publications/files/ECMA-ST/ECMA-48,%202nd%20Edition,%20August%201979.pdf)
    /// for more information on device control strings.
    fn dcs_hook(
        &mut self,
        mode: u8,
        params: &[i64],
        intermediates: &[u8],
        ignored_excess_intermediates: bool,
    );

    /// This action passes characters from the data string part of a device control string to a
    /// handler that has previously been selected by the dcs_hook action. C0 controls are also
    /// passed to the handler.
    ///
    /// See [ECMA-48](http://www.ecma-international.org/publications/files/ECMA-ST/ECMA-48,%202nd%20Edition,%20August%201979.pdf)
    /// for more information on device control strings.
    fn dcs_put(&mut self, byte: u8);

    /// When a device control string is terminated by ST, CAN, SUB or ESC, this action calls the
    /// previously selected handler function with an “end of data” parameter. This allows the
    /// handler to finish neatly.
    ///
    /// See [ECMA-48](http://www.ecma-international.org/publications/files/ECMA-ST/ECMA-48,%202nd%20Edition,%20August%201979.pdf)
    /// for more information on device control strings.
    fn dcs_unhook(&mut self);

    /// The final character of an escape sequence has arrived, so determine the control function
    /// to be executed from the intermediate character(s) and final character, and execute it.
    ///
    /// See [ECMA-48](http://www.ecma-international.org/publications/files/ECMA-ST/ECMA-48,%202nd%20Edition,%20August%201979.pdf)
    /// for more information on escape sequences.
    fn esc_dispatch(
        &mut self,
        params: &[i64],
        intermediates: &[u8],
        ignored_excess_intermediates: bool,
        byte: u8,
    );

    /// A final character of a Control Sequence Initiator has arrived, so determine the control function to be executed from
    /// private marker, intermediate character(s) and final character, and execute it, passing in
    /// the parameter list.
    ///
    /// See [ECMA-48](http://www.ecma-international.org/publications/files/ECMA-ST/ECMA-48,%202nd%20Edition,%20August%201979.pdf)
    /// for more information on control functions.
    fn csi_dispatch(&mut self, params: &[CsiParam], parameters_truncated: bool, byte: u8);

    /// Called when an OSC string is terminated by ST, CAN, SUB or ESC.
    ///
    /// `params` is an array of byte strings (which may also be valid utf-8)
    /// that were passed as semicolon separated parameters to the operating
    /// system command.
    fn osc_dispatch(&mut self, params: &[&[u8]]);

    /// Called when an APC string is terminated by ST
    /// `data` is the data contained within the APC sequence.
    #[cfg(any(feature = "std", feature = "alloc"))]
    fn apc_dispatch(&mut self, data: Vec<u8>);
}

/// `VTAction` is an alternative way to work with the parser; rather
/// than implementing the VTActor trait you can use `CollectingVTActor`
/// to capture the sequence of events into a `Vec<VTAction>`.
#[cfg(any(feature = "std", feature = "alloc"))]
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum VTAction {
    Print(char),
    ExecuteC0orC1(u8),
    DcsHook {
        params: Vec<i64>,
        intermediates: Vec<u8>,
        ignored_excess_intermediates: bool,
        byte: u8,
    },
    DcsPut(u8),
    DcsUnhook,
    EscDispatch {
        params: Vec<i64>,
        intermediates: Vec<u8>,
        ignored_excess_intermediates: bool,
        byte: u8,
    },
    CsiDispatch {
        params: Vec<CsiParam>,
        parameters_truncated: bool,
        byte: u8,
    },
    OscDispatch(Vec<Vec<u8>>),
    ApcDispatch(Vec<u8>),
}

/// This is an implementation of `VTActor` that captures the events
/// into an internal vector.
/// It can be iterated via `into_iter` or have the internal
/// vector extracted via `into_vec`.
#[cfg(any(feature = "std", feature = "alloc"))]
#[derive(Default)]
pub struct CollectingVTActor {
    actions: Vec<VTAction>,
}

#[cfg(any(feature = "std", feature = "alloc"))]
impl IntoIterator for CollectingVTActor {
    type Item = VTAction;
    type IntoIter = alloc::vec::IntoIter<VTAction>;

    fn into_iter(self) -> Self::IntoIter {
        self.actions.into_iter()
    }
}

#[cfg(any(feature = "std", feature = "alloc"))]
impl CollectingVTActor {
    pub fn into_vec(self) -> Vec<VTAction> {
        self.actions
    }
}

#[cfg(any(feature = "std", feature = "alloc"))]
impl VTActor for CollectingVTActor {
    fn print(&mut self, b: char) {
        self.actions.push(VTAction::Print(b));
    }

    fn execute_c0_or_c1(&mut self, control: u8) {
        self.actions.push(VTAction::ExecuteC0orC1(control));
    }

    fn dcs_hook(
        &mut self,
        byte: u8,
        params: &[i64],
        intermediates: &[u8],
        ignored_excess_intermediates: bool,
    ) {
        self.actions.push(VTAction::DcsHook {
            byte,
            params: params.to_vec(),
            intermediates: intermediates.to_vec(),
            ignored_excess_intermediates,
        });
    }

    fn dcs_put(&mut self, byte: u8) {
        self.actions.push(VTAction::DcsPut(byte));
    }

    fn dcs_unhook(&mut self) {
        self.actions.push(VTAction::DcsUnhook);
    }

    fn esc_dispatch(
        &mut self,
        params: &[i64],
        intermediates: &[u8],
        ignored_excess_intermediates: bool,
        byte: u8,
    ) {
        self.actions.push(VTAction::EscDispatch {
            params: params.to_vec(),
            intermediates: intermediates.to_vec(),
            ignored_excess_intermediates,
            byte,
        });
    }

    fn csi_dispatch(&mut self, params: &[CsiParam], parameters_truncated: bool, byte: u8) {
        self.actions.push(VTAction::CsiDispatch {
            params: params.to_vec(),
            parameters_truncated,
            byte,
        });
    }

    fn osc_dispatch(&mut self, params: &[&[u8]]) {
        self.actions.push(VTAction::OscDispatch(
            params.iter().map(|i| i.to_vec()).collect(),
        ));
    }

    fn apc_dispatch(&mut self, data: Vec<u8>) {
        self.actions.push(VTAction::ApcDispatch(data));
    }
}

const MAX_INTERMEDIATES: usize = 2;
const MAX_OSC: usize = 64;
const MAX_PARAMS: usize = 256;

/// An APC sequence accumulates until its terminator arrives, so a producer
/// that never sends one would otherwise grow this buffer until the process
/// dies. The bound sits above any legal payload — the largest image the
/// terminal will keep is 100MB, roughly 133MB once base64 encoded — so it only
/// ever trips on a sequence that was never going to be usable.
#[cfg(any(feature = "std", feature = "alloc"))]
const MAX_APC: usize = 256 * 1024 * 1024;

struct OscState {
    #[cfg(any(feature = "std", feature = "alloc"))]
    buffer: Vec<u8>,
    #[cfg(not(any(feature = "std", feature = "alloc")))]
    buffer: heapless::Vec<u8, { MAX_OSC * 16 }>,
    param_indices: [usize; MAX_OSC],
    num_params: usize,
    full: bool,
}

impl OscState {
    fn put(&mut self, param: char) {
        if param == ';' {
            match self.num_params {
                MAX_OSC => {
                    self.full = true;
                }
                num => {
                    self.param_indices[num.saturating_sub(1)] = self.buffer.len();
                    self.num_params += 1;
                }
            }
        } else if !self.full {
            let mut buf = [0u8; 8];
            let extend_result = self
                .buffer
                .extend_from_slice(param.encode_utf8(&mut buf).as_bytes());

            #[cfg(all(not(feature = "std"), not(feature = "alloc")))]
            {
                if extend_result.is_err() {
                    self.full = true;
                    return;
                }
            }

            let _ = extend_result;

            if self.num_params == 0 {
                self.num_params = 1;
            }
        }
    }

    /// Consume the leading run of bytes in 0x20..=0x7f, which the state
    /// table maps to OscPut without leaving OscString, with the same effect
    /// as calling `put` once per byte. Returns the length of the run.
    fn put_ascii_run(&mut self, bytes: &[u8]) -> usize {
        let mut consumed = 0;
        loop {
            let rest = &bytes[consumed..];
            let text = rest
                .iter()
                .position(|&b| b == b';' || !(0x20..=0x7f).contains(&b))
                .unwrap_or(rest.len());
            self.put_text(&rest[..text]);
            consumed += text;
            if rest.get(text) != Some(&b';') {
                return consumed;
            }
            self.put(';');
            consumed += 1;
        }
    }

    #[cfg(any(feature = "std", feature = "alloc"))]
    fn put_text(&mut self, text: &[u8]) {
        // Appending to a Vec cannot fail, so `full` holds for the whole run.
        if !text.is_empty() && !self.full {
            self.buffer.extend_from_slice(text);
            if self.num_params == 0 {
                self.num_params = 1;
            }
        }
    }

    #[cfg(not(any(feature = "std", feature = "alloc")))]
    fn put_text(&mut self, text: &[u8]) {
        for &b in text {
            self.put(b as char);
        }
    }
}

/// The virtual terminal parser.  It works together with an implementation of `VTActor`.
pub struct VTParser {
    state: State,

    intermediates: [u8; MAX_INTERMEDIATES],
    num_intermediates: usize,
    ignored_excess_intermediates: bool,

    osc: OscState,

    params: [CsiParam; MAX_PARAMS],
    num_params: usize,
    current_param: Option<CsiParam>,
    params_full: bool,
    #[cfg(any(feature = "std", feature = "alloc"))]
    apc_data: Vec<u8>,
    /// The largest APC this parser will accumulate. Always `MAX_APC` outside
    /// of tests, which lower it so the limit can be exercised without moving
    /// a quarter of a gigabyte through the state machine.
    #[cfg(any(feature = "std", feature = "alloc"))]
    apc_limit: usize,
    /// Set once an APC sequence has outgrown `apc_limit`. Its bytes are then
    /// discarded rather than dispatched: a truncated APC is not a shorter
    /// command, it is a different one, and acting on it would be worse than
    /// ignoring it.
    #[cfg(any(feature = "std", feature = "alloc"))]
    apc_full: bool,

    utf8_parser: Utf8Parser,
    utf8_return_state: State,
}

/// Represents a parameter to a CSI-based escaped sequence.
///
/// CSI escapes typically have the form: `CSI 3 m`, but can also
/// bundle multiple values together: `CSI 3 ; 4 m`.  In both
/// of those examples the parameters are simple integer values
/// and latter of which would be expressed as a slice containing
/// `[CsiParam::Integer(3), CsiParam::Integer(4)]`.
///
/// There are some escape sequences that use colons to subdivide and
/// extend the meaning.  For example: `CSI 4:3 m` is a sequence used
/// to denote a curly underline.  That would be represented as:
/// `[CsiParam::ColonList(vec![Some(4), Some(3)])]`.
///
/// Later: reading ECMA 48, CSI is defined as:
/// CSI P ... P  I ... I  F
/// Where P are parameter bytes in the range 0x30-0x3F [0-9:;<=>?]
/// and I are intermediate bytes in the range 0x20-0x2F
/// and F is the final byte in the range 0x40-0x7E
///
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub enum CsiParam {
    Integer(i64),
    P(u8),
}

impl core::fmt::Debug for CsiParam {
    fn fmt(&self, fmt: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            Self::Integer(i) => write!(fmt, "Integer({i})"),
            Self::P(n) => write!(fmt, "P({})", char::from(*n)),
        }
    }
}

impl Default for CsiParam {
    fn default() -> Self {
        Self::Integer(0)
    }
}

impl CsiParam {
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Self::Integer(i) => Some(*i),
            _ => None,
        }
    }
}

impl core::fmt::Display for CsiParam {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            CsiParam::Integer(v) => {
                write!(f, "{}", v)?;
            }
            CsiParam::P(p) => {
                write!(f, "{}", *p as char)?;
            }
        }
        Ok(())
    }
}

impl VTParser {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let param_indices = [0usize; MAX_OSC];

        Self {
            state: State::Ground,
            utf8_return_state: State::Ground,

            intermediates: [0, 0],
            num_intermediates: 0,
            ignored_excess_intermediates: false,

            osc: OscState {
                buffer: Vec::new(),
                param_indices,
                num_params: 0,
                full: false,
            },

            params: [CsiParam::default(); MAX_PARAMS],
            num_params: 0,
            params_full: false,
            current_param: None,

            utf8_parser: Utf8Parser::new(),
            #[cfg(any(feature = "std", feature = "alloc"))]
            apc_data: Vec::new(),
            #[cfg(any(feature = "std", feature = "alloc"))]
            apc_limit: MAX_APC,
            #[cfg(any(feature = "std", feature = "alloc"))]
            apc_full: false,
        }
    }

    /// Returns if the state machine is in the ground state,
    /// i.e. there is no pending state held by the state machine.
    pub fn is_ground(&self) -> bool {
        self.state == State::Ground
    }

    fn as_integer_params(&self) -> [i64; MAX_PARAMS] {
        let mut res = [0i64; MAX_PARAMS];
        let mut i = 0;
        for src in &self.params[0..self.num_params] {
            if let CsiParam::Integer(value) = src {
                res[i] = *value;
            } else if let CsiParam::P(b';') = src {
                i += 1;
            }
        }
        res
    }

    fn finish_param(&mut self) {
        if let Some(val) = self.current_param.take() {
            if self.num_params < MAX_PARAMS {
                self.params[self.num_params] = val;
                self.num_params += 1;
            }
        }
    }

    /// Promote early intermediates to parameters.
    /// This is handle sequences such as DECSET that use `?`
    /// prior to other numeric parameters.
    /// `?` is technically in the intermediate range and shouldn't
    /// appear in the parameter position according to ECMA 48
    fn promote_intermediates_to_params(&mut self) {
        if self.num_intermediates > 0 {
            for &p in &self.intermediates[..self.num_intermediates] {
                if self.num_params >= MAX_PARAMS {
                    self.ignored_excess_intermediates = true;
                    break;
                }
                self.params[self.num_params] = CsiParam::P(p);
                self.num_params += 1;
            }
            self.num_intermediates = 0;
        }
    }

    fn action(&mut self, action: Action, param: u8, actor: &mut dyn VTActor) {
        match action {
            Action::None | Action::Ignore => {}
            Action::Print => actor.print(param as char),
            Action::Execute => actor.execute_c0_or_c1(param),
            Action::Clear => {
                self.num_intermediates = 0;
                self.ignored_excess_intermediates = false;
                self.osc.num_params = 0;
                self.osc.full = false;
                self.num_params = 0;
                self.params_full = false;
                self.current_param.take();
                #[cfg(any(feature = "std", feature = "alloc"))]
                {
                    self.apc_data.clear();
                    self.apc_data.shrink_to_fit();
                    self.apc_full = false;
                    self.osc.buffer.clear();
                    self.osc.buffer.shrink_to_fit();
                }
            }
            Action::Collect => {
                if self.num_intermediates < MAX_INTERMEDIATES {
                    self.intermediates[self.num_intermediates] = param;
                    self.num_intermediates += 1;
                } else {
                    self.ignored_excess_intermediates = true;
                }
            }
            Action::Param => {
                if self.params_full {
                    return;
                }

                self.promote_intermediates_to_params();

                match param {
                    b'0'..=b'9' => match self.current_param.take() {
                        Some(CsiParam::Integer(i)) => {
                            self.current_param.replace(CsiParam::Integer(
                                i.saturating_mul(10).saturating_add((param - b'0') as i64),
                            ));
                        }
                        Some(_) => unreachable!(),
                        None => {
                            self.current_param
                                .replace(CsiParam::Integer((param - b'0') as i64));
                        }
                    },
                    p => {
                        self.finish_param();

                        if self.num_params + 1 > MAX_PARAMS {
                            self.params_full = true;
                        } else {
                            self.params[self.num_params] = CsiParam::P(p);
                            self.num_params += 1;
                        }
                    }
                }
            }
            Action::Hook => {
                self.finish_param();
                actor.dcs_hook(
                    param,
                    &self.as_integer_params()[0..self.num_params],
                    &self.intermediates[0..self.num_intermediates],
                    self.ignored_excess_intermediates,
                );
            }
            Action::Put => actor.dcs_put(param),
            Action::EscDispatch => {
                self.finish_param();
                actor.esc_dispatch(
                    &self.as_integer_params()[0..self.num_params],
                    &self.intermediates[0..self.num_intermediates],
                    self.ignored_excess_intermediates,
                    param,
                );
            }
            Action::CsiDispatch => {
                self.finish_param();
                self.promote_intermediates_to_params();
                actor.csi_dispatch(
                    &self.params[0..self.num_params],
                    self.ignored_excess_intermediates,
                    param,
                );
            }
            Action::Unhook => actor.dcs_unhook(),
            Action::OscStart => {
                self.osc.buffer.clear();
                #[cfg(any(feature = "std", feature = "alloc"))]
                self.osc.buffer.shrink_to_fit();
                self.osc.num_params = 0;
                self.osc.full = false;
            }
            Action::OscPut => self.osc.put(param as char),

            Action::OscEnd => {
                if self.osc.num_params == 0 {
                    actor.osc_dispatch(&[]);
                } else {
                    let mut params: [&[u8]; MAX_OSC] = [b""; MAX_OSC];
                    let mut offset = 0usize;
                    let mut slice = self.osc.buffer.as_slice();
                    let limit = self.osc.num_params.min(MAX_OSC);
                    #[allow(clippy::needless_range_loop)]
                    for i in 0..limit - 1 {
                        let (a, b) = slice.split_at(self.osc.param_indices[i] - offset);
                        params[i] = a;
                        slice = b;
                        offset = self.osc.param_indices[i];
                    }
                    params[limit - 1] = slice;
                    actor.osc_dispatch(&params[0..limit]);
                }
            }

            Action::ApcStart => {
                #[cfg(any(feature = "std", feature = "alloc"))]
                {
                    self.apc_data.clear();
                    self.apc_data.shrink_to_fit();
                    self.apc_full = false;
                }
            }
            Action::ApcPut => {
                #[cfg(any(feature = "std", feature = "alloc"))]
                {
                    if self.apc_full {
                        // Abandoned; go on discarding until the sequence ends.
                    } else if self.apc_data.len() >= self.apc_limit {
                        self.apc_full = true;
                        // Assigning rather than clearing releases the capacity.
                        self.apc_data = Vec::new();
                    } else {
                        self.apc_data.push(param);
                    }
                }
            }
            Action::ApcEnd => {
                #[cfg(any(feature = "std", feature = "alloc"))]
                {
                    if self.apc_full {
                        self.apc_full = false;
                    } else {
                        actor.apc_dispatch(core::mem::take(&mut self.apc_data));
                    }
                }
            }

            Action::Utf8 => self.next_utf8(actor, param),
        }
    }

    // Process a utf-8 multi-byte sequence.
    // The state tables emit Action::Utf8 to initiate a multi-byte
    // sequence, and once we're in the utf-8 state we'll defer to
    // this method for each byte until the Decode struct is signalled
    // that we're done.
    // We use the REPLACEMENT_CHARACTER for invalid sequences.
    // We return to the ground state after each codepoint, successful
    // or otherwise.
    fn next_utf8(&mut self, actor: &mut dyn VTActor, byte: u8) {
        struct Decoder {
            codepoint: Option<char>,
        }

        impl utf8parse::Receiver for Decoder {
            fn codepoint(&mut self, c: char) {
                self.codepoint.replace(c);
            }

            fn invalid_sequence(&mut self) {
                self.codepoint(char::REPLACEMENT_CHARACTER);
            }
        }

        let mut decoder = Decoder { codepoint: None };

        self.utf8_parser.advance(&mut decoder, byte);
        if let Some(c) = decoder.codepoint {
            // Slightly gross special cases C1 controls that were
            // encoded as UTF-8 rather than emitted as raw 8-bit.
            // If the decoded value is in the byte range, and that
            // value would cause a state transition, then we process
            // that state transition rather than performing the default
            // string accumulation.
            if c as u32 <= 0xff {
                let byte = ((c as u32) & 0xff) as u8;

                let (action, state) = lookup(self.utf8_return_state, byte);
                if action == Action::Execute
                    || (state != self.utf8_return_state && state != State::Utf8Sequence)
                {
                    self.action(lookup_exit(self.utf8_return_state), 0, actor);
                    self.action(action, byte, actor);
                    self.action(lookup_entry(state), 0, actor);
                    self.utf8_return_state = self.state;
                    self.state = state;
                    return;
                }
            }

            match self.utf8_return_state {
                State::Ground => actor.print(c),
                State::OscString => self.osc.put(c),
                state => panic!("unreachable state {:?}", state),
            };
            self.state = self.utf8_return_state;
        }
    }

    /// Parse a single byte.  This may result in a call to one of the
    /// methods on the provided `actor`.
    #[inline(always)]
    pub fn parse_byte(&mut self, byte: u8, actor: &mut dyn VTActor) {
        // While in utf-8 parsing mode, co-opt the vt state
        // table and instead use the utf-8 state table from the
        // parser.  It will drop us back into the Ground state
        // after each recognized (or invalid) codepoint.
        if self.state == State::Utf8Sequence {
            self.next_utf8(actor, byte);
            return;
        }

        self.parse_non_utf8_byte(byte, actor);
    }

    #[inline(always)]
    fn parse_non_utf8_byte(&mut self, byte: u8, actor: &mut dyn VTActor) {
        let (action, state) = lookup(self.state, byte);

        if state != self.state {
            if state != State::Utf8Sequence {
                self.action(lookup_exit(self.state), 0, actor);
            }
            self.action(action, byte, actor);
            self.action(lookup_entry(state), byte, actor);
            self.utf8_return_state = self.state;
            self.state = state;
        } else {
            self.action(action, byte, actor);
        }
    }

    /// A complete `ESC [ params final` whose parameters are only digits, ';'
    /// and ':' (not starting with ':'), such as SGR and cursor motion.
    /// Applies what the state machine does for those bytes -- clearing on
    /// entry, one Param per parameter byte, then CsiDispatch -- without
    /// walking it byte by byte. Anything else, including a sequence that
    /// continues past `bytes`, returns None and is left to the state machine.
    /// Kept out of line so that the ASCII run loop in `parse` stays tight.
    #[inline(never)]
    fn parse_plain_csi(&mut self, bytes: &[u8], actor: &mut dyn VTActor) -> Option<usize> {
        let params = bytes.strip_prefix(b"\x1b[")?;
        let len = params
            .iter()
            .position(|b| !matches!(b, b'0'..=b'9' | b':' | b';'))?;
        let final_byte = params[len];
        // A leading ':' moves CsiEntry to CsiIgnore rather than dispatching.
        if !(0x40..=0x7e).contains(&final_byte) || params[0] == b':' {
            return None;
        }
        // Entering Escape and then CsiEntry clears twice; once is the same.
        self.action(Action::Clear, 0, actor);
        // Action::Param for each byte; after a clear there are no
        // intermediates to promote.
        let mut current: Option<i64> = None;
        for &b in &params[..len] {
            if self.params_full {
                continue;
            }
            if b.is_ascii_digit() {
                let digit = (b - b'0') as i64;
                current = Some(match current {
                    Some(value) => value.saturating_mul(10).saturating_add(digit),
                    None => digit,
                });
            } else {
                if let Some(value) = current.take() {
                    if self.num_params < MAX_PARAMS {
                        self.params[self.num_params] = CsiParam::Integer(value);
                        self.num_params += 1;
                    }
                }
                if self.num_params + 1 > MAX_PARAMS {
                    self.params_full = true;
                } else {
                    self.params[self.num_params] = CsiParam::P(b);
                    self.num_params += 1;
                }
            }
        }
        self.current_param = current.map(CsiParam::Integer);
        self.action(Action::CsiDispatch, final_byte, actor);
        self.utf8_return_state = if len > 0 {
            State::CsiParam
        } else {
            State::CsiEntry
        };
        Some(2 + len + 1)
    }

    /// Parse a sequence of bytes.  The sequence need not be complete.
    /// This may result in some number of calls to the methods on the
    /// provided `actor`.
    pub fn parse(&mut self, mut bytes: &[u8], actor: &mut dyn VTActor) {
        while !bytes.is_empty() {
            // Continuation bytes cannot begin a printable run. Keep their
            // original decoder path free of ASCII and payload-state checks.
            if self.state == State::Utf8Sequence {
                self.next_utf8(actor, bytes[0]);
                bytes = &bytes[1..];
                // Start a borrowed run only after the existing decoder has
                // returned to Ground. ASCII and control-only input retain
                // their original path without additional run-detection work.
                if self.is_ground() {
                    let text = printable_utf8_prefix(bytes);
                    if !text.is_empty() {
                        actor.print_utf8(text);
                        bytes = &bytes[text.len()..];
                    }
                }
                continue;
            }
            // Only Ground can bypass the state machine. In particular, ASCII
            // following an incomplete UTF-8 sequence must reach next_utf8.
            if self.is_ground() && (b' '..=b'~').contains(&bytes[0]) {
                let end = bytes
                    .iter()
                    .position(|b| !(b' '..=b'~').contains(b))
                    .unwrap_or(bytes.len());
                actor.print_ascii(core::str::from_utf8(&bytes[..end]).unwrap());
                bytes = &bytes[end..];
            } else if self.state == State::OscString && (0x20..=0x7f).contains(&bytes[0]) {
                // OSC text stays in OscString, so append it in runs. Other
                // bytes, including UTF-8 and terminators, take the byte path.
                let consumed = self.osc.put_ascii_run(bytes);
                bytes = &bytes[consumed..];
            } else {
                if bytes[0] == 0x1b && self.is_ground() {
                    if let Some(consumed) = self.parse_plain_csi(bytes, actor) {
                        bytes = &bytes[consumed..];
                        continue;
                    }
                }
                self.parse_non_utf8_byte(bytes[0], actor);
                bytes = &bytes[1..];
                if matches!(
                    self.state,
                    State::ApcString
                        | State::DcsPassthrough
                        | State::DcsIgnore
                        | State::SosPmString
                ) {
                    // Other string payloads (notably images) need the byte
                    // parser, so avoid an extra run-detection branch on every
                    // byte. Finish this input chunk with the original loop.
                    // Any text after the terminator still emits ordinary
                    // print actions; batching can resume on the next chunk.
                    for &byte in bytes {
                        self.parse_byte(byte, actor);
                    }
                    return;
                }
            }
        }
    }
}

/// Stop before controls, incomplete codepoints or invalid encodings so that
/// the existing state machine retains all recovery and C1 dispatch behavior.
/// Inspect each byte at most once, including on malformed input.
#[inline(never)]
fn printable_utf8_prefix(bytes: &[u8]) -> &str {
    let mut pos = 0;
    while pos < bytes.len() {
        let tail = &bytes[pos..];
        let continuation = |b: u8| (0x80..=0xbf).contains(&b);
        let len = match tail {
            [0x20..=0x7e, ..] => 1,
            // U+0080..U+009F are encoded C1 controls, not printable text.
            [0xc2, 0xa0..=0xbf, ..] => 2,
            [0xc3..=0xdf, b, ..] if continuation(*b) => 2,
            [a @ 0xe0..=0xef, b, c, ..]
                if continuation(*c)
                    && match a {
                        0xe0 => (0xa0..=0xbf).contains(b),
                        0xed => (0x80..=0x9f).contains(b),
                        _ => continuation(*b),
                    } =>
            {
                3
            }
            [a @ 0xf0..=0xf4, b, c, d, ..]
                if continuation(*c)
                    && continuation(*d)
                    && match a {
                        0xf0 => (0x90..=0xbf).contains(b),
                        0xf4 => (0x80..=0x8f).contains(b),
                        _ => continuation(*b),
                    } =>
            {
                4
            }
            _ => break,
        };
        pos += len;
    }
    let prefix = &bytes[..pos];
    debug_assert!(core::str::from_utf8(prefix).is_ok());
    // SAFETY: Each step above accepts a complete UTF-8 scalar, checking every
    // continuation byte and excluding overlong encodings, surrogate values and
    // values above U+10FFFF. `pos` only advances past such scalars; invalid or
    // incomplete sequences stop the scan. The borrowed bytes remain unchanged.
    unsafe { core::str::from_utf8_unchecked(prefix) }
}

#[cfg(test)]
mod test {
    use super::*;
    use k9::assert_equal as assert_eq;

    fn parse_as_vec(bytes: &[u8]) -> Vec<VTAction> {
        let mut parser = VTParser::new();
        let mut actor = CollectingVTActor::default();
        parser.parse(bytes, &mut actor);
        actor.into_vec()
    }

    #[test]
    fn printable_utf8_accepts_all_scalars_except_controls() {
        for code in 0..=0x10ffff {
            if let Some(c) = char::from_u32(code) {
                let mut buffer = [0; 4];
                let text = c.encode_utf8(&mut buffer);
                let expected = if c.is_control() { 0 } else { text.len() };
                assert_eq!(
                    printable_utf8_prefix(text.as_bytes()).len(),
                    expected,
                    "{code:x}"
                );
                for end in 0..text.len() {
                    assert!(printable_utf8_prefix(&text.as_bytes()[..end]).is_empty());
                }
            }
        }
    }

    #[test]
    fn printable_utf8_prefix_matches_standard_validator() {
        fn check(bytes: &[u8]) {
            let valid_len = match core::str::from_utf8(bytes) {
                Ok(text) => text.len(),
                Err(error) => error.valid_up_to(),
            };
            let valid = core::str::from_utf8(&bytes[..valid_len]).unwrap();
            let end = valid
                .char_indices()
                .find(|(_, c)| c.is_control())
                .map_or(valid.len(), |(index, _)| index);
            assert_eq!(printable_utf8_prefix(bytes), &valid[..end], "{bytes:x?}");
        }
        // All byte pairs cover short malformed encodings and every C1 control.
        for a in 0..=255u8 {
            for b in 0..=255u8 {
                check(&[a, b]);
            }
        }
        // Mutate each byte around every encoding boundary. Include a valid
        // leading run so the returned prefix must stop at the correct offset.
        for c in [
            '\u{80}', '\u{7ff}', '\u{800}', '\u{d7ff}', '\u{e000}',
            '\u{ffff}', '\u{10000}', '\u{10ffff}', '🙂',
        ] {
            let mut encoded = [0; 4];
            let text = c.encode_utf8(&mut encoded);
            for index in 0..text.len() {
                for byte in 0..=255u8 {
                    let mut bytes = "é中".as_bytes().to_vec();
                    let start = bytes.len();
                    bytes.extend_from_slice(text.as_bytes());
                    bytes[start + index] = byte;
                    bytes.extend_from_slice("x🙂".as_bytes());
                    for end in 0..=bytes.len() {
                        check(&bytes[..end]);
                    }
                }
            }
        }
    }

    #[test]
    fn utf8_runs_preserve_controls_invalid_input_and_streaming_states() {
        let prefixes: &[&[u8]] = &[
            b"",
            b"\x1b",
            b"\x1b[12;",
            b"\x1b]0;",
            b"\x1bP1;2q",
            b"\x1b_",
            b"\x1b^",
            b"\xc3",
            b"\xf0\x9f",
        ];
        let mut suffixes = vec![
            "中e\u{301}🙂\u{a0}é\u{10ffff}text".as_bytes().to_vec(),
            b"\xe0\x80\x80\xed\xa0\x80\xf0\x80\x80\x80\xf4\x90\x80\x80\xff".to_vec(),
        ];
        for c in 0x80..=0x9f {
            let mut suffix = vec![0xc2, c];
            suffix.extend_from_slice(b"31mtext\x1b\\\x07");
            suffixes.push(suffix);
        }
        for prefix in prefixes {
            for suffix in &suffixes {
                let mut input = prefix.to_vec();
                input.extend_from_slice("界🙂".as_bytes());
                input.extend_from_slice(suffix);
                input.extend_from_slice(b"\x1b\\\x1b[0mend");
                let mut scalar = VTParser::new();
                let mut expected = CollectingVTActor::default();
                for &b in &input {
                    scalar.parse_byte(b, &mut expected);
                }
                let expected = expected.into_vec();
                for split in 0..=input.len() {
                    let mut batched = VTParser::new();
                    let mut actual = CollectingVTActor::default();
                    batched.parse(&input[..split], &mut actual);
                    batched.parse(&input[split..], &mut actual);
                    assert_eq!(
                        actual.into_vec(),
                        expected,
                        "input={input:?}, split={split}"
                    );
                    assert_eq!(batched.is_ground(), scalar.is_ground());
                }
            }
        }
    }

    #[test]
    fn utf8_runs_match_byte_parser_for_every_byte_pair() {
        for a in 0..=255 {
            for b in 0..=255 {
                let mut input = "中🙂".as_bytes().to_vec();
                input.extend_from_slice(&[a, b]);
                input.extend_from_slice("é界\x1b\\tail".as_bytes());
                let mut scalar = VTParser::new();
                let mut expected = CollectingVTActor::default();
                for &byte in &input {
                    scalar.parse_byte(byte, &mut expected);
                }
                let mut batched = VTParser::new();
                let mut actual = CollectingVTActor::default();
                batched.parse(&input, &mut actual);
                assert_eq!(actual.into_vec(), expected.into_vec(), "a={a}, b={b}");
                assert_eq!(batched.is_ground(), scalar.is_ground());
            }
        }
    }

    #[test]
    fn ascii_runs_match_byte_parser_in_every_streaming_state() {
        let prefixes: &[&[u8]] = &[
            b"",
            b"\x1b",
            b"\x1b[",
            b"\x1b[12;",
            b"\x1b]0;",
            b"\x1bP1;2q",
            b"\x1b_",
            b"\x1b^",
            b"\xc3",
            b"\xf0\x9f",
        ];
        for prefix in prefixes {
            for byte in 0..=255 {
                let mut input = prefix.to_vec();
                input.push(byte);
                input.extend_from_slice(b"abc ~\x7f\x00\x1b\\e\xcc\x81\r\nend");
                let mut scalar = VTParser::new();
                let mut expected = CollectingVTActor::default();
                for &b in &input {
                    scalar.parse_byte(b, &mut expected);
                }
                let expected = expected.into_vec();
                for size in [1, 2, 7, 64] {
                    let mut batched = VTParser::new();
                    let mut actual = CollectingVTActor::default();
                    for chunk in input.chunks(size) {
                        batched.parse(chunk, &mut actual);
                    }
                    assert_eq!(
                        actual.into_vec(),
                        expected,
                        "prefix={prefix:?}, byte={byte}, chunk={size}"
                    );
                    assert_eq!(batched.is_ground(), scalar.is_ground());
                }
            }
        }
    }

    #[test]
    fn test_mixed() {
        assert_eq!(
            parse_as_vec(b"yo\x07\x1b[32mwoot\x1b[0mdone"),
            vec![
                VTAction::Print('y'),
                VTAction::Print('o'),
                VTAction::ExecuteC0orC1(0x07,),
                VTAction::CsiDispatch {
                    params: vec![CsiParam::Integer(32)],
                    parameters_truncated: false,
                    byte: b'm',
                },
                VTAction::Print('w',),
                VTAction::Print('o',),
                VTAction::Print('o',),
                VTAction::Print('t',),
                VTAction::CsiDispatch {
                    params: vec![CsiParam::Integer(0)],
                    parameters_truncated: false,
                    byte: b'm',
                },
                VTAction::Print('d',),
                VTAction::Print('o',),
                VTAction::Print('n',),
                VTAction::Print('e',),
            ]
        );
    }

    #[test]
    fn test_print() {
        assert_eq!(
            parse_as_vec(b"yo"),
            vec![VTAction::Print('y'), VTAction::Print('o')]
        );
    }

    #[test]
    fn test_osc_with_c1_st() {
        assert_eq!(
            parse_as_vec(b"\x1b]0;there\x9c"),
            vec![VTAction::OscDispatch(vec![
                b"0".to_vec(),
                b"there".to_vec()
            ])]
        );
    }

    #[test]
    fn test_osc_with_bel_st() {
        assert_eq!(
            parse_as_vec(b"\x1b]0;hello\x07"),
            vec![VTAction::OscDispatch(vec![
                b"0".to_vec(),
                b"hello".to_vec()
            ])]
        );
    }

    #[test]
    fn test_decset() {
        assert_eq!(
            parse_as_vec(b"\x1b[?1l"),
            vec![VTAction::CsiDispatch {
                params: vec![CsiParam::P(b'?'), CsiParam::Integer(1)],
                parameters_truncated: false,
                byte: b'l',
            },]
        );
    }

    #[test]
    fn test_osc_too_many_params() {
        let fields = (0..MAX_OSC + 2)
            .into_iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>();
        let input = format!("\x1b]{}\x07", fields.join(";"));
        let actions = parse_as_vec(input.as_bytes());
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            VTAction::OscDispatch(parsed_fields) => {
                let fields: Vec<_> = fields.into_iter().map(|s| s.as_bytes().to_vec()).collect();
                assert_eq!(parsed_fields.as_slice(), &fields[0..MAX_OSC]);
            }
            other => panic!("Expected OscDispatch but got {:?}", other),
        }
    }

    #[test]
    fn test_osc_with_no_params() {
        assert_eq!(
            parse_as_vec(b"\x1b]\x07"),
            vec![VTAction::OscDispatch(vec![])]
        );
    }

    #[test]
    fn test_osc_with_esc_sequence_st() {
        // This case isn't the same as the other OSC cases; even though
        // `ESC \` is the long form escape sequence for ST, the ESC on its
        // own breaks out of the OSC state and jumps into the ESC state,
        // and that leaves the `\` character to be dispatched there in
        // the calling application.
        assert_eq!(
            parse_as_vec(b"\x1b]woot\x1b\\"),
            vec![
                VTAction::OscDispatch(vec![b"woot".to_vec()]),
                VTAction::EscDispatch {
                    params: vec![],
                    intermediates: vec![],
                    ignored_excess_intermediates: false,
                    byte: b'\\'
                }
            ]
        );
    }

    #[test]
    fn test_fancy_underline() {
        assert_eq!(
            parse_as_vec(b"\x1b[4m"),
            vec![VTAction::CsiDispatch {
                params: vec![CsiParam::Integer(4)],
                parameters_truncated: false,
                byte: b'm'
            }]
        );

        assert_eq!(
            // This is the kitty curly underline sequence.
            parse_as_vec(b"\x1b[4:3m"),
            vec![VTAction::CsiDispatch {
                params: vec![
                    CsiParam::Integer(4),
                    CsiParam::P(b':'),
                    CsiParam::Integer(3)
                ],
                parameters_truncated: false,
                byte: b'm'
            }]
        );
    }

    #[test]
    fn test_colon_rgb() {
        assert_eq!(
            parse_as_vec(b"\x1b[38:2::128:64:192m"),
            vec![VTAction::CsiDispatch {
                params: vec![
                    CsiParam::Integer(38),
                    CsiParam::P(b':'),
                    CsiParam::Integer(2),
                    CsiParam::P(b':'),
                    CsiParam::P(b':'),
                    CsiParam::Integer(128),
                    CsiParam::P(b':'),
                    CsiParam::Integer(64),
                    CsiParam::P(b':'),
                    CsiParam::Integer(192),
                ],
                parameters_truncated: false,
                byte: b'm'
            }]
        );
    }

    #[test]
    fn test_csi_omitted_param() {
        assert_eq!(
            parse_as_vec(b"\x1b[;1m"),
            vec![VTAction::CsiDispatch {
                params: vec![CsiParam::P(b';'), CsiParam::Integer(1)],
                parameters_truncated: false,
                byte: b'm'
            }]
        );
    }

    #[test]
    fn test_csi_too_many_params() {
        // Due to the much higher CSI element limit,
        // we must construct this test differently.
        let mut input = "\x1b[0".to_string();
        let mut params = vec![CsiParam::default()];

        for n in 1..=127 {
            input.push_str(&format!(";{n}"));
            params.push(CsiParam::P(b';'));
            params.push(CsiParam::Integer(n));
        }
        input.push_str(";128");

        input.push('p');
        params.push(CsiParam::P(b';'));

        assert_eq!(
            parse_as_vec(input.as_bytes()),
            vec![VTAction::CsiDispatch {
                params: params,
                parameters_truncated: false,
                byte: b'p'
            }]
        );
    }

    #[test]
    fn test_csi_intermediates() {
        assert_eq!(
            parse_as_vec(b"\x1b[1 p"),
            vec![VTAction::CsiDispatch {
                params: vec![CsiParam::Integer(1), CsiParam::P(b' ')],
                parameters_truncated: false,
                byte: b'p'
            }]
        );
        assert_eq!(
            parse_as_vec(b"\x1b[1 !p"),
            vec![VTAction::CsiDispatch {
                params: vec![CsiParam::Integer(1), CsiParam::P(b' '), CsiParam::P(b'!')],
                parameters_truncated: false,
                byte: b'p'
            }]
        );
        assert_eq!(
            parse_as_vec(b"\x1b[1 !#p"),
            vec![VTAction::CsiDispatch {
                // Note that the `#` was discarded
                params: vec![CsiParam::Integer(1), CsiParam::P(b' '), CsiParam::P(b'!')],
                parameters_truncated: true,
                byte: b'p'
            }]
        );
    }

    #[test]
    fn osc_utf8() {
        assert_eq!(
            parse_as_vec("\x1b]\u{af}\x07".as_bytes()),
            vec![VTAction::OscDispatch(vec!["\u{af}".as_bytes().to_vec()])]
        );
    }

    #[test]
    fn osc_fedora_vte() {
        assert_eq!(
            parse_as_vec("\u{9d}777;preexec\u{9c}".as_bytes()),
            vec![VTAction::OscDispatch(vec![
                b"777".to_vec(),
                b"preexec".to_vec(),
            ])]
        );
    }

    #[test]
    fn print_utf8() {
        assert_eq!(
            parse_as_vec("\u{af}".as_bytes()),
            vec![VTAction::Print('\u{af}')]
        );
    }

    #[test]
    fn utf8_control() {
        assert_eq!(
            parse_as_vec("\u{8d}".as_bytes()),
            vec![VTAction::ExecuteC0orC1(0x8d)]
        );
    }

    #[test]
    fn tmux_control() {
        assert_eq!(
            parse_as_vec("\x1bP1000phello\x1b\\".as_bytes()),
            vec![
                VTAction::DcsHook {
                    byte: b'p',
                    params: vec![1000],
                    intermediates: vec![],
                    ignored_excess_intermediates: false,
                },
                VTAction::DcsPut(b'h'),
                VTAction::DcsPut(b'e'),
                VTAction::DcsPut(b'l'),
                VTAction::DcsPut(b'l'),
                VTAction::DcsPut(b'o'),
                VTAction::DcsUnhook,
                VTAction::EscDispatch {
                    params: vec![],
                    intermediates: vec![],
                    ignored_excess_intermediates: false,
                    byte: b'\\',
                }
            ]
        );
    }

    #[test]
    fn tmux_passthru() {
        // I'm not convinced that we *should* represent this tmux sequence
        // in this way, but it is how it currently maps.
        // It's worth noting that we see this as final byte `t` here, which
        // collides with decVT105G in https://vt100.net/emu/dcsseq_dec.html
        assert_eq!(
            parse_as_vec("\x1bPtmux;data\x1b\\".as_bytes()),
            vec![
                VTAction::DcsHook {
                    byte: b't',
                    params: vec![],
                    intermediates: vec![],
                    ignored_excess_intermediates: false,
                },
                VTAction::DcsPut(b'm'),
                VTAction::DcsPut(b'u'),
                VTAction::DcsPut(b'x'),
                VTAction::DcsPut(b';'),
                VTAction::DcsPut(b'd'),
                VTAction::DcsPut(b'a'),
                VTAction::DcsPut(b't'),
                VTAction::DcsPut(b'a'),
                VTAction::DcsUnhook,
                VTAction::EscDispatch {
                    params: vec![],
                    intermediates: vec![],
                    ignored_excess_intermediates: false,
                    byte: b'\\',
                }
            ]
        );
    }

    #[test]
    fn an_oversized_apc_is_dropped() {
        let mut parser = VTParser::new();
        parser.apc_limit = 8;
        let mut actor = CollectingVTActor::default();

        let mut input = Vec::new();
        input.extend_from_slice(b"\x1b_G");
        input.resize(64, b'a');
        input.extend_from_slice(b"\x1b\\");
        parser.parse(&input, &mut actor);

        assert!(
            !actor
                .into_vec()
                .iter()
                .any(|a| matches!(a, VTAction::ApcDispatch(_))),
            "an APC past the size limit must be dropped, not truncated and dispatched"
        );
    }

    #[test]
    fn an_apc_after_an_oversized_one_is_still_dispatched() {
        let mut parser = VTParser::new();
        parser.apc_limit = 8;
        let mut actor = CollectingVTActor::default();

        let mut input = Vec::new();
        input.extend_from_slice(b"\x1b_G");
        input.resize(64, b'a');
        input.extend_from_slice(b"\x1b\\");
        parser.parse(&input, &mut actor);
        parser.parse(b"\x1b_Gf=24;ok\x1b\\", &mut actor);

        assert!(
            actor
                .into_vec()
                .iter()
                .any(|a| matches!(a, VTAction::ApcDispatch(d) if d == b"Gf=24;ok")),
            "the limit must reset when the oversized sequence ends"
        );
    }

    #[test]
    fn kitty_img() {
        assert_eq!(
            parse_as_vec("\x1b_Gf=24,s=10,v=20;payload\x1b\\".as_bytes()),
            vec![
                VTAction::ApcDispatch(b"Gf=24,s=10,v=20;payload".to_vec()),
                VTAction::EscDispatch {
                    params: vec![],
                    intermediates: vec![],
                    ignored_excess_intermediates: false,
                    byte: b'\\',
                }
            ]
        );
    }

    #[test]
    fn sixel() {
        assert_eq!(
            parse_as_vec("\x1bPqhello\x1b\\".as_bytes()),
            vec![
                VTAction::DcsHook {
                    byte: b'q',
                    params: vec![],
                    intermediates: vec![],
                    ignored_excess_intermediates: false,
                },
                VTAction::DcsPut(b'h'),
                VTAction::DcsPut(b'e'),
                VTAction::DcsPut(b'l'),
                VTAction::DcsPut(b'l'),
                VTAction::DcsPut(b'o'),
                VTAction::DcsUnhook,
                VTAction::EscDispatch {
                    params: vec![],
                    intermediates: vec![],
                    ignored_excess_intermediates: false,
                    byte: b'\\',
                }
            ]
        );
    }

    /// Feed `input` to a parser in `chunks` and to another one byte at a
    /// time, checking actions and OSC state at every chunk boundary.
    fn assert_runs_match_byte_parser(input: &[u8], chunks: &[usize], what: &str) {
        let mut scalar = VTParser::new();
        let mut expected = CollectingVTActor::default();
        let mut batched = VTParser::new();
        let mut actual = CollectingVTActor::default();
        let mut offset = 0;
        let mut sizes = chunks.iter().cycle();
        while offset < input.len() {
            let end = offset
                .saturating_add(*sizes.next().unwrap())
                .min(input.len());
            batched.parse(&input[offset..end], &mut actual);
            for &b in &input[offset..end] {
                scalar.parse_byte(b, &mut expected);
            }
            offset = end;
            // core's assert only formats the message on failure.
            core::assert_eq!(actual.actions, expected.actions, "{what}, offset={offset}");
            core::assert_eq!(batched.state, scalar.state, "{what}, offset={offset}");
            // Only read inside a sequence and reset on entry; the ground UTF-8
            // runs already leave a different stale value outside one.
            if scalar.state == State::Utf8Sequence {
                core::assert_eq!(
                    batched.utf8_return_state,
                    scalar.utf8_return_state,
                    "{what}, offset={offset}"
                );
            }
            core::assert_eq!(
                batched.osc.buffer.as_slice(),
                scalar.osc.buffer.as_slice(),
                "{what}, offset={offset}"
            );
            core::assert_eq!(
                batched.osc.param_indices,
                scalar.osc.param_indices,
                "{what}, offset={offset}"
            );
            core::assert_eq!(
                batched.osc.num_params,
                scalar.osc.num_params,
                "{what}, offset={offset}"
            );
            core::assert_eq!(batched.osc.full, scalar.osc.full, "{what}, offset={offset}");
            // CSI accumulation, including slots left over from earlier ones.
            core::assert_eq!(batched.params, scalar.params, "{what}, offset={offset}");
            core::assert_eq!(
                batched.num_params,
                scalar.num_params,
                "{what}, offset={offset}"
            );
            core::assert_eq!(
                batched.params_full,
                scalar.params_full,
                "{what}, offset={offset}"
            );
            core::assert_eq!(
                batched.current_param,
                scalar.current_param,
                "{what}, offset={offset}"
            );
            core::assert_eq!(
                (batched.intermediates, batched.num_intermediates),
                (scalar.intermediates, scalar.num_intermediates),
                "{what}, offset={offset}"
            );
            core::assert_eq!(
                batched.ignored_excess_intermediates,
                scalar.ignored_excess_intermediates,
                "{what}, offset={offset}"
            );
        }
    }

    #[test]
    fn osc_runs_match_byte_parser() {
        let many_params = format!("\x1b]{}\x07", ";x".repeat(MAX_OSC + 3));
        let long_payload = format!("\x1b]6;{}\x07after", "a1".repeat(9000));
        let inputs: &[&[u8]] = &[
            b"\x1b]0;title\x07tail",
            b"\x1b];;a;;b;\x07",
            b"\x1b];lead\x1b\\",
            b"\x1b]8;id=1;http://x/\x1b\\link\x1b]8;;\x1b\\",
            b"\x1b]2;a\x7fb ~c\x9cafter",
            b"\x1b]2;\xe4\xb8\xad;\xc3\xa9x\xc2\x9cafter",
            b"\x1b]2;ab\ncd\x00ef\x07",
            b"\x9d0;c1 start\x07\xc2\x9d1;utf8 c1\x07",
            b"\x1b]0;cancelled\x18then\x1b]1;sub\x1adone\x07",
            b"\x1b]0;a\x1b[1mb\x1b]0;c\x1bP1q\x1b\\",
            many_params.as_bytes(),
            long_payload.as_bytes(),
        ];
        for (i, input) in inputs.iter().enumerate() {
            for size in [1, 2, 3, 5, 7, 64, 4096, usize::MAX] {
                assert_runs_match_byte_parser(input, &[size], &format!("input {i}, chunk {size}"));
            }
            assert_runs_match_byte_parser(input, &[1, 4, 2, 9, 3, 17], &format!("input {i}"));
        }
    }

    #[test]
    fn osc_runs_match_byte_parser_on_random_input() {
        // Deterministic xorshift so failures reproduce.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let alphabet: &[&[u8]] = &[
            b"a",
            b"Z",
            b"0",
            b"9",
            b" ",
            b"~",
            b";",
            b";;",
            b"\x7f",
            b"\x07",
            b"\x1b",
            b"\\",
            b"\x1b]",
            b"\x1b]0;",
            b"\x1b]8;;",
            b"\x9d",
            b"\x9c",
            b"\xc2\x9c",
            b"\xc2\x9d",
            b"\xe4\xb8\xad",
            b"\xf0\x9f\x99\x82",
            b"\xc3",
            b"\x80",
            b"\xff",
            b"\x00",
            b"\n",
            b"\x18",
            b"\x1a",
            b"\x1b[",
            b"\x1bP",
            b"\x1b_",
            b"\x1b^",
        ];
        for round in 0..2000 {
            let mut input = b"\x1b]".to_vec();
            for _ in 0..(next() % 200) {
                if next() % 4 == 0 {
                    input.extend_from_slice(alphabet[(next() % alphabet.len() as u64) as usize]);
                } else {
                    input.push(0x20 + (next() % 0x60) as u8);
                }
            }
            let chunks: Vec<usize> = (0..8).map(|_| 1 + (next() % 40) as usize).collect();
            assert_runs_match_byte_parser(&input, &chunks, &format!("round={round}"));
        }
    }

    #[test]
    fn plain_csi_matches_byte_parser() {
        let many = format!("\x1b[{}m", "1;".repeat(MAX_PARAMS + 5));
        let many_colons = format!("\x1b[{}m", "2:".repeat(MAX_PARAMS + 5));
        let huge = format!("\x1b[{}m", "9".repeat(40));
        let inputs: &[&[u8]] = &[
            b"\x1b[m\x1b[0m\x1b[;m\x1b[1;m\x1b[;1m",
            b"\x1b[38;2;1;2;3mX\x1b[38:2::1:2:3mY\x1b[4:3m",
            b"\x1b[12;34Hab\x1b[H\x1b[5A\x1b[2K\x1b[100b\x1b[@\x1b[~",
            b"\x1b[:1m\x1b[1:m\x1b[1;:2m",
            b"\x1b[?25h\x1b[>4;1m\x1b[=1c\x1b[<1;2;3M\x1b[1 q\x1b[1$p",
            b"\x1b[1\x072H\x1b[1\x7f2H\x1b[1\x182H\x1b[1\x1a2H",
            b"\x1b[1\x1b[2m\x1b[1;2\x1b]0;t\x07\x1b[3m",
            b"\x1b[1;2\x9b3m\xc2\x9b4m\x9b5m",
            b"\x1b[1;2\x80m\x1b[1;\xe4\xb8\xadm\x1b[1\x7e",
            b"\x1bP1;2q#0\x1b\\\x1b[1m\x1b_Ga=q\x1b\\\x1b[2m",
            b"\xe4\xb8\x1b[1mz\xc3\x1b[2m",
            many.as_bytes(),
            many_colons.as_bytes(),
            huge.as_bytes(),
        ];
        for (i, input) in inputs.iter().enumerate() {
            for size in [1, 2, 3, 5, 7, 64, usize::MAX] {
                assert_runs_match_byte_parser(input, &[size], &format!("input {i}, chunk {size}"));
            }
            assert_runs_match_byte_parser(input, &[1, 4, 2, 9, 3, 17], &format!("input {i}"));
        }
    }

    #[test]
    fn plain_csi_matches_byte_parser_on_random_input() {
        let mut seed = 0x0123_4567_89ab_cdefu64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let alphabet: &[&[u8]] = &[
            b"\x1b[",
            b"\x1b[",
            b"\x1b",
            b"[",
            b"0",
            b"1",
            b"9",
            b"12",
            b";",
            b":",
            b"?",
            b">",
            b" ",
            b"$",
            b"m",
            b"H",
            b"@",
            b"~",
            b"`",
            b"\x7f",
            b"\x07",
            b"\x18",
            b"\x1b]",
            b"\x9b",
            b"\xc2\x9b",
            b"\xe4\xb8\xad",
            b"a",
            b"\n",
        ];
        for round in 0..3000 {
            let mut input = vec![];
            for _ in 0..(next() % 60) {
                input.extend_from_slice(alphabet[(next() % alphabet.len() as u64) as usize]);
            }
            let chunks: Vec<usize> = (0..8).map(|_| 1 + (next() % 24) as usize).collect();
            assert_runs_match_byte_parser(&input, &chunks, &format!("round={round}"));
        }
    }

    #[test]
    fn test_ommitted_dcs_param() {
        assert_eq!(
            parse_as_vec("\x1bP;1q\x1b\\".as_bytes()),
            vec![
                VTAction::DcsHook {
                    byte: b'q',
                    params: vec![0, 1],
                    intermediates: vec![],
                    ignored_excess_intermediates: false,
                },
                VTAction::DcsUnhook,
                VTAction::EscDispatch {
                    params: vec![],
                    intermediates: vec![],
                    ignored_excess_intermediates: false,
                    byte: b'\\',
                }
            ]
        );
    }
}
