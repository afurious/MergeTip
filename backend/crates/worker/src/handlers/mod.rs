//! Job handlers, one per job kind. Engine-backed handlers are thin: decode the payload, call the
//! engine, and translate its error into retry or dead-letter.

mod engine;
mod noop;

pub use engine::{
    PayTipHandler, PostReplyHandler, ProcessEventHandler, ReconcilePaymentHandler, VerifyTipHandler,
};
pub use noop::NoopHandler;
