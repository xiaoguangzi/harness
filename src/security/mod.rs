//! What limits the model's tool calls: the permission rules and modes that
//! decide which calls run, and the OS sandbox their commands run in.

pub mod permissions;
pub mod sandbox;
