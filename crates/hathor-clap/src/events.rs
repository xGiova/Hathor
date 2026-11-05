//! The event lists a host hands to `clap.params.flush`, behind safe views.
//!
//! ## Raw pointers in this module, and why they are sound
//!
//! Both views borrow, and never own, event-list vtables that the host builds on
//! the stack or in its own memory for exactly one `flush` call. The CLAP rule is
//! blunt: every pointer inside `clap_process`/`flush` is valid for the duration
//! of the call and nothing may be kept past it — so these views are born inside
//! the trampoline, hand out borrowed values, and die with the call frame.
//! `get` returning a header pointer is checked against the event's own `size`
//! field before a wider read, and `type_`/`space_id` must match before a
//! `clap_event_param_value` is ever reinterpreted out of it.

use crate::sys::events::{
    clap_event_header, clap_event_param_value, clap_input_events, clap_output_events,
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_VALUE,
};

/// A borrowed read view over the host's pending parameter values.
pub struct InEvents<'a> {
    sys: &'a clap_input_events,
}

/// One accepted parameter value: the subset of `clap_event_param_value` the
/// default flush path needs (id + value + frame offset).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParamUpdate {
    /// Parameter id, as declared by [`crate::ParamInfo`].
    pub id: u32,
    /// The new value in the parameter's own range.
    pub value: f64,
    /// Frame offset in the block the value takes effect (0 = start of block).
    pub time: u32,
}

impl<'a> InEvents<'a> {
    /// Wraps the host list. Null means "no list", a legal host answer mapped to
    /// no view at all.
    pub(crate) fn from_raw(sys: *const clap_input_events) -> Option<Self> {
        // SAFETY: the host contract keeps this pointer (when non-null) valid for
        // exactly the duration of the flush call; the caller hands us a borrow
        // scoped to that call.
        unsafe { sys.as_ref() }.map(|sys| Self { sys })
    }

    /// Number of events in the list.
    pub fn len(&self) -> u32 {
        let Some(size) = self.sys.size else {
            return 0;
        };
        // SAFETY: vtable call on the borrowed list; no arguments beyond the list
        // itself, and the integer answer is the host's to keep honest.
        unsafe { size(self.sys) }
    }

    /// True when the host queued nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Extracts the `param_value` events in order; other event types, other
    /// spaces, short headers and misaligned payloads are skipped — never guessed
    /// about.
    pub fn param_values(&self) -> impl Iterator<Item = ParamUpdate> + '_ {
        let n = self.len();
        let get = self.sys.get;
        (0..n).filter_map(move |i| {
            let raw: *const clap_event_header = unsafe {
                // SAFETY: host vtable call on the borrowed list; answers null or a
                // pointer to the event at `i`, valid until the end of the flush.
                get?(self.sys, i)
            };
            let header = unsafe { raw.cast::<clap_event_header>().as_ref() }?;
            if header.space_id != CLAP_CORE_EVENT_SPACE_ID || header.type_ != CLAP_EVENT_PARAM_VALUE
            {
                return None;
            }
            const WIDE: usize = core::mem::size_of::<clap_event_param_value>();
            if (header.size as usize) < WIDE {
                // A short event is not the layout we may read; skip rather than
                // walk past it into whatever the host stored there.
                return None;
            }
            let wide: *const clap_event_param_value = raw.cast();
            if wide.align_offset(core::mem::align_of::<clap_event_param_value>()) != 0 {
                return None; // a host that misaligns events gets no crash from us
            }
            // SAFETY: size and type/space just checked say the header is the first
            // field of a `clap_event_param_value`; alignment was just verified, so
            // this reinterpret is in-bounds and sound for a plain read.
            let ev = unsafe { &*wide };
            Some(ParamUpdate {
                id: ev.param_id,
                value: ev.value,
                time: header.time,
            })
        })
    }
}

/// A borrowed write view over the host's outgoing event list.
pub struct OutEvents<'a> {
    sys: &'a mut clap_output_events,
}

impl<'a> OutEvents<'a> {
    /// Wraps the host list; null maps to no view (calls then report `false`).
    pub(crate) fn from_raw(sys: *mut clap_output_events) -> Option<Self> {
        // SAFETY: same contract as `InEvents`: valid exactly for the flush call;
        // the host vtable needs the mutable-ish handle because `try_push` takes
        // `*const` — we only hand out `&mut` so pushes serialize against each
        // other at compile time.
        unsafe { sys.as_mut() }.map(|sys| Self { sys })
    }

