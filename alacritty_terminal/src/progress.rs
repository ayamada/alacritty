//! OSC 9;4 progress reporting.
//!
//! The ANSI parser (`vte`) does not support the ConEmu/Windows Terminal progress reporting
//! escape sequence `OSC 9 ; 4 ; state ; progress`, so this module extracts it from the PTY
//! byte stream before parsing. All other bytes, including other OSC sequences, are passed
//! through unmodified.

/// Maximum number of payload bytes captured for a single OSC sequence.
///
/// Longer sequences (e.g. OSC 52 clipboard content) are forwarded without parsing to avoid
/// unbounded buffering.
const OSC_CAPTURE_LIMIT: usize = 64;

const ESC: u8 = 0x1B;
const BEL: u8 = 0x07;

/// Progress reported by an application using `OSC 9;4`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// Hide the progress indicator.
    Hidden,
    /// Normal progress with a percentage.
    Normal(u8),
    /// Error state with an optional percentage.
    Error(Option<u8>),
    /// Indeterminate state without a known percentage.
    Indeterminate,
    /// Warning/paused state with an optional percentage.
    Warning(Option<u8>),
}

/// OSC terminator handling necessary to keep the parser in sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Terminator {
    /// `BEL`.
    Bel,
    /// `ESC` (start of `ST`).
    Esc,
    /// Any other byte terminating the sequence (`CAN`/`SUB`).
    Byte(u8),
}

/// Current state of the extraction state machine.
#[derive(Default)]
enum State {
    /// Regular terminal output, not inside an escape sequence.
    #[default]
    Ground,
    /// Saw `ESC`, but not yet the byte starting the escape sequence.
    ///
    /// `absorb_st` is set after an intercepted OSC terminated with `ESC`, so that a following
    /// `\\` (`ST`) is swallowed as well.
    Escape { absorb_st: bool },
    /// Inside an `OSC` sequence, capturing its payload.
    Osc,
    /// Inside an `OSC` sequence which is forwarded byte-by-byte.
    OscOverflow,
}

/// Extract `OSC 9;4` progress reports from a PTY byte stream.
#[derive(Default)]
pub struct OscProgressFilter {
    state: State,
    /// Payload of the currently captured `OSC` sequence (without `ESC ]`).
    osc: Vec<u8>,
    /// Bytes forwarded to the terminal parser during the current call.
    out: Vec<u8>,
    /// Progress events parsed during the current call.
    progress: Vec<Progress>,
}

impl OscProgressFilter {
    /// Extract progress reports from the given PTY bytes.
    ///
    /// Returns the bytes to pass on to the terminal parser and the progress events parsed from
    /// this chunk. The returned values are only valid until the next call to `filter`.
    pub fn filter(&mut self, bytes: &[u8]) -> (&[u8], &[Progress]) {
        self.out.clear();
        self.progress.clear();

        // Fast path for regular output, which never starts an escape sequence.
        if matches!(self.state, State::Ground) && !bytes.contains(&ESC) {
            self.out.extend_from_slice(bytes);
        } else {
            for &byte in bytes {
                self.advance(byte);
            }
        }

        (&self.out, &self.progress)
    }

    /// Advance the state machine by a single byte.
    fn advance(&mut self, byte: u8) {
        match self.state {
            State::Ground => {
                if byte == ESC {
                    self.state = State::Escape { absorb_st: false };
                } else {
                    self.out.push(byte);
                }
            },
            State::Escape { absorb_st } => match byte {
                b']' => self.start_osc(),
                // Swallow the `ST` of an intercepted OSC.
                b'\\' if absorb_st => self.state = State::Ground,
                ESC => {
                    self.out.push(ESC);
                    self.state = State::Escape { absorb_st: false };
                },
                _ => {
                    self.out.push(ESC);
                    self.out.push(byte);
                    self.state = State::Ground;
                },
            },
            State::Osc => match byte {
                BEL => self.finish_osc(Terminator::Bel),
                0x18 | 0x1A => self.finish_osc(Terminator::Byte(byte)),
                ESC => self.finish_osc(Terminator::Esc),
                // Bytes the parser ignores inside OSC strings.
                0x00..=0x06 | 0x08..=0x17 | 0x19 | 0x1C..=0x1F => (),
                _ if self.osc.len() < OSC_CAPTURE_LIMIT => self.osc.push(byte),
                _ => {
                    self.overflow_osc();
                    self.out.push(byte);
                },
            },
            State::OscOverflow => match byte {
                BEL | 0x18 | 0x1A => {
                    self.out.push(byte);
                    self.state = State::Ground;
                },
                ESC => {
                    self.out.push(ESC);
                    self.state = State::Ground;
                },
                _ => self.out.push(byte),
            },
        }
    }

