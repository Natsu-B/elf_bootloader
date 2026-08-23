//! RSP frame parsing and assembly.

/// Events produced while parsing RSP frames.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RspFrameEvent {
    /// Byte should be ignored.
    Ignore,
    /// More bytes needed to complete the frame.
    NeedMore,
    /// Frame boundary detected; resync in progress.
    Resync,
    /// Ctrl-C interrupt received.
    CtrlC,
    /// Frame is complete and ready for processing.
    FrameComplete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RspFrameByteKind {
    None,
    Payload,
    Checksum,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum RspFrameState {
    #[default]
    Idle,
    InFrame,
    Checksum(u8),
}

/// RSP frame assembler state machine.
#[derive(Default)]
pub struct RspFrameAssembler {
    state: RspFrameState,
}

impl RspFrameAssembler {
    /// Creates a new frame assembler.
    pub const fn new() -> Self {
        Self {
            state: RspFrameState::Idle,
        }
    }

    /// Resets the assembler to idle state.
    pub fn reset(&mut self) {
        self.state = RspFrameState::Idle;
    }

    /// Pushes a byte and returns the resulting event.
    pub fn push(&mut self, byte: u8) -> RspFrameEvent {
        self.push_with_kind(byte).0
    }

    pub(crate) fn push_with_kind(&mut self, byte: u8) -> (RspFrameEvent, RspFrameByteKind) {
        match self.state {
            RspFrameState::Idle => match byte {
                b'$' => {
                    self.state = RspFrameState::InFrame;
                    (RspFrameEvent::NeedMore, RspFrameByteKind::None)
                }
                0x03 => (RspFrameEvent::CtrlC, RspFrameByteKind::None),
                _ => (RspFrameEvent::Ignore, RspFrameByteKind::None),
            },
            RspFrameState::InFrame => match byte {
                b'$' => (RspFrameEvent::Resync, RspFrameByteKind::None),
                b'#' => {
                    self.state = RspFrameState::Checksum(0);
                    (RspFrameEvent::NeedMore, RspFrameByteKind::None)
                }
                _ => (RspFrameEvent::NeedMore, RspFrameByteKind::Payload),
            },
            RspFrameState::Checksum(count) => {
                if byte == b'$' {
                    self.state = RspFrameState::InFrame;
                    return (RspFrameEvent::Resync, RspFrameByteKind::None);
                }
                if count == 0 {
                    self.state = RspFrameState::Checksum(1);
                    (RspFrameEvent::NeedMore, RspFrameByteKind::Checksum)
                } else {
                    self.state = RspFrameState::Idle;
                    (RspFrameEvent::FrameComplete, RspFrameByteKind::Checksum)
                }
            }
        }
    }
}
