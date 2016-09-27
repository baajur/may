extern crate kernel32;

use std::{cmp, io, ptr, u32};
use std::cell::UnsafeCell;
use std::sync::atomic::Ordering;
use std::os::windows::io::AsRawSocket;
use smallvec::SmallVec;
use super::winapi::*;
use super::miow::Overlapped;
use super::miow::iocp::{CompletionPort, CompletionStatus};
use sync::BoxedOption;
use scheduler::Scheduler;
use coroutine::CoroutineImpl;
use yield_now::set_co_para;
use timeout_list::{TimeoutHandle, ns_to_ms};

type TimerHandle = TimeoutHandle<TimerData>;

// the timeout data
pub struct TimerData {
    event_data: *mut EventData,
}

// event associated io data, must be construct in the coroutine
// this passed in to the _overlapped verion API and will read back
// when IOCP get an io event. the timer handle is used to remove
// from the timeout list and co will be pushed to the event_list
// for scheduler
#[repr(C)]
pub struct EventData {
    overlapped: UnsafeCell<Overlapped>,
    handle: HANDLE,
    pub timer: Option<TimerHandle>,
    pub co: BoxedOption<CoroutineImpl>,
}

impl EventData {
    pub fn new(handle: HANDLE) -> EventData {
        EventData {
            overlapped: UnsafeCell::new(Overlapped::zero()),
            handle: handle,
            timer: None,
            co: BoxedOption::none(),
        }
    }

    #[inline]
    pub fn get_overlapped(&self) -> &mut Overlapped {
        unsafe { &mut *self.overlapped.get() }
    }

    pub fn timer_data(&self) -> TimerData {
        TimerData { event_data: self as *const _ as *mut _ }
    }

    pub fn get_io_size(&self) -> usize {
        let ol = unsafe { &*self.get_overlapped().raw() };
        ol.InternalHigh as usize
    }
}

// buffer to receive the system events
pub type EventsBuf = SmallVec<[CompletionStatus; 1024]>;

pub struct Selector {
    /// The actual completion port that's used to manage all I/O
    port: CompletionPort,
}

impl Selector {
    pub fn new() -> io::Result<Selector> {
        CompletionPort::new(1).map(|cp| Selector { port: cp })
    }

    pub fn select(&self,
                  s: &Scheduler,
                  events: &mut EventsBuf,
                  timeout: Option<u64>)
                  -> io::Result<()> {
        let timeout = timeout.map(|to| cmp::min(ns_to_ms(to), u32::MAX as u64) as u32);
        info!("select; timeout={:?}", timeout);
        info!("polling IOCP");
        let n = match self.port.get_many(events, timeout) {
            Ok(statuses) => statuses.len(),
            Err(ref e) if e.raw_os_error() == Some(WAIT_TIMEOUT as i32) => 0,
            Err(e) => return Err(e),
        };

        for status in events[..n].iter() {
            // need to check the status for each io
            let overlapped = status.overlapped();
            if overlapped.is_null() {
                // this is just a wakeup event, ignore it
                continue;
            }
            let data = unsafe { &mut *(overlapped as *mut EventData) };
            let overlapped = unsafe { &*(*overlapped).raw() };
            info!("select got overlapped, stuats = {}", overlapped.Internal);

            let co = data.co.take_fast(Ordering::Relaxed);
            if co.is_none() {
                // there is no coroutine prepared, just ignore this one
                error!("can't get coroutine in the iocp select");
                continue;
            }
            let mut co = co.unwrap();

            const STATUS_CANCELLED_U32: u32 = STATUS_CANCELLED as u32;
            // check the status
            match overlapped.Internal as u32 {
                ERROR_OPERATION_ABORTED |
                STATUS_CANCELLED_U32 => {
                    info!("coroutine timeout");
                    set_co_para(&mut co, io::Error::new(io::ErrorKind::TimedOut, "timeout"));
                    // timer data is poped already
                }
                NO_ERROR => {
                    // do nothing here
                    // need a way to detect timeout, it's not safe to del timer here
                    // according to windows API it's can't cancel the completed io operation
                    // the timeout function would remove the timer handle
                }
                err => {
                    error!("iocp err={:?}", err);
                    set_co_para(&mut co, io::Error::from_raw_os_error(err as i32));
                }
            }

            // it's safe to remove the timer since we are
            // runing the timer_list in the same thread
            data.timer.take().map(|h| {
                unsafe {
                    // tell the timer function not to cancel the io
                    // it's not always true that you can really remove the timer entry
                    h.get_data().data.event_data = ptr::null_mut();
                }
                h.remove()
            });

            // schedule the coroutine
            s.schedule_io(co);
        }

        Ok(())
    }

    // this will post an os event so that we can wakeup the event loop
    #[inline]
    pub fn wakeup(&self) {
        self.port.post(CompletionStatus::new(0, 0, ptr::null_mut())).unwrap();
    }

    // register file hanle to the iocp
    #[inline]
    pub fn add_socket<T: AsRawSocket + ?Sized>(&self, t: &T) -> io::Result<()> {
        // the token para is not used, just pass the handle
        self.port.add_socket(t.as_raw_socket() as usize, t)
    }

    // windows register function does nothing,
    // the completion model would call the actuall API instead of register
    #[inline]
    pub fn add_io(&self, _io: &mut EventData) -> io::Result<()> {
        Ok(())
    }
}

unsafe fn cancel_io(handle: HANDLE, overlapped: &Overlapped) -> io::Result<()> {
    let overlapped = overlapped.raw();
    let ret = kernel32::CancelIoEx(handle, overlapped);
    if ret == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// when timeout happend we need to cancel the io operation
// this will trigger an event on the IOCP and processed in the selector
pub fn timeout_handler(data: TimerData) {
    if data.event_data.is_null() {
        return;
    }

    unsafe {
        let event_data = &mut *data.event_data;
        // remove the event timer
        event_data.timer.take();
        cancel_io(event_data.handle, event_data.get_overlapped())
            .map_err(|e| error!("CancelIoEx failed! e = {}", e))
            .ok(); // ignore the error, the select may grab the data first!
    }
}