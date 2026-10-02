//! A congestion controller for a trusted LAN.
//!
//! quinn's default (Cubic) starts small and backs off on loss, which on a LAN only adds latency:
//! a keyframe burst would sit in the datagram queue (and get silently dropped) while the window
//! grows. Bitrate is controlled at the encoder instead, so here the window is simply large and
//! fixed.

use std::any::Any;
use std::sync::Arc;
use std::time::Instant;

use quinn::congestion::{Controller, ControllerFactory};

/// 32 MiB in flight is far more than a LAN can hold, i.e. effectively "never block".
const WINDOW: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct LanController;

impl Controller for LanController {
    fn on_congestion_event(&mut self, _now: Instant, _sent: Instant, _persistent: bool, _lost: u64) {}

    fn on_mtu_update(&mut self, _new_mtu: u16) {}

    fn window(&self) -> u64 {
        WINDOW
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        WINDOW
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[derive(Debug, Default)]
pub struct LanControllerFactory;

impl ControllerFactory for LanControllerFactory {
    fn build(self: Arc<Self>, _now: Instant, _current_mtu: u16) -> Box<dyn Controller> {
        Box::new(LanController)
    }
}