    /// Start capturing an `OSC` sequence.
    fn start_osc(&mut self) {
        self.osc.clear();
        self.state = State::Osc;
    }

    /// Forward the captured `OSC` data and stop capturing.
    fn overflow_osc(&mut self) {
        self.out.push(ESC);
        self.out.push(b']');
        self.out.extend_from_slice(&self.osc);
        self.osc.clear();
        self.state = State::OscOverflow;
    }

    /// Finish the captured `OSC` sequence with the given terminator.
    fn finish_osc(&mut self, terminator: Terminator) {
        match parse_progress(&self.osc) {
            // Drop the intercepted progress sequence.
            Some(progress) => {
                self.progress.push(progress);
                self.state = match terminator {
                    Terminator::Esc => State::Escape { absorb_st: true },
                    Terminator::Bel => State::Ground,
                    Terminator::Byte(byte) => {
                        self.out.push(byte);
                        State::Ground
                    },
                };
            },
            // Forward unhandled sequences unmodified.
            None => {
                self.out.push(ESC);
                self.out.push(b']');
                self.out.extend_from_slice(&self.osc);
                match terminator {
                    Terminator::Bel => self.out.push(BEL),
                    Terminator::Esc => self.out.push(ESC),
                    Terminator::Byte(byte) => self.out.push(byte),
                }
                self.state = State::Ground;
            },
        }

        self.osc.clear();
    }
}

/// Parse the payload of an `OSC 9;4` sequence.
///
/// The payload is expected to have the form `9;4[;state[;value]]`.
fn parse_progress(payload: &[u8]) -> Option<Progress> {
    let mut params = payload.split(|&byte| byte == b';');
    if params.next() != Some(b"9") || params.next() != Some(b"4") {
        return None;
    }

    // `OSC 9;4` clears the progress indicator.
    let state = match params.next() {
        None => return Some(Progress::Hidden),
        Some(state) => state,
    };
    let value = params.next().map(parse_percent);
    if params.next().is_some() {
        return None;
    }

    match state {
        b"0" => Some(Progress::Hidden),
        b"1" => Some(Progress::Normal(value.flatten()?)),
        b"2" => Some(Progress::Error(optional_percent(value)?)),
        b"3" => Some(Progress::Indeterminate),
        b"4" => Some(Progress::Warning(optional_percent(value)?)),
        _ => None,
    }
}

/// Parse an optional progress value.
///
/// Returns `None` if the parameter was present but invalid.
fn optional_percent(value: Option<Option<u8>>) -> Option<Option<u8>> {
    match value {
        None => Some(None),
        Some(Some(value)) => Some(Some(value)),
        Some(None) => None,
    }
}

