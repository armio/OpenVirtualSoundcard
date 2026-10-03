//! The IO trace (design section 6.6): the first 64 IO operations of every
//! IO session, written into the region for the daemon to print once. It
//! answers, on every CI run, where the HAL puts its input and output times.
//!
//! A session is traced into the region attached when it starts. A region
//! attached later in the session (IO started before the first welcome, or
//! the daemon restarted) gets a session of its own at its first operation,
//! so its daemon prints that region's first 64 operations.

use std::sync::atomic::{AtomicU64, Ordering};

use ovsc_shm::layout::IO_TRACE_ENTRIES;

use super::Attachment;
use crate::abi::IOCycleInfo;

/// The trace writer's state.
pub(crate) struct Trace {
    /// Entries this session has claimed.
    next: AtomicU64,
    /// The daemon generation of the region this session is traced into; 0
    /// before the first.
    generation: AtomicU64,
}

/// One operation to trace.
pub(crate) struct TraceOp<'a> {
    pub op: u32,
    pub stream: u32,
    pub frames: u32,
    pub cycle: &'a IOCycleInfo,
    /// Host ticks at the end of the operation.
    pub done_ticks: u64,
}

impl Trace {
    pub(crate) const fn new() -> Self {
        Self { next: AtomicU64::new(0), generation: AtomicU64::new(0) }
    }

    /// Starts a session (StartIO 0 to 1) in `a`, the current attachment.
    /// Without one, the session starts in the first region it records into.
    pub(crate) fn start_session(&self, a: Option<&Attachment>) {
        match a {
            Some(a) => self.begin(a),
            None => {
                self.generation.store(0, Ordering::Relaxed);
                self.next.store(0, Ordering::Relaxed);
            }
        }
    }

    /// Starts the session in `a`: entries start again from 0, and the
    /// region's session number moves on so the daemon prints it.
    fn begin(&self, a: &Attachment) {
        self.generation.store(a.generation, Ordering::Relaxed);
        self.next.store(0, Ordering::Relaxed);
        let (header, _) = a.view.io_trace();
        header.next.store(0, Ordering::Relaxed);
        header.session.fetch_add(1, Ordering::Release);
    }

    /// Records `t` into `a` if this session has traced fewer than 64
    /// operations there. Real-time safe.
    pub(crate) fn record(&self, a: &Attachment, t: &TraceOp<'_>) {
        if self.generation.load(Ordering::Relaxed) != a.generation {
            self.begin(a);
        }
        if self.next.load(Ordering::Relaxed) >= IO_TRACE_ENTRIES as u64 {
            return;
        }
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let (header, entries) = a.view.io_trace();
        let Some(e) = entries.get(n as usize) else {
            return;
        };
        let c = t.cycle;
        e.cycle_counter.store(c.mIOCycleCounter, Ordering::Relaxed);
        e.op_stream.store(u64::from(t.op) | u64::from(t.stream) << 32, Ordering::Relaxed);
        let nominal = u64::from(c.mNominalIOBufferFrameSize);
        e.frames.store(u64::from(t.frames) | nominal << 32, Ordering::Relaxed);
        e.current_sample.store(c.mCurrentTime.mSampleTime.to_bits(), Ordering::Relaxed);
        e.current_host_ticks.store(c.mCurrentTime.mHostTime, Ordering::Relaxed);
        e.input_sample.store(c.mInputTime.mSampleTime.to_bits(), Ordering::Relaxed);
        e.output_sample.store(c.mOutputTime.mSampleTime.to_bits(), Ordering::Relaxed);
        e.done_host_ticks.store(t.done_ticks, Ordering::Relaxed);
        // Publishes the entry: the daemon reads `next` first.
        header.next.fetch_max(n.saturating_add(1), Ordering::Release);
    }
}
