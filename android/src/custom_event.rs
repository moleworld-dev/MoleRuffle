//! Custom event type for Ruffle on Android

use ruffle_core::events::KeyDescriptor;

use crate::PlayerRunnable;

/// User-defined events.
pub enum RuffleEvent {
    /// Indicates that a task is ready to be polled.
    TaskPoll(PlayerRunnable),
    VirtualKeyEvent {
        down: bool,
        key_descriptor: KeyDescriptor,
    },
    RunContextMenuCallback(usize),
    ClearContextMenu,
    RequestContextMenu,
    /// MoleRuffle:显/隐系统自带软键盘(⌨ 按钮触发)。
    ToggleSoftKeyboard,
}