/// Parse a progress value between 0 and 100.
fn parse_percent(value: &[u8]) -> Option<u8> {
    if value.is_empty() || value.len() > 3 {
        return None;
    }

    let mut number = 0u16;
    for &byte in value {
        if !byte.is_ascii_digit() {
            return None;
        }
        number = number * 10 + u16::from(byte - b'0');
    }

    Some(number.min(100) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run chunks through a single filter, collecting forwarded bytes and progress events.
    fn run_chunks(chunks: &[&[u8]]) -> (Vec<u8>, Vec<Progress>) {
        let mut filter = OscProgressFilter::default();
        let mut output = Vec::new();
        let mut progress = Vec::new();

        for chunk in chunks {
            let (bytes, events) = filter.filter(chunk);
            output.extend_from_slice(bytes);
            progress.extend_from_slice(events);
        }

        (output, progress)
    }

    /// Run a single chunk through a filter.
    fn run(input: &[u8]) -> (Vec<u8>, Vec<Progress>) {
        run_chunks(&[input])
    }

    /// Run input through a filter split at every possible position.
    fn run_all_splits(input: &[u8]) -> (Vec<u8>, Vec<Progress>) {
        let expected = run_chunks(&[input]);

        for split in 1..input.len() {
            let result = run_chunks(&[&input[..split], &input[split..]]);
            assert_eq!(expected, result, "split at {split}");
        }

        let single: Vec<&[u8]> = input.iter().map(std::slice::from_ref).collect();
        let result = run_chunks(&single);
        assert_eq!(expected, result, "single byte splits");

        expected
    }

    #[test]
    fn progress_bel_terminated() {
        let (output, progress) = run(b"\x1b]9;4;1;50\x07");
        assert!(output.is_empty());
        assert_eq!(progress, [Progress::Normal(50)]);
    }

    #[test]
    fn progress_st_terminated() {
        let (output, progress) = run(b"\x1b]9;4;2;75\x1b\\");
        assert!(output.is_empty());
        assert_eq!(progress, [Progress::Error(Some(75))]);
    }

    #[test]
    fn progress_without_value() {
        let (output, progress) = run(b"\x1b]9;4;2\x07");
        assert!(output.is_empty());
        assert_eq!(progress, [Progress::Error(None)]);

        let (output, progress) = run(b"\x1b]9;4;4\x07");
        assert!(output.is_empty());
        assert_eq!(progress, [Progress::Warning(None)]);
    }

    #[test]
    fn progress_clear() {
        let (output, progress) = run(b"\x1b]9;4\x07");
        assert!(output.is_empty());
        assert_eq!(progress, [Progress::Hidden]);

        let (output, progress) = run(b"\x1b]9;4;0;0\x07");
        assert!(output.is_empty());
        assert_eq!(progress, [Progress::Hidden]);
    }

    #[test]
    fn progress_indeterminate() {
        let (output, progress) = run(b"\x1b]9;4;3\x07");
        assert!(output.is_empty());
        assert_eq!(progress, [Progress::Indeterminate]);
    }

    #[test]
    fn progress_value_clamped() {
        let (output, progress) = run(b"\x1b]9;4;1;150\x07");
        assert!(output.is_empty());
        assert_eq!(progress, [Progress::Normal(100)]);
    }

    #[test]
    fn progress_with_ignored_control_bytes() {
        let (output, progress) = run(b"\x1b]9;4;1;5\x000\x07");
        assert!(output.is_empty());
        assert_eq!(progress, [Progress::Normal(50)]);
    }

    #[test]
    fn progress_chunked() {
        let (output, progress) = run_all_splits(b"\x1b]9;4;1;50\x07");
        assert!(output.is_empty());
        assert_eq!(progress, [Progress::Normal(50)]);
    }

    #[test]
    fn progress_st_chunked() {
        let (output, progress) = run_all_splits(b"\x1b]9;4;3\x1b\\");
        assert!(output.is_empty());
        assert_eq!(progress, [Progress::Indeterminate]);
    }

    #[test]
    fn invalid_progress_is_forwarded() {
        for input in [
            &b"\x1b]9;4;9;50\x07"[..],
            b"\x1b]9;4;1;abc\x07",
            b"\x1b]9;4;1\x07",
            b"\x1b]9;4;1;50;25\x07",
            b"\x1b]9;4;\x07",
        ] {
            let (output, progress) = run(input);
            assert_eq!(output, input);
            assert!(progress.is_empty());
        }
    }

    #[test]
    fn other_sequences_are_forwarded() {
        // iTerm2 notification using the same OSC 9.
        let input = b"\x1b]9;hello\x07";
        let (output, progress) = run(input);
        assert_eq!(output, input);
        assert!(progress.is_empty());

        // Window title.
        let input = b"\x1b]0;title\x07";
        let (output, progress) = run(input);
        assert_eq!(output, input);
        assert!(progress.is_empty());
    }

    #[test]
    fn mixed_chunks_are_forwarded_in_order() {
        let input = b"A\x1b]9;4;1;50\x07B\x1b]0;title\x07C\x1b]9;4;0\x07D";
        let (output, progress) = run_all_splits(input);
        assert_eq!(output, b"AB\x1b]0;title\x07CD");
        assert_eq!(progress, [Progress::Normal(50), Progress::Hidden]);
    }

    #[test]
    fn progress_followed_by_escape() {
        let (output, progress) = run(b"\x1b]9;4;3\x1b[c");
        assert_eq!(output, b"\x1b[c");
        assert_eq!(progress, [Progress::Indeterminate]);
    }

    #[test]
    fn progress_can_terminated() {
        let (output, progress) = run(b"\x1b]9;4;1;50\x18");
        assert_eq!(output, b"\x18");
        assert_eq!(progress, [Progress::Normal(50)]);
    }

    #[test]
    fn long_osc_is_forwarded() {
        let mut input = b"\x1b]52;c;".to_vec();
        input.extend(std::iter::repeat_n(b'A', 4096));
        input.push(BEL);
        input.extend_from_slice(b"\x1b]9;4;1;10\x07");

        let (output, progress) = run(&input);
        assert_eq!(output, input[..input.len() - 11]);
        assert_eq!(progress, [Progress::Normal(10)]);
    }

    #[test]
    fn long_osc_split_is_forwarded() {
        let mut input = b"\x1b]52;c;".to_vec();
        input.extend(std::iter::repeat_n(b'A', 512));
        input.push(BEL);

        let (output, progress) = run_all_splits(&input);
        assert_eq!(output, input);
        assert!(progress.is_empty());
    }
}
