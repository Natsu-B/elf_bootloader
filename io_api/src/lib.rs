//! Generic I/O abstraction layer for various device types.
//!
//! This crate provides trait definitions for:
//! - Block devices (storage)
//! - Ethernet interfaces
//! - Generic byte streams

#![no_std]

/// Block device I/O traits.
pub mod block_device;
/// Ethernet interface traits.
pub mod ethernet;
/// Generic byte stream traits.
pub mod stream;
