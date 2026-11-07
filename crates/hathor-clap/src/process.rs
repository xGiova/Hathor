//! The block view of `clap_process`: where CLAP memory meets `hathor-dsp-core`.
//!
//! ## Layout argument (this is the load-bearing one)
//!
//! CLAP hands audio as a `clap_audio_buffer` per port: `data32` is an array of
//! `channel_count` pointers, and **each channel's frames are contiguous f32**
//! (`clap.audio-ports.h`: the host owns the arrays until `process` returns).
//! Channels need not be contiguous *with each other* — so the honest mapping to
//! `hathor_dsp_core::buffer::Block` is one single-channel block per channel:
//! `Block::from_slice(channel, Shape::new(1, frames))`, whose only requirement
//! (one flat `channels × frames` slice) is exactly the per-channel guarantee CLAP
//! makes. That is the "no copy" path, and the unit tests below prove writes
//! through the view land in the host's own arrays.
//!
//! Inputs are deliberately exposed as *read-only* slices: a `Block` is a
//! processing view, and handing out a mutable one over input memory would let a
//! caller hold two unique views of the same frames. The consumer pattern the
//! plugin task follows: copy input slices into the output [`Block`]s (the only
//! unavoidable copy is *into* the host's output arrays, and hosts can make even
//! that a no-op via in-place pairs — see [`ProcessView::out_aliases_in`]).
//!
//! Interleaved audio is NOT a CLAP wire format (`data32` is planar by contract);
//! a future interleaved host bridge would enter here as a conversion, deferred
//! deliberately — the wrapper maps planar and nothing else (task note:
//! `interleaved=false first, true path deferred with a doc note`).
//!
//! ## 64-bit hosts: the `data64` half of the wire format
//!
//! `clap_audio_buffer` is a two-pointer declaration: `data32` XOR `data64` —
//! exactly one of them is non-null and that IS the sample width the host
//! chose for the port (clap.h allows 64-bit doubles; whether a given host
//! uses them is verified in that host and nowhere else — NOT VERIFIED here).
//! The view therefore reads the POINTER PAIR, not a flag
//! ([`ProcessView::channel_format`]), hands f64 rows through parallel
//! accessors, and treats both-or-neither as a malformed port: `None`, and
//! the consumer refuses rather than guessing which array the host meant.
//! Width conversion stays a plugin decision, not a hidden wrapper copy: the
//! analyzer narrows f64 into its f32 scratch, the EQ runs f64 through
//! activate-sized f32 scratch — each labeled at its adapter.
//!
//! ## Raw pointers in this module, and why they are sound
//!
//! Every slice handed out is reborrowed from host-owned arrays that the CLAP
//! contract keeps valid — and only valid — for the duration of one `process`
//! call. `ProcessView<'a>` carries that call's borrow, so views cannot outlive
//! it. Nulls in `data32` and indices past a port's own `channel_count` are
//! rejected with `None`, never unwrapped. `constant_mask != 0` is exposed, not
//! interpreted: we process every frame regardless, which is correct for constant
//! channels, just not optimal (no host we have tested sends one — expected, NOT
//! VERIFIED).

use hathor_dsp_core::buffer::Block;
use hathor_dsp_core::{ProcessCtx, Shape, Transport};

use crate::sys::audio_buffer::clap_audio_buffer;
use crate::sys::events::{
    clap_event_transport, CLAP_TRANSPORT_HAS_TEMPO, CLAP_TRANSPORT_HAS_TIME_SIGNATURE,
    CLAP_TRANSPORT_IS_PLAYING,
};
use crate::sys::process::{
    clap_process, clap_process_status, CLAP_PROCESS_CONTINUE, CLAP_PROCESS_CONTINUE_IF_NOT_QUIET,
    CLAP_PROCESS_ERROR, CLAP_PROCESS_SLEEP, CLAP_PROCESS_TAIL,
};

