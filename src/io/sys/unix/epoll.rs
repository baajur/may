use std::time::Duration;
use std::os::unix::io::RawFd;
use std::{io, cmp, ptr, isize};
use smallvec::SmallVec;
use timeout_list::{now, ns_to_ms};
use super::nix::sys::epoll::*;
use super::nix::unistd::{read, write, close};
use super::libc::{eventfd, EFD_NONBLOCK};
use super::{EventFlags, FLAG_READ, FLAG_WRITE};
use super::{EventData, TimerList, from_nix_error, timeout_handler};

// covert interested event into system EpollEventKind
#[inline]
fn interest_to_epoll_kind(interest: EventFlags) -> EpollEventKind {
    let mut kind = EpollEventKind::from(EPOLLONESHOT | EPOLLET);

    if interest.contains(FLAG_READ) {
        kind.insert(EPOLLIN);
    }

    if interest.contains(FLAG_WRITE) {
        kind.insert(EPOLLOUT);
    }
    // kind.insert(EPOLLRDHUP);
    kind
}

fn create_eventfd() -> io::Result<RawFd> {
    let fd = unsafe { eventfd(0, EFD_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd as RawFd)
}

pub type SysEvent = EpollEvent;

struct SingleSelector {
    epfd: RawFd,
    evfd: RawFd,
    timer_list: TimerList,
}

impl SingleSelector {
    pub fn new() -> io::Result<Self> {
        let info = EpollEvent {
            events: EpollEventKind::from(EPOLLONESHOT | EPOLLET | EPOLLIN),
            data: 0, // wakeup data is 0
        };

        let epfd = try!(epoll_create().map_err(from_nix_error));
        let evfd = try!(create_eventfd());

        // add the eventfd to the epfd
        try!(epoll_ctl(epfd, EpollOp::EpollCtlAdd, evfd, &info).map_err(from_nix_error));

        Ok(SingleSelector {
            epfd: epfd,
            evfd: evfd,
            timer_list: TimerList::new(),
        })
    }
}

impl Drop for SingleSelector {
    fn drop(&mut self) {
        let _ = close(self.evfd);
        let _ = close(self.epfd);
    }
}

pub struct Selector {
    // 128 should be fine for max io threads
    vec: SmallVec<[SingleSelector; 128]>,
}

impl Selector {
    pub fn new(io_workers: usize) -> io::Result<Self> {
        let mut s = Selector { vec: SmallVec::new() };

        for _ in 0..io_workers {
            let ss = try!(SingleSelector::new());
            s.vec.push(ss);
        }

        Ok(s)
    }

    pub fn select(&self,
                  id: usize,
                  events: &mut [SysEvent],
                  timeout: Option<u64>)
                  -> io::Result<Option<u64>> {
        let timeout_ms = timeout.map(|to| cmp::min(ns_to_ms(to), isize::MAX as u64) as isize)
            .unwrap_or(-1);
        // info!("select; timeout={:?}", timeout_ms);

        // Wait for epoll events for at most timeout_ms milliseconds
        let n = try!(epoll_wait(self.vec[id].epfd, events, timeout_ms).map_err(from_nix_error));

        for event in events[..n].iter() {
            if event.data == 0 {
                // this is just a wakeup event, ignore it
                let mut buf = [0u8; 8];
                // clear the eventfd, ignore the result
                read(self.vec[id].evfd, &mut buf).ok();
                // info!("got wakeup event in select");
                continue;
            }
            let data = unsafe { &mut *(event.data as *mut EventData) };
            let mut co = data.co.take().expect("can't get co in selector");
            co.prefetch();

            // it's safe to remove the timer since we are runing the timer_list in the same thread
            data.timer.take().map(|h| {
                unsafe {
                    // tell the timer hanler not to cancel the io
                    // it's not always true that you can really remove the timer entry
                    h.get_data().data.event_data = ptr::null_mut();
                }
                h.remove()
            });

            // schedule the coroutine
            match co.resume() {
                Some(ev) => ev.subscribe(co),
                None => panic!("coroutine not return!"),
            }
        }

        // deal with the timer list
        let next_expire = self.vec[id].timer_list.schedule_timer(now(), &timeout_handler);
        Ok(next_expire)
    }

    // this will post an os event so that we can wakeup the event loop
    #[inline]
    fn wakeup(&self, id: usize) {
        let buf = unsafe { ::std::slice::from_raw_parts(&1u64 as *const u64 as _, 8) };
        let ret = write(self.vec[id].evfd, buf);
        info!("wakeup id={:?}, ret={:?}", id, ret);
    }

    // register io event to the selector
    // #[inline]
    // pub fn add_fd(&self, fd: RawFd) -> io::Result<()> {
    //     let info = EpollEvent {
    //         events: EpollEventKind::empty(),
    //         data: 0,
    //     };
    //     let epfd = self.epfd[fd as usize % self.epfd.len()];
    //     info!("add fd to epoll select, fd={:?}", fd);
    //     epoll_ctl(epfd, EpollOp::EpollCtlAdd, fd, &info).map_err(from_nix_error)
    // }

    // register io event to the selector
    #[inline]
    pub fn add_io(&self, ev_data: &EventData) -> io::Result<()> {
        let info = EpollEvent {
            events: interest_to_epoll_kind(ev_data.interest),
            data: ev_data as *const _ as _,
        };
        let fd = ev_data.fd;
        let id = fd as usize % self.vec.len();
        let epfd = self.vec[id].epfd;
        info!("mod fd to epoll select, fd={:?}", fd);
        epoll_ctl(epfd, EpollOp::EpollCtlAdd, fd, &info).map_err(from_nix_error)
    }

    #[inline]
    pub fn del_fd(&self, fd: RawFd) {
        let info = EpollEvent {
            events: EpollEventKind::empty(),
            data: 0,
        };
        let id = fd as usize % self.vec.len();
        let epfd = self.vec[id].epfd;
        info!("add fd to epoll select, fd={:?}", fd);
        epoll_ctl(epfd, EpollOp::EpollCtlDel, fd, &info).ok();
    }

    // register the io request to the timeout list
    #[inline]
    pub fn add_io_timer(&self, io: &mut EventData, timeout: Option<Duration>) {
        let id = io.fd as usize % self.vec.len();
        io.timer = timeout.map(|dur| {
            // info!("io timeout = {:?}", dur);
            let (h, b_new) = self.vec[id].timer_list.add_timer(dur, io.timer_data());
            if b_new {
                // wakeup the event loop threead to recal the next wait timeout
                self.wakeup(id);
            }
            h
        });
    }
}
