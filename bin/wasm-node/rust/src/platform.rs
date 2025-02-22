// Smoldot
// Copyright (C) 2019-2022  Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use crate::{bindings, timers::Delay};

use smoldot_light_base::ConnectError;

use core::{fmt, marker, pin::Pin, slice, str, time::Duration};
use futures::prelude::*;
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicUsize, Ordering},
};

/// Total number of bytes that all the connections created through [`Platform`] combined have
/// received.
pub static TOTAL_BYTES_RECEIVED: AtomicUsize = AtomicUsize::new(0);
/// Total number of bytes that all the connections created through [`Platform`] combined have
/// sent.
pub static TOTAL_BYTES_SENT: AtomicUsize = AtomicUsize::new(0);

pub(crate) struct Platform;

impl smoldot_light_base::Platform for Platform {
    type Delay = Delay;
    type Instant = crate::Instant;
    type Connection = core::convert::Infallible;
    type Stream = Pin<Box<Connection>>;
    type ConnectFuture = future::BoxFuture<
        'static,
        Result<
            smoldot_light_base::PlatformConnection<Self::Stream, Self::Connection>,
            ConnectError,
        >,
    >;
    type StreamDataFuture = future::BoxFuture<'static, ()>;
    type NextSubstreamFuture =
        future::Pending<Option<(Self::Stream, smoldot_light_base::PlatformSubstreamDirection)>>;

    fn now_from_unix_epoch() -> Duration {
        Duration::from_secs_f64(unsafe { bindings::unix_time_ms() } / 1000.0)
    }

    fn now() -> Self::Instant {
        crate::Instant::now()
    }

    fn sleep(duration: Duration) -> Self::Delay {
        Delay::new(duration)
    }

    fn sleep_until(when: Self::Instant) -> Self::Delay {
        Delay::new_at(when)
    }

    fn connect(url: &str) -> Self::ConnectFuture {
        let mut pointer = Box::pin(Connection {
            id: None,
            open: false,
            closed_message: None,
            messages_queue: VecDeque::with_capacity(32),
            messages_queue_first_offset: 0,
            something_happened: event_listener::Event::new(),
            _pinned: marker::PhantomPinned,
        });

        let id = u32::try_from(&*pointer as *const Connection as usize).unwrap();

        let mut error_ptr = [0u8; 9];

        let ret_code = unsafe {
            bindings::connection_new(
                id,
                u32::try_from(url.as_bytes().as_ptr() as usize).unwrap(),
                u32::try_from(url.as_bytes().len()).unwrap(),
                u32::try_from(&mut error_ptr as *mut [u8; 9] as usize).unwrap(),
            )
        };

        let err = if ret_code != 0 {
            let ptr = u32::from_le_bytes(<[u8; 4]>::try_from(&error_ptr[0..4]).unwrap());
            let len = u32::from_le_bytes(<[u8; 4]>::try_from(&error_ptr[4..8]).unwrap());
            let error_message: Box<[u8]> = unsafe {
                Box::from_raw(slice::from_raw_parts_mut(
                    usize::try_from(ptr).unwrap() as *mut u8,
                    usize::try_from(len).unwrap(),
                ))
            };

            Err(ConnectError {
                message: str::from_utf8(&error_message).unwrap().to_owned(),
                is_bad_addr: error_ptr[8] != 0,
            })
        } else {
            unsafe {
                Pin::get_unchecked_mut(pointer.as_mut()).id = Some(id);
            }

            Ok(())
        };

        async move {
            if let Err(err) = err {
                return Err(err);
            }

            loop {
                if pointer.closed_message.is_some() || pointer.open {
                    break;
                }

                let listener = unsafe {
                    Pin::get_unchecked_mut(pointer.as_mut())
                        .something_happened
                        .listen()
                };

                listener.await;
            }

            if pointer.open {
                Ok(smoldot_light_base::PlatformConnection::SingleStream(
                    pointer,
                ))
            } else {
                debug_assert!(pointer.closed_message.is_some());
                Err(ConnectError {
                    message: pointer.closed_message.as_ref().unwrap().clone(),
                    is_bad_addr: false,
                })
            }
        }
        .boxed()
    }

    fn next_substream(_connection: &mut Self::Connection) -> Self::NextSubstreamFuture {
        unreachable!()
    }

    fn open_out_substream(_connection: &mut Self::Connection) {
        unreachable!()
    }

    fn wait_more_data(connection: &mut Self::Stream) -> Self::StreamDataFuture {
        if !connection.messages_queue.is_empty() || connection.closed_message.is_some() {
            return future::ready(()).boxed();
        }

        let listener = unsafe {
            Pin::get_unchecked_mut(connection.as_mut())
                .something_happened
                .listen()
        };

        listener.boxed()
    }

    fn read_buffer(connection: &mut Self::Stream) -> Option<&[u8]> {
        if let Some(buffer) = connection.messages_queue.front() {
            debug_assert!(!buffer.is_empty());
            debug_assert!(connection.messages_queue_first_offset < buffer.len());
            Some(&buffer[connection.messages_queue_first_offset..])
        } else if connection.closed_message.is_some() {
            None
        } else {
            Some(&[])
        }
    }

