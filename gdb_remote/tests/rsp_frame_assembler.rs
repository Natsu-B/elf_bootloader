use gdb_remote::RspFrameAssembler;
use gdb_remote::RspFrameEvent;

#[test]
fn rsp_frame_completes() {
    let mut assembler = RspFrameAssembler::new();
    let data = b"$qSupported#cc";
    let mut last = RspFrameEvent::Ignore;
    for &byte in data {
        last = assembler.push(byte);
    }
    assert_eq!(last, RspFrameEvent::FrameComplete);
}

#[test]
fn rsp_frame_partial_needs_more() {
    let mut assembler = RspFrameAssembler::new();
    let data = b"$qSup";
    let mut saw_complete = false;
    let mut last = RspFrameEvent::Ignore;
    for &byte in data {
        let event = assembler.push(byte);
        saw_complete |= matches!(event, RspFrameEvent::FrameComplete | RspFrameEvent::CtrlC);
        last = event;
    }
    assert!(!saw_complete);
    assert_eq!(last, RspFrameEvent::NeedMore);
}

#[test]
fn rsp_frame_resync_on_dollar() {
    let mut assembler = RspFrameAssembler::new();
    let data = b"$q$Supported#cc";
    let mut saw_resync = false;
    for &byte in data {
        if assembler.push(byte) == RspFrameEvent::Resync {
            saw_resync = true;
        }
    }
    assert!(saw_resync);
}

#[test]
fn rsp_frame_ctrl_c() {
    let mut assembler = RspFrameAssembler::default();
    let event = assembler.push(0x03);
    assert_eq!(event, RspFrameEvent::CtrlC);
}