/// What the plugin tells the host after one block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStatus {
    /// Failure: the host should stop this plugin.
    Error,
    /// Normal continuation.
    Continue,
    /// Continue, but quiet enough that a clever host could pause.
    ContinueIfNotQuiet,
    /// Tail is still audible (reverb decay); keep pumping.
    Tail,
    /// Nothing to do: silence, host may skip the audio output.
    Sleep,
}

impl ProcessStatus {
    pub(crate) fn to_sys(self) -> clap_process_status {
        match self {
            Self::Error => CLAP_PROCESS_ERROR,
            Self::Continue => CLAP_PROCESS_CONTINUE,
            Self::ContinueIfNotQuiet => CLAP_PROCESS_CONTINUE_IF_NOT_QUIET,
            Self::Tail => CLAP_PROCESS_TAIL,
            Self::Sleep => CLAP_PROCESS_SLEEP,
        }
    }
}

/// One block of the stream, as the host delivered it.
pub struct ProcessView<'a> {
    sys: &'a clap_process,
    sample_rate_hz: f64,
}

/// The sample width a port's buffer declares, read from the pointer pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    /// `data32` carries the rows: planar f32 frames per channel.
    F32,
    /// `data64` carries the rows: planar f64 frames per channel.
    F64,
}

/// A borrowed port array (inputs or outputs) with sane bounds.
struct Ports<'a> {
    base: *const clap_audio_buffer,
    count: usize,
    _marker: core::marker::PhantomData<&'a ()>,
}

impl<'a> Ports<'a> {
    fn new(base: *const clap_audio_buffer, count: u32) -> Self {
        Self {
            base,
            count: count as usize,
            _marker: core::marker::PhantomData,
        }
    }

    /// Borrow port `index`'s buffer struct, if the host declared it.
    fn port(&self, index: usize) -> Option<&'a clap_audio_buffer> {
        if index >= self.count || self.base.is_null() {
            return None;
        }
        // SAFETY: the CLAP contract keeps an array of `count` audio buffers valid
        // for the duration of the call; `index` was just bounds-checked against
        // the count the host itself declared.
        unsafe { self.base.add(index).as_ref() }
    }
}

impl<'a> ProcessView<'a> {
    /// Wraps the host's `clap_process` for the duration of the call.
    ///
    /// # Safety
    /// The caller vouches for everything the CLAP contract vouches for a live
    /// host: every raw field inside `sys` must describe a valid snapshot for
    /// `'a` — buffer row pointers non-null and matching their declared
    /// `channel_count`s, event lists well-formed or null, all of it alive and
    /// stable for `'a` (for a real host: the duration of the callback). Inside
    /// a host callback the wrapper discharges this from the vtable contract;
    /// a test harness calling this OUTSIDE one (the reason it is public)
    /// assumes the verification burden itself, exactly like the host would —
    /// a mistyped `channel_count` here is UB downstream, not a panic.
    pub const unsafe fn from_sys(sys: &'a clap_process, sample_rate_hz: f64) -> Self {
        Self {
            sys,
            sample_rate_hz,
        }
    }

    /// Frames in this block.
    pub fn frames(&self) -> usize {
        self.sys.frames_count as usize
    }

    /// Monotonic sample position of the block's first frame (CLAP steady time).
    pub fn steady_time(&self) -> i64 {
        self.sys.steady_time
    }

    /// The sample rate the host declared at `activate`.
    pub const fn sample_rate_hz(&self) -> f64 {
        self.sample_rate_hz
    }