    /// Queues a `param_value` event (the plugin telling the host "this is my
    /// value now"): used by the gesture/echo paths in the plugin task.
    pub fn push_param_value(&mut self, id: u32, value: f64) -> bool {
        let event = clap_event_param_value {
            header: clap_event_header {
                size: core::mem::size_of::<clap_event_param_value>() as u32,
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_PARAM_VALUE,
                flags: 0,
            },
            param_id: id,
            cookie: core::ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value,
        };
        let Some(try_push) = self.sys.try_push else {
            return false;
        };
        unsafe {
            // SAFETY: `event` is a stack local that outlives this synchronous
            // call; the host must copy what it needs before returning (CLAP rule
            // — expected, NOT VERIFIED per host). `self.sys` is the borrowed
            // list, valid for the flush call.
            try_push(self.sys, &event.header)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn param(id: u32, value: f64) -> clap_event_param_value {
        clap_event_param_value {
            header: clap_event_header {
                size: core::mem::size_of::<clap_event_param_value>() as u32,
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_PARAM_VALUE,
                flags: 0,
            },
            param_id: id,
            cookie: core::ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value,
        }
    }

    // Test storage: events live in a static so the vtable can hand out pointers
    // that outlive the closure below; the list is read-only for these tests.
    static mut TABLE: Option<[clap_event_param_value; 3]> = None;

    fn fill_table() {
        let a = param(10, 0.25);
        let mut foreign = param(11, 9.0);
        foreign.header.space_id = 42; // not the core space: must be skipped
        let mut b = param(12, 0.5);
        b.header.time = 32;
        let table = &raw mut TABLE;
        unsafe {
            // SAFETY: test binary, single-threaded access under the table init
            // discipline below; `fill_table` is called once at test start before
            // any pointer is handed out.
            *table = Some([a, foreign, b]);
        }
    }

    unsafe extern "C" fn size_three(_l: *const clap_input_events) -> u32 {
        3
    }

    unsafe extern "C" fn get_mixed(
        _l: *const clap_input_events,
        index: u32,
    ) -> *const clap_event_header {
        // SAFETY: the table was filled before this vtable is ever called; the
        // returned pointer is into a `static mut` the test keeps alive.
        unsafe {
            let table = (*core::ptr::addr_of!(TABLE)).as_ref();
            match table.and_then(|t| t.get(index as usize)) {
                Some(ev) => core::ptr::from_ref(ev).cast(),
                None => core::ptr::null(),
            }
        }
    }

    #[test]
    fn null_lists_are_no_events_not_wrong_answers() {
        assert!(InEvents::from_raw(core::ptr::null()).is_none());
        assert!(OutEvents::from_raw(core::ptr::null_mut()).is_none());
    }

    #[test]
    fn param_values_skips_foreign_spaces_and_walks_in_order() {
        fill_table();
        let list = clap_input_events {
            ctx: core::ptr::null_mut(),
            size: Some(size_three),
            get: Some(get_mixed),
        };
        let view = InEvents::from_raw(&list).expect("non-null");
        let got: Vec<ParamUpdate> = view.param_values().collect();
        assert_eq!(
            got,
            vec![
                ParamUpdate {
                    id: 10,
                    value: 0.25,
                    time: 0
                },
                ParamUpdate {
                    id: 12,
                    value: 0.5,
                    time: 32
                },
            ],
            "the foreign-space event must not appear, and order must follow the list"
        );
    }

    #[test]
    fn missing_vtable_slots_read_as_empty_not_as_panic() {
        let list = clap_input_events {
            ctx: core::ptr::null_mut(),
            size: None,
            get: None,
        };
        let view = InEvents::from_raw(&list).expect("non-null struct");
        assert_eq!(view.len(), 0, "no size fn = no events");
        assert!(view.param_values().next().is_none());
    }

    #[test]
    fn push_goes_through_the_hosts_try_push_slot() {
        static SEEN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
        unsafe extern "C" fn record(
            _l: *const clap_output_events,
            event: *const clap_event_header,
        ) -> bool {
            // SAFETY: test-controlled synchronous call; `event` points at the
            // caller's stack local, valid right now.
            unsafe {
                let ev = &*(event.cast::<clap_event_param_value>());
                SEEN.store(ev.param_id, core::sync::atomic::Ordering::SeqCst);
            }
            true
        }
        let mut list = clap_output_events {
            ctx: core::ptr::null_mut(),
            try_push: Some(record),
        };
        {
            let mut out = OutEvents::from_raw(&mut list).expect("non-null");
            assert!(
                out.push_param_value(77, 1.5),
                "the push must reach the host"
            );
        }
        assert_eq!(
            SEEN.load(core::sync::atomic::Ordering::SeqCst),
            77,
            "and the id must survive the round trip"
        );
        list.try_push = None;
        let mut out = OutEvents::from_raw(&mut list).expect("non-null, no fn");
        assert!(
            !out.push_param_value(78, 1.0),
            "a host without try_push must get a clean false, not a crash"
        );
    }
}
