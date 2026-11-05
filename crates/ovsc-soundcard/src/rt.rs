//! Real-time priority for the device callback threads.
//!
//! With a few periods of device buffering (cpal's ALSA backend uses two),
//! a callback that runs late by more than a period is an xrun. On macOS the
//! callbacks run on CoreAudio's real-time IO thread, and with cpal's
//! `realtime` feature WASAPI's threads join the MMCSS "Audio" class. cpal's
//! ALSA threads would only be promoted through rtkit, and only for `hw:`
//! devices (not the `plughw:` names the documentation recommends), so on
//! Linux and the BSDs the bridge raises each callback thread to `SCHED_FIFO`
//! itself, from its first callback.

/// `SCHED_FIFO` priority of the callback threads: above typical audio
/// applications and sound servers (PipeWire's data threads use 88), below
/// ovsc-core's network threads (99), whose deadlines are tighter and whose
/// work is far shorter.
#[cfg(all(unix, not(target_vendor = "apple")))]
const CALLBACK_PRIORITY: u8 = 90;

/// Whether the bridge raises the priority of callbacks of the device with
/// backend identifier `id` itself.
pub(crate) fn wanted(id: &str) -> bool {
    cfg!(all(unix, not(target_vendor = "apple"))) && eligible_pcm(id)
}

/// Whether an ALSA PCM may run its callbacks with real-time priority.
/// Devices served by a sound server (`pulse`, `pipewire`, `jack`, and
/// `default`, which usually routes to one) block on that server, so
/// priority would gain nothing and a busy-polling plugin would starve the
/// machine; `null` never blocks at all.
pub(crate) fn eligible_pcm(id: &str) -> bool {
    let kind = id.split(':').next().unwrap_or("").trim().to_ascii_lowercase();
    !matches!(kind.as_str(), "default" | "pulse" | "pipewire" | "jack" | "null")
}

/// Gives the calling thread `SCHED_FIFO` real-time priority. Returns
/// whether it worked (Linux needs `CAP_SYS_NICE`, e.g. root). One system
/// call, no allocation, no logging: callable from the first callback.
#[cfg(all(unix, not(target_vendor = "apple")))]
pub(crate) fn promote_current_thread() -> bool {
    use thread_priority::{
        RealtimeThreadSchedulePolicy, ThreadPriority, ThreadPriorityValue, ThreadSchedulePolicy,
        set_thread_priority_and_policy, thread_native_id,
    };
    let Ok(value) = ThreadPriorityValue::try_from(CALLBACK_PRIORITY) else { return false };
    let fifo = ThreadSchedulePolicy::Realtime(RealtimeThreadSchedulePolicy::Fifo);
    set_thread_priority_and_policy(thread_native_id(), ThreadPriority::Crossplatform(value), fifo)
        .is_ok()
}

/// Other platforms schedule audio callbacks themselves.
#[cfg(not(all(unix, not(target_vendor = "apple"))))]
pub(crate) fn promote_current_thread() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sound_servers_are_not_promoted() {
        for id in ["hw:CARD=Loopback,DEV=0", "plughw:CARD=Loopback,DEV=1", "front:CARD=PCH,DEV=0"] {
            assert!(eligible_pcm(id), "{id}");
        }
        for id in ["default", "pulse", "pipewire", "jack", "null", "Default:CARD=x", " pulse"] {
            assert!(!eligible_pcm(id), "{id}");
        }
    }

    #[test]
    #[cfg(all(unix, not(target_vendor = "apple")))]
    fn promotion_reports_what_happened() {
        std::thread::spawn(|| {
            let ok = promote_current_thread();
            let policy = thread_priority::thread_schedule_policy().unwrap();
            let fifo = thread_priority::ThreadSchedulePolicy::Realtime(
                thread_priority::RealtimeThreadSchedulePolicy::Fifo,
            );
            // Root (or CAP_SYS_NICE) gets real-time scheduling; others are
            // refused and say so.
            assert_eq!(ok, policy == fifo, "{policy:?}");
        })
        .join()
        .unwrap();
    }
}
