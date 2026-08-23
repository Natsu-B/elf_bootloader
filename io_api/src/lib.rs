//! Generic I/O abstraction layer for various device types.
//!
//! This crate provides trait definitions for:
//! - Block devices (storage)
//! - Serial I/O (UART)
//! - Ethernet interfaces
//! - Generic byte streams

#![no_std]

/// Block device I/O traits.
pub mod block_device;
/// Ethernet interface traits.
pub mod ethernet;
/// Serial I/O traits.
pub mod serial;
/// Generic byte stream traits.
pub mod stream;
