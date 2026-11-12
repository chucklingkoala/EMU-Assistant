//! Keeps Windows from going to sleep while a long job runs.

/// While this guard is alive, Windows does not go to sleep on idle (the screen may still
/// turn off). Create it on the worker thread that does the work: the request belongs to
/// the thread that made it and ends with the guard.
pub struct KeepAwake;

impl KeepAwake {
    pub fn start() -> Self {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED};
            SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
        }
        KeepAwake
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS};
            SetThreadExecutionState(ES_CONTINUOUS);
        }
    }
}
