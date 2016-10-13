use std::{self, io};
use std::time::Duration;
use std::sync::atomic::Ordering;
use std::net::{ToSocketAddrs, SocketAddr};
use std::os::unix::io::{FromRawFd, IntoRawFd, AsRawFd};
use super::super::libc;
use super::super::{IoData, co_io_result};
use io::add_socket;
use net::TcpStream;
use net2::TcpBuilder;
use yield_now::yield_with;
use scheduler::get_scheduler;
use coroutine::{CoroutineImpl, EventSource};

pub struct TcpStreamConnect {
    io_data: IoData,
    builder: TcpBuilder,
    ret: Option<io::Result<TcpStream>>,
    addr: SocketAddr,
}

impl TcpStreamConnect {
    pub fn new<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let err = io::Error::new(io::ErrorKind::Other, "no socket addresses resolved");
        try!(addr.to_socket_addrs())
            .fold(Err(err), |prev, addr| {
                prev.or_else(|_| {
                    let builder = match addr {
                        SocketAddr::V4(..) => try!(TcpBuilder::new_v4()),
                        SocketAddr::V6(..) => try!(TcpBuilder::new_v6()),
                    };
                    Ok((builder, addr))
                })
            })
            .and_then(|(builder, addr)| {
                // before yield we must set the socket to nonblocking mode and registe to selector
                let fd = builder.as_raw_fd();
                let s: std::net::TcpStream = unsafe { FromRawFd::from_raw_fd(fd) };
                try!(s.set_nonblocking(true));
                // prevent close the socket
                s.into_raw_fd();

                // register the socket
                add_socket(&builder).map(|io| {
                    // unix connect is some like completion mode
                    // we must give the connect request first to the system
                    let ret = match builder.connect(&addr) {
                        Err(ref e) if e.raw_os_error() == Some(libc::EINPROGRESS) => None,
                        ret @ _ => Some(ret.map(|s| TcpStream::from_stream(s, io))),
                    };

                    TcpStreamConnect {
                        io_data: io,
                        builder: builder,
                        ret: ret,
                        addr: addr,
                    }
                })
            })
    }

    #[inline]
    pub fn done(self) -> io::Result<TcpStream> {
        match self.ret {
            Some(s) => return s,
            None => {}
        }

        loop {
            try!(co_io_result());

            match self.builder.connect(&self.addr) {
                Err(ref e) if e.raw_os_error() == Some(libc::EINPROGRESS) => {}
                Err(ref e) if e.raw_os_error() == Some(libc::EALREADY) => {}
                ret @ _ => return ret.map(|s| TcpStream::from_stream(s, self.io_data)),
            }

            // clear the events
            if self.io_data.inner().io_flag.swap(0, Ordering::Relaxed) != 0 {
                continue;
            }

            // the result is still EINPROGRESS, need to try again
            yield_with(&self);
        }
    }
}

impl EventSource for TcpStreamConnect {
    fn subscribe(&mut self, co: CoroutineImpl) {
        let s = get_scheduler();
        let io_data = self.io_data.inner();
        s.add_io_timer(io_data, Some(Duration::from_secs(10)));
        io_data.co.swap(co, Ordering::Release);

        // there is no event
        if self.io_data.inner().io_flag.load(Ordering::Relaxed) == 0 {
            return;
        }

        // since we got data here, need to remove the timer handle and schedule
        self.io_data.inner().schedule();
    }
}