    /// The transport, when the host attached one. The borrow is confined to this
    /// view, which is confined to the call.
    pub fn transport(&self) -> Option<&'a clap_event_transport> {
        if self.sys.transport.is_null() {
            return None;
        }
        // SAFETY: a non-null transport is the host's `clap_event_transport`,
        // valid for the whole call (CLAP rule; verified against our mock-host
        // proxy, NOT VERIFIED against real hosts yet).
        unsafe { self.sys.transport.cast::<clap_event_transport>().as_ref() }
    }

    /// Number of input ports the host filled for this call.
    pub fn input_count(&self) -> usize {
        self.ports(true).count
    }

    /// Number of output ports.
    pub fn output_count(&self) -> usize {
        self.ports(false).count
    }

    /// The host's own [`ProcessCtx`] for the core `Node` contract: sample rate,
    /// frames, position, transport — built here so consumers do not re-derive
    /// (and cannot disagree about) what one block means.
    pub fn ctx(&self) -> ProcessCtx {
        let position = u64::try_from(self.sys.steady_time).unwrap_or(0);
        ProcessCtx {
            sample_rate: self.sample_rate_hz.round().max(0.0) as u32,
            frames: self.frames(),
            sample_position: position,
            transport: self.transport_snapshot(),
        }
    }

    /// Maps the CLAP transport flags to the core [`Transport`]; missing fields
    /// become `None`/defaults honestly (no tempo advertised → `tempo_bpm: None`,
    /// time signature absent → 4/4, the core default).
    pub fn transport_snapshot(&self) -> Option<Transport> {
        let t = self.transport()?;
        let beats = if t.flags & CLAP_TRANSPORT_HAS_TIME_SIGNATURE != 0 {
            u32::from(t.tsig_num).max(1)
        } else {
            4
        };
        Some(Transport {
            playing: t.flags & CLAP_TRANSPORT_IS_PLAYING != 0,
            tempo_bpm: (t.flags & CLAP_TRANSPORT_HAS_TEMPO != 0 && t.tempo > 0.0)
                .then_some(t.tempo as f32),
            beats_per_bar: beats,
        })
    }

    /// Read view over one input channel: a borrowed slice of `frames` samples
    /// living in the host's array.
    pub fn input_channel(&self, port: usize, channel: usize) -> Option<&'a [f32]> {
        let p: *const f32 = self.channel_slice(port, channel, true)?;
        Some(unsafe {
            // SAFETY: `channel_slice` verified the pointer non-null and the
            // port's `channel_count` covers `channel`; CLAP keeps each channel
            // array contiguous for exactly `frames_count` f32 for the call.
            core::slice::from_raw_parts(p, self.frames())
        })
    }

    /// Mutable view over one output channel.
    pub fn output_channel(&mut self, port: usize, channel: usize) -> Option<&mut [f32]> {
        let frames = self.frames();
        let p = self.channel_slice(port, channel, false)?;
        Some(unsafe {
            // SAFETY: same per-channel contiguity guarantee; output pointers are
            // `*mut` by CLAP design. The returned borrow is tied to `&mut self`,
            // so the borrow checker hands out at most one at a time — no two
            // unique views of the same frames can exist.
            core::slice::from_raw_parts_mut(p, frames)
        })
    }

    /// A single-channel [`Block`] over an output channel: this is the object a
    /// core `Node` processes, straight onto the host's memory.
    pub fn output_block(&mut self, port: usize, channel: usize) -> Option<Block<'_>> {
        let frames = self.frames();
        let slice = self.output_channel(port, channel)?;
        Some(Block::from_slice(slice, Shape::new(1, frames)))
    }

    /// Declared channels of a port (either direction).
    pub fn port_channels(&self, port: usize, is_input: bool) -> Option<u32> {
        self.ports(is_input).port(port).map(|b| b.channel_count)
    }

    /// The `constant_mask` of a port: zero means "every frame is real"; non-zero
    /// is the CLAP "constant channel" hint. We expose it and still process all
    /// frames — correct for constant channels, just not optimal (no host we have
    /// tested sends non-zero — expected, NOT VERIFIED).
    pub fn constant_mask(&self, port: usize, is_input: bool) -> Option<u64> {
        self.ports(is_input).port(port).map(|b| b.constant_mask)
    }

    /// The `latency` field a host may pre-announce on a port.
    pub fn port_latency(&self, port: usize, is_input: bool) -> Option<u32> {
        self.ports(is_input).port(port).map(|b| b.latency)
    }

    /// The port's sample width: the XOR of its two row arrays. `None` means
    /// either no such port, or a malformed buffer (both non-null or both
    /// null) — consumers refuse rather than guess which array the host meant.
    pub fn channel_format(&self, port: usize, is_input: bool) -> Option<SampleFormat> {
        let buf = self.ports(is_input).port(port)?;
        match (buf.data32.is_null(), buf.data64.is_null()) {
            (false, true) => Some(SampleFormat::F32),
            (true, false) => Some(SampleFormat::F64),
            _ => None,
        }
    }

    /// Read view over one f64 input channel (the `data64` world; see
    /// [`Self::channel_format`]).
    pub fn input_channel_f64(&self, port: usize, channel: usize) -> Option<&'a [f64]> {
        let p: *const f64 = self.channel_slice_f64(port, channel, true)?;
        Some(unsafe {
            // SAFETY: `channel_slice_f64` verified the pointer non-null and
            // the port's `channel_count` covers `channel`; CLAP keeps each
            // channel array contiguous for exactly `frames_count` f64 for the
            // duration of the call.
            core::slice::from_raw_parts(p, self.frames())
        })
    }

    /// Mutable view over one f64 output channel.
    pub fn output_channel_f64(&mut self, port: usize, channel: usize) -> Option<&mut [f64]> {
        let frames = self.frames();
        let p = self.channel_slice_f64(port, channel, false)?;
        Some(unsafe {
            // SAFETY: same per-channel contiguity guarantee; output pointers
            // are `*mut` by CLAP design and the borrow is tied to `&mut self`,
            // so at most one unique view of the frames exists at a time.
            core::slice::from_raw_parts_mut(p, frames)
        })
    }

    /// True when input port 0 and output port 0 share channel memory (an
    /// in-place pair the host gave us): a consumer must then treat the output
    /// slice as already carrying the input instead of holding a read borrow and
    /// a write borrow of the same bytes at once. We compare the *channel
    /// pointers*, not the row-array pointers — hosts legitimately hand separate
    /// arrays that point at the same buffers (mock-host proxy verifies). Both
    /// widths are checked: a 64-bit host aliases `data64` rows, and a
    /// half-shared row (one channel aliased, the next not) is still an alias —
    /// consumers must never fabricate mixed in/out arrays in their mocks.
    pub fn out_aliases_in(&self) -> bool {
        match (self.ports(true).port(0), self.ports(false).port(0)) {
            (Some(i), Some(o)) => {
                let shared = core::cmp::min(i.channel_count, o.channel_count) as usize;
                // SAFETY: each row array is an array of its port's declared
                // `channel_count` pointers (CLAP port contract); `shared` is
                // the min of the two, so reads are in-bounds for whichever
                // arrays the null checks below let through.
                unsafe {
                    let f32_shared = !i.data32.is_null()
                        && !o.data32.is_null()
                        && (0..shared).any(|c| *i.data32.add(c) == *o.data32.add(c));
                    let f64_shared = !i.data64.is_null()
                        && !o.data64.is_null()
                        && (0..shared).any(|c| *i.data64.add(c) == *o.data64.add(c));
                    f32_shared || f64_shared
                }
            }
            _ => false,
        }
    }

    fn ports(&self, is_input: bool) -> Ports<'a> {
        if is_input {
            Ports::new(self.sys.audio_inputs, self.sys.audio_inputs_count)
        } else {
            Ports::new(
                self.sys.audio_outputs.cast_const(),
                self.sys.audio_outputs_count,
            )
        }
    }

    fn channel_slice(&self, port: usize, channel: usize, is_input: bool) -> Option<*mut f32> {
        let buf = self.ports(is_input).port(port)?;
        if buf.channel_count as usize <= channel || buf.data32.is_null() {
            return None;
        }
        // SAFETY: `data32` is an array of `channel_count` pointers by the CLAP
        // port contract; the index is bounds-checked against the port's own count.
        let p = unsafe { *buf.data32.add(channel) };
        (!p.is_null()).then_some(p)
    }

    fn channel_slice_f64(&self, port: usize, channel: usize, is_input: bool) -> Option<*mut f64> {
        let buf = self.ports(is_input).port(port)?;
        if buf.channel_count as usize <= channel || buf.data64.is_null() {
            return None;
        }
        // SAFETY: `data64` is an array of `channel_count` pointers by the CLAP
        // port contract; the index is bounds-checked against the port's own count.
        let p = unsafe { *buf.data64.add(channel) };
        (!p.is_null()).then_some(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Host-shaped storage: two planar channels, allocated apart so the mapping
    /// cannot accidentally depend on channel adjacency.
    struct FakeHost {
        in0: Vec<f32>,
        in1: Vec<f32>,
        out0: Vec<f32>,
        out1: Vec<f32>,
        in_ptrs: Vec<*mut f32>,
        out_ptrs: Vec<*mut f32>,
        in_buf: clap_audio_buffer,
        out_buf: clap_audio_buffer,
    }

    fn fake(frames: usize) -> FakeHost {
        let mut h = FakeHost {
            in0: (0..frames).map(|i| i as f32 * 0.01).collect(),
            in1: (0..frames).map(|i| -(i as f32) * 0.01).collect(),
            out0: vec![0.0; frames],
            out1: vec![0.0; frames],
            in_ptrs: Vec::new(),
            out_ptrs: Vec::new(),
            in_buf: clap_audio_buffer {
                data32: core::ptr::null_mut(),
                data64: core::ptr::null_mut(),
                channel_count: 2,
                latency: 0,
                constant_mask: 0,
            },
            out_buf: clap_audio_buffer {
                data32: core::ptr::null_mut(),
                data64: core::ptr::null_mut(),
                channel_count: 2,
                latency: 0,
                constant_mask: 0,
            },
        };
        h.in_ptrs = vec![h.in0.as_mut_ptr(), h.in1.as_mut_ptr()];
        h.out_ptrs = vec![h.out0.as_mut_ptr(), h.out1.as_mut_ptr()];
        h.in_buf.data32 = h.in_ptrs.as_mut_ptr();
        h.out_buf.data32 = h.out_ptrs.as_mut_ptr();
        h
    }

    /// The process struct lives in a leaked box so raw pointers inside the view
    /// have a stable backing for the test — the shape of one host call, minus the
    /// un-leak (reclaimed at the end of each test below).
    fn leaked_process(h: &mut FakeHost, frames: u32) -> *mut clap_process {
        Box::into_raw(Box::new(clap_process {
            steady_time: 0,
            frames_count: frames,
            transport: core::ptr::null(),
            audio_inputs: &raw const h.in_buf,
            audio_outputs: &raw mut h.out_buf,
            audio_inputs_count: 1,
            audio_outputs_count: 1,
            in_events: core::ptr::null(),
            out_events: core::ptr::null(),
        }))
    }

    /// SAFETY discipline for the reclaimer: only ever called with a pointer from
    /// `leaked_process`, exactly once, before `h`'s arrays drop.
    unsafe fn reclaim(p: *mut clap_process) {
        drop(unsafe { Box::from_raw(p) });
    }

    #[test]
    fn block_view_writes_land_in_the_hosts_own_arrays() {
        let frames = 64usize;
        let mut h = fake(frames);
        let proc = leaked_process(&mut h, frames as u32);
        // SAFETY: `proc` is the box we just leaked; the borrow lasts until reclaim.
        let mut view = unsafe { ProcessView::from_sys(&*proc, 48_000.0) };
        {
            let mut block = view.output_block(0, 0).expect("one-channel block");
            block.channel(0)[0] = 1.0;
            block.channel(0)[1] = -1.0;
        }
        assert_eq!(
            &h.out0[..2],
            &[1.0, -1.0],
            "no copy: the host array itself moved"
        );
        assert_eq!(view.frames(), frames);
        assert_eq!(view.input_count(), 1);
        assert_eq!(view.output_count(), 1);
        assert_eq!(view.port_channels(0, true), Some(2));
        // SAFETY: our own leak, reclaimed exactly once, while `h` is still alive.
        unsafe { reclaim(proc) };
    }

    #[test]
    fn input_slices_are_reads_and_out_of_range_answers_are_none() {
        let mut h = fake(32);
        h.in_ptrs[1] = core::ptr::null_mut(); // a port that lies: one null channel
        let proc = leaked_process(&mut h, 32);
        // SAFETY: test-owned leak; borrowed then reclaimed below.
        let view = unsafe { ProcessView::from_sys(&*proc, 48_000.0) };
        assert!(
            view.input_channel(0, 1).is_none(),
            "a null channel pointer reads as None, never as a slice"
        );
        assert!(view.input_channel(0, 5).is_none(), "out of range too");
        assert!(view.input_channel(7, 0).is_none(), "no such port");
        assert_eq!(
            view.input_channel(0, 0).map(|s| s[1]),
            Some(0.01),
            "the good channel reads host values"
        );
        // SAFETY: our own leak, reclaimed exactly once.
        unsafe { reclaim(proc) };
    }

    #[test]
    fn ctx_maps_the_block_for_the_core_contract() {
        let mut h = fake(64);
        let proc = leaked_process(&mut h, 64);
        // SAFETY: test-owned leak; borrowed then reclaimed below.
        let view = unsafe { ProcessView::from_sys(&*proc, 48_000.0) };
        let ctx = view.ctx();
        assert_eq!(ctx.sample_rate, 48_000);
        assert_eq!(ctx.frames, 64);
        assert!(ctx.transport.is_none(), "no transport = None, honestly");
        // SAFETY: our own leak, reclaimed exactly once.
        unsafe { reclaim(proc) };
    }

    #[test]
    fn transport_snapshot_maps_flags() {
        let mut h = fake(64);
        let transport = Box::leak(Box::new(clap_event_transport {
            header: crate::sys::events::clap_event_header {
                size: core::mem::size_of::<clap_event_transport>() as u32,
                time: 0,
                space_id: 0,
                type_: crate::sys::events::CLAP_EVENT_TRANSPORT,
                flags: 0,
            },
            flags: CLAP_TRANSPORT_IS_PLAYING
                | CLAP_TRANSPORT_HAS_TEMPO
                | CLAP_TRANSPORT_HAS_TIME_SIGNATURE,
            song_pos_beats: 1000,
            song_pos_seconds: 20,
            tempo: 120.0,
            tempo_inc: 0.0025,
            loop_start_beats: -1,
            loop_end_beats: -1,
            loop_start_seconds: -1,
            loop_end_seconds: -1,
            bar_start: 0,
            bar_number: 0,
            tsig_num: 3,
            tsig_denom: 4,
        }));
        let p = leaked_process(&mut h, 64);
        // SAFETY: our own leak; we patch the transport pointer before borrowing.
        unsafe { (*p).transport = core::ptr::from_ref(transport).cast() };
        // SAFETY: test-owned leak; borrowed then reclaimed below.
        let view = unsafe { ProcessView::from_sys(&*p, 48_000.0) };
        let t = view.transport_snapshot().expect("flags say it has data");
        assert!(t.playing);
        assert_eq!(t.tempo_bpm, Some(120.0));
        assert_eq!(t.beats_per_bar, 3);
        // SAFETY: reclaim both test leaks exactly once.
        unsafe { reclaim(p) };
        drop(unsafe { Box::from_raw(transport) });
    }

    #[test]
    fn in_place_pairs_are_reported() {
        let mut h = fake(16);
        // The host "gives" us the same buffer for in and out: one alias.
        h.out_ptrs = vec![h.in0.as_mut_ptr(), h.in1.as_mut_ptr()];
        h.out_buf.data32 = h.out_ptrs.as_mut_ptr();
        let proc = leaked_process(&mut h, 16);
        // SAFETY: test-owned leak; borrowed then reclaimed below.
        let view = unsafe { ProcessView::from_sys(&*proc, 48_000.0) };
        assert!(
            view.out_aliases_in(),
            "the consumer must learn in-place pairs instead of fighting UB"
        );
        // SAFETY: our own leak, reclaimed exactly once.
        unsafe { reclaim(proc) };
        // and the negative case: distinct buffers must NOT read as aliased
        let mut h2 = fake(16);
        let proc2 = leaked_process(&mut h2, 16);
        // SAFETY: second test-owned leak; borrowed then reclaimed below.
        let view2 = unsafe { ProcessView::from_sys(&*proc2, 48_000.0) };
        assert!(
            !view2.out_aliases_in(),
            "separate in/out arrays of separate buffers are not an alias"
        );
        // SAFETY: our own leak, reclaimed exactly once.
        unsafe { reclaim(proc2) };
    }

    /// The 64-bit sibling of [`FakeHost`]: same shape, `data64` rows only.
    struct FakeHost64 {
        in0: Vec<f64>,
        in1: Vec<f64>,
        out0: Vec<f64>,
        out1: Vec<f64>,
        in_ptrs: Vec<*mut f64>,
        out_ptrs: Vec<*mut f64>,
        in_buf: clap_audio_buffer,
        out_buf: clap_audio_buffer,
    }

    fn fake64(frames: usize) -> FakeHost64 {
        let mut h = FakeHost64 {
            in0: (0..frames).map(|i| i as f64 * 0.01).collect(),
            in1: (0..frames).map(|i| -(i as f64) * 0.01).collect(),
            out0: vec![0.0; frames],
            out1: vec![0.0; frames],
            in_ptrs: Vec::new(),
            out_ptrs: Vec::new(),
            in_buf: clap_audio_buffer {
                data32: core::ptr::null_mut(),
                data64: core::ptr::null_mut(),
                channel_count: 2,
                latency: 0,
                constant_mask: 0,
            },
            out_buf: clap_audio_buffer {
                data32: core::ptr::null_mut(),
                data64: core::ptr::null_mut(),
                channel_count: 2,
                latency: 0,
                constant_mask: 0,
            },
        };
        h.in_ptrs = vec![h.in0.as_mut_ptr(), h.in1.as_mut_ptr()];
        h.out_ptrs = vec![h.out0.as_mut_ptr(), h.out1.as_mut_ptr()];
        h.in_buf.data64 = h.in_ptrs.as_mut_ptr();
        h.out_buf.data64 = h.out_ptrs.as_mut_ptr();
        h
    }

    fn leaked_process64(h: &mut FakeHost64, frames: u32) -> *mut clap_process {
        Box::into_raw(Box::new(clap_process {
            steady_time: 0,
            frames_count: frames,
            transport: core::ptr::null(),
            audio_inputs: &raw const h.in_buf,
            audio_outputs: &raw mut h.out_buf,
            audio_inputs_count: 1,
            audio_outputs_count: 1,
            in_events: core::ptr::null(),
            out_events: core::ptr::null(),
        }))
    }

    #[test]
    fn f64_rows_land_in_the_hosts_own_arrays() {
        let frames = 64usize;
        let mut h = fake64(frames);
        let proc = leaked_process64(&mut h, frames as u32);
        // SAFETY: `proc` is the box we just leaked; the borrow lasts until reclaim.
        let mut view = unsafe { ProcessView::from_sys(&*proc, 48_000.0) };
        assert_eq!(view.input_channel_f64(0, 0).map(|s| s[2]), Some(0.02));
        assert!(
            view.input_channel(0, 0).is_none(),
            "a 64-bit port answers None to the f32 accessor, never a lie"
        );
        {
            let o1 = view.output_channel_f64(0, 1).expect("row");
            o1[3] = 2.5;
        }
        assert_eq!(h.out1[3], 2.5, "no copy: the host's own 64-bit array moved");
        assert!(view.output_channel_f64(0, 5).is_none(), "bounds still hold");
        // SAFETY: our own leak, reclaimed exactly once, while `h` is alive.
        unsafe { reclaim(proc) };
    }

    #[test]
    fn channel_format_is_the_xor_of_the_pointer_pair() {
        let mut h32 = fake(16);
        let mut h64 = fake64(16);
        // SAFETY: our own leaks; borrowed then reclaimed below.
        let p32 = leaked_process(&mut h32, 16);
        let p64 = leaked_process64(&mut h64, 16);
        let v32 = unsafe { ProcessView::from_sys(&*p32, 48_000.0) };
        let v64 = unsafe { ProcessView::from_sys(&*p64, 48_000.0) };
        assert_eq!(v32.channel_format(0, true), Some(SampleFormat::F32));
        assert_eq!(v32.channel_format(0, false), Some(SampleFormat::F32));
        assert_eq!(v64.channel_format(0, true), Some(SampleFormat::F64));
        assert_eq!(v64.input_channel_f64(0, 0).map(|s| s[1]), Some(0.01));
        assert_eq!(v64.channel_format(3, true), None, "no such port");
        // Both arrays non-null is a malformed buffer: None, and BOTH typed
        // accessors are refused downstream by the consumers' format check.
        let mut both = fake64(16);
        let mut d32 = vec![core::ptr::null_mut::<f32>(); 2];
        d32[0] = core::ptr::from_mut(&mut both.in0[0]).cast();
        both.in_buf.data32 = d32.as_mut_ptr();
        let pboth = leaked_process64(&mut both, 16);
        // SAFETY: our own leak.
        let vboth = unsafe { ProcessView::from_sys(&*pboth, 48_000.0) };
        assert_eq!(
            vboth.channel_format(0, true),
            None,
            "XOR violated: refuse to guess"
        );
        // SAFETY: reclaim each leak exactly once.
        unsafe {
            reclaim(p32);
            reclaim(p64);
            reclaim(pboth);
        }
    }

    #[test]
    fn out_aliases_in_spots_the_64_bit_pair() {
        // A 64-bit in-place pair: same row pointers, f64 arrays.
        let mut h = fake64(16);
        h.out_ptrs = vec![h.in0.as_mut_ptr(), h.in1.as_mut_ptr()];
        h.out_buf.data64 = h.out_ptrs.as_mut_ptr();
        let proc = leaked_process64(&mut h, 16);
        // SAFETY: test-owned leak; borrowed then reclaimed below.
        let view = unsafe { ProcessView::from_sys(&*proc, 48_000.0) };
        assert!(
            view.out_aliases_in(),
            "64-bit in-place pairs are aliases too"
        );
        // SAFETY: our own leak, reclaimed exactly once.
        unsafe { reclaim(proc) };
        // Partial sharing: only row 0 aliases — STILL an alias (the pairwise
        // law: consumers must never fabricate half-shared mock arrays).
        let mut p = fake64(16);
        let i0 = p.in0.as_mut_ptr();
        let o1 = p.out1.as_mut_ptr();
        p.out_ptrs = vec![i0, o1];
        p.out_buf.data64 = p.out_ptrs.as_mut_ptr();
        let proc2 = leaked_process64(&mut p, 16);
        // SAFETY: second test-owned leak; borrowed then reclaimed below.
        let view2 = unsafe { ProcessView::from_sys(&*proc2, 48_000.0) };
        assert!(
            view2.out_aliases_in(),
            "one shared row of two is still an alias"
        );
        // SAFETY: our own leak, reclaimed exactly once.
        unsafe { reclaim(proc2) };
        // Negative control: distinct arrays, no shared rows in either width.
        let mut h2 = fake64(16);
        let proc3 = leaked_process64(&mut h2, 16);
        // SAFETY: third test-owned leak; borrowed then reclaimed below.
        let view3 = unsafe { ProcessView::from_sys(&*proc3, 48_000.0) };
        assert!(!view3.out_aliases_in(), "separate rows are not an alias");
        // SAFETY: our own leak, reclaimed exactly once.
        unsafe { reclaim(proc3) };
    }
}