    fn advance_read_cursor(connection: &mut Self::Stream, bytes: usize) {
        let this = unsafe { Pin::get_unchecked_mut(connection.as_mut()) };

        this.messages_queue_first_offset += bytes;

        if let Some(buffer) = this.messages_queue.front() {
            assert!(this.messages_queue_first_offset <= buffer.len());
            if this.messages_queue_first_offset == buffer.len() {
                this.messages_queue.pop_front();
                this.messages_queue_first_offset = 0;
            }
        } else {
            assert_eq!(bytes, 0);
        };
    }

    fn send(connection: &mut Self::Stream, data: &[u8]) {
        unsafe {
            let this = Pin::get_unchecked_mut(connection.as_mut());

            // Connection might have been closed, but API user hasn't detected it yet.
            if this.closed_message.is_some() {
                return;
            }

            TOTAL_BYTES_SENT.fetch_add(data.len(), Ordering::Relaxed);

            bindings::stream_send(
                this.id.unwrap(),
                0,
                u32::try_from(data.as_ptr() as usize).unwrap(),
                u32::try_from(data.len()).unwrap(),
            );
        }
    }
}

/// Connection connected to a target.
pub(crate) struct Connection {
    /// If `Some`, [`bindings::connection_close`] must be called. Set to a value after
    /// [`bindings::connection_new`] returns success.
    id: Option<u32>,
    /// True if [`bindings::connection_open_single_stream`] has been called.
    open: bool,
    /// `Some` if [`bindings::connection_closed`] has been called.
    closed_message: Option<String>,
    /// List of messages received through [`bindings::stream_message`]. Must never contain
    /// empty messages.
    messages_queue: VecDeque<Box<[u8]>>,
    /// Position of the read cursor within the first element of [`Connection::messages_queue`].
    messages_queue_first_offset: usize,
    /// Event notified whenever one of the fields above is modified.
    something_happened: event_listener::Event,
    /// Prevents the [`Connection`] from being unpinned.
    _pinned: marker::PhantomPinned,
}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("Connection")
            .field(self.id.as_ref().unwrap())
            .finish()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            unsafe {
                bindings::connection_close(id);
            }
        }
    }
}

pub(crate) fn connection_open_single_stream(connection_id: u32) {
    let connection = unsafe { &mut *(usize::try_from(connection_id).unwrap() as *mut Connection) };
    connection.open = true;
    connection.something_happened.notify(usize::max_value());
}

pub(crate) fn connection_open_multi_stream(connection_id: u32, peer_id_ptr: u32, peer_id_len: u32) {
    let connection = unsafe { &mut *(usize::try_from(connection_id).unwrap() as *mut Connection) };
    connection.open = true;
    connection.something_happened.notify(usize::max_value());

    let _peer_id: Box<[u8]> = {
        let peer_id_ptr = usize::try_from(peer_id_ptr).unwrap();
        let peer_id_len = usize::try_from(peer_id_len).unwrap();
        unsafe {
            Box::from_raw(slice::from_raw_parts_mut(
                peer_id_ptr as *mut u8,
                peer_id_len,
            ))
        }
    };

    todo!()
}

pub(crate) fn stream_message(connection_id: u32, _stream_id: u32, ptr: u32, len: u32) {
    let connection = unsafe { &mut *(usize::try_from(connection_id).unwrap() as *mut Connection) };

    let ptr = usize::try_from(ptr).unwrap();
    let len = usize::try_from(len).unwrap();

    TOTAL_BYTES_RECEIVED.fetch_add(len, Ordering::Relaxed);

    let message: Box<[u8]> =
        unsafe { Box::from_raw(slice::from_raw_parts_mut(ptr as *mut u8, len)) };

    // Ignore empty message to avoid all sorts of problems.
    if message.is_empty() {
        return;
    }

    if connection.messages_queue.is_empty() {
        connection.messages_queue_first_offset = 0;
    }

    // TODO: add some limit to `messages_queue`, to avoid DoS attacks?

    connection.messages_queue.push_back(message);
    connection.something_happened.notify(usize::max_value());
}

pub(crate) fn connection_stream_opened(_connection_id: u32, _stream_id: u32, _outbound: u32) {
    todo!()
}

pub(crate) fn connection_closed(connection_id: u32, ptr: u32, len: u32) {
    let connection = unsafe { &mut *(usize::try_from(connection_id).unwrap() as *mut Connection) };

    connection.closed_message = Some({
        let ptr = usize::try_from(ptr).unwrap();
        let len = usize::try_from(len).unwrap();
        let message: Box<[u8]> =
            unsafe { Box::from_raw(slice::from_raw_parts_mut(ptr as *mut u8, len)) };
        str::from_utf8(&message).unwrap().to_owned()
    });

    connection.something_happened.notify(usize::max_value());
}

pub(crate) fn stream_closed(_connection_id: u32, _stream_id: u32) {
    todo!()
}
