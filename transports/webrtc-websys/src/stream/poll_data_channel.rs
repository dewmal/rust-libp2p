// This crate inspired by webrtc::data::data_channel::poll_data_channel.rs
use crate::error::Error;
use futures::channel;
use futures::{AsyncRead, AsyncWrite, FutureExt, StreamExt};
use std::fmt;
use std::io;
use std::pin::Pin;
use std::result::Result;
use std::task::{ready, Context, Poll};
use wasm_bindgen::{prelude::*, JsCast};
use web_sys::{MessageEvent, RtcDataChannel, RtcDataChannelEvent, RtcDataChannelState};

/// Default capacity of the temporary read buffer used by [`webrtc_sctp::stream::PollStream`].
const DEFAULT_READ_BUF_SIZE: usize = 8192;

/// A wrapper around around [`RtcDataChannel`], which implements [`AsyncRead`] and
/// [`AsyncWrite`].
///
/// Both `poll_read` and `poll_write` calls allocate temporary buffers, which results in an
/// additional overhead.
pub struct PollDataChannel {
    /// The [RtcDataChannel]
    data_channel: RtcDataChannel,

    /// Receive messages from the RtcDataChannel callback
    /// mpsc since multiple messages may be sent
    rx_onmessage: channel::mpsc::Receiver<Vec<u8>>,
    /// Receive onopen event from the RtcDataChannel callback
    /// oneshot since only one onopen event is sent
    rx_onopen: channel::mpsc::Receiver<()>,
    /// Receieve onbufferedamountlow event from the RtcDataChannel callback
    /// mpsc since multiple onbufferedamountlow events may be sent
    rx_onbufferedamountlow: channel::mpsc::Receiver<()>,
    /// Receive onclose event from the RtcDataChannel callback
    /// oneshot since only one onclose event is sent
    rx_onclose: channel::oneshot::Receiver<()>,

    read_buf_cap: usize,
}

impl PollDataChannel {
    /// Constructs a new `PollDataChannel`.
    pub(crate) fn new(data_channel: RtcDataChannel) -> Self {
        /*
         * On Open
         */
        let (mut tx_onopen, rx_onopen) = channel::mpsc::channel(2);

        let onopen_callback = Closure::<dyn FnMut(_)>::new(move |_ev: RtcDataChannelEvent| {
            log::debug!("Data Channel opened");
            if let Err(e) = tx_onopen.try_send(()) {
                log::error!("Error sending onopen event {:?}", e);
            }
        });

        data_channel.set_onopen(Some(onopen_callback.as_ref().unchecked_ref()));
        onopen_callback.forget();

        /*
         * On Message Stream
         */
        let (mut tx_onmessage, rx_onmessage) = channel::mpsc::channel(2); // TODO: How big should this be?

        let onmessage_callback = Closure::<dyn FnMut(_)>::new(move |ev: MessageEvent| {
            let data = ev.data();
            // Convert from Js ArrayBuffer to Vec<u8>
            let data = js_sys::Uint8Array::new(&data).to_vec();
            if let Err(e) = tx_onmessage.try_send(data) {
                log::error!("Error sending onmessage event {:?}", e)
            }
        });

        data_channel.set_onmessage(Some(onmessage_callback.as_ref().unchecked_ref()));
        onmessage_callback.forget();

        // On Close
        let (tx_onclose, rx_onclose) = channel::oneshot::channel();

        let onclose_callback = Closure::once_into_js(move |_ev: RtcDataChannelEvent| {
            log::trace!("Data Channel closed");
            // TODO: This is Erroring, likely because the channel is already closed by the time we try to send/receive this?
            if let Err(e) = tx_onclose.send(()) {
                log::error!("Error sending onclose event {:?}", e);
            }
        });

        data_channel.set_onclose(Some(onclose_callback.as_ref().unchecked_ref()));
        // Note: `once_into_js` Closure does NOT call `forget()`, see the wasm_bindgen::Closure docs for more info

        /*
         * Convert `RTCDataChannel: bufferedamountlow event` Low Event Callback to Future
         */
        let (mut tx_onbufferedamountlow, rx_onbufferedamountlow) = channel::mpsc::channel(2);

        let onbufferedamountlow_callback =
            Closure::<dyn FnMut(_)>::new(move |_ev: RtcDataChannelEvent| {
                if let Err(e) = tx_onbufferedamountlow.try_send(()) {
                    log::error!("Error sending onbufferedamountlow event {:?}", e)
                }
            });

        data_channel
            .set_onbufferedamountlow(Some(onbufferedamountlow_callback.as_ref().unchecked_ref()));
        onbufferedamountlow_callback.forget();

        Self {
            data_channel,
            rx_onmessage,
            rx_onopen,
            rx_onclose,
            rx_onbufferedamountlow,
            read_buf_cap: DEFAULT_READ_BUF_SIZE,
        }
    }

    /// Get back the inner data_channel.
    pub(crate) fn into_inner(self) -> RtcDataChannel {
        self.data_channel
    }

    /// Obtain a clone of the inner data_channel.
    pub fn clone_inner(&self) -> RtcDataChannel {
        self.data_channel.clone()
    }

    /// Set the capacity of the temporary read buffer (default: 8192).
    pub fn set_read_buf_capacity(&mut self, capacity: usize) {
        self.read_buf_cap = capacity
    }

    /// Get Ready State of [RtcDataChannel]
    pub(crate) fn ready_state(&self) -> RtcDataChannelState {
        self.data_channel.ready_state()
    }

    /// Send data buffer
    pub fn send(&self, data: &[u8]) -> Result<(), Error> {
        self.data_channel.send_with_u8_array(data)?;
        Ok(())
    }

    /// StreamIdentifier returns the Stream identifier associated to the stream.
    pub(crate) fn stream_identifier(&self) -> u16 {
        // self.data_channel.id() // not released (yet), see https://github.com/rustwasm/wasm-bindgen/issues/3547

        // temp workaround: use label, though it is "" so it's not unique
        // TODO: After the above PR is released, use the label instead of the stream id
        let label = self.data_channel.label();
        let b = label.as_bytes();
        let mut stream_id: u16 = 0;
        b.iter().enumerate().for_each(|(i, &b)| {
            stream_id += (b as u16) << (8 * i);
        });
        stream_id
    }
}

impl AsyncRead for PollDataChannel {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        match ready!(self.rx_onmessage.poll_next_unpin(cx)) {
            Some(data) => {
                let data_len = data.len();
                let buf_len = buf.len();
                log::trace!("poll_read [{:?} of {} bytes]", data_len, buf_len);
                let len = std::cmp::min(data_len, buf_len);
                buf[..len].copy_from_slice(&data[..len]);
                Poll::Ready(Ok(len))
            }
            None => Poll::Ready(Ok(0)), // if None, the stream is exhausted, no data to read
        }
    }
}

impl AsyncWrite for PollDataChannel {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        log::trace!(
            "poll_write: [{:?}], state: {:?}",
            buf.len(),
            self.data_channel.ready_state()
        );
        // If the data channel is not open,
        // poll on open future until the channel is open
        if self.data_channel.ready_state() != RtcDataChannelState::Open {
            ready!(self.rx_onopen.poll_next_unpin(cx)).unwrap();
        }

        // Now that the channel is open, send the data
        match self.send(buf) {
            Ok(_) => Poll::Ready(Ok(buf.len())),
            Err(e) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Error sending data: {:?}", e),
            ))),
        }
    }

    /// Attempt to flush the object, ensuring that any buffered data reach their destination.
    /// On success, returns Poll::Ready(Ok(())).
    /// If flushing cannot immediately complete, this method returns Poll::Pending and arranges for the current task (via cx.waker().wake_by_ref()) to receive a notification when the object can make progress towards flushing.
    ///
    /// With RtcDataChannel, there no native future to await for flush to complete.
    /// However, Whenever this value decreases to fall to or below the value specified in the
    /// bufferedAmountLowThreshold property, the user agent fires the bufferedamountlow event.
    ///
    /// We can therefore create a callback future called `onbufferedamountlow_fut` to listen for `bufferedamountlow` event and wake the task
    /// The default `bufferedAmountLowThreshold` value is 0.
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // if bufferedamountlow empty, return ready
        if self.data_channel.buffered_amount() == 0 {
            return Poll::Ready(Ok(()));
        }

        // Otherwise, wait for the event to occur, so poll on onbufferedamountlow_fut
        match self.rx_onbufferedamountlow.poll_next_unpin(cx) {
            Poll::Pending => Poll::Pending,
            _ => Poll::Ready(Ok(())),
        }
    }

    /// Initiates or attempts to shut down this writer,
    /// returning success when the connection callback returns has completely shut down.
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        log::trace!("poll_close");
        self.data_channel.close();
        // TODO: Confirm that channels are closing properyl. Poll onclose 'should' be done here but it Errors when sent, as data channals can be closed by either end
        let _ = ready!(self.rx_onclose.poll_unpin(cx));
        log::trace!("close complete");
        Poll::Ready(Ok(()))
    }
}

impl Clone for PollDataChannel {
    fn clone(&self) -> PollDataChannel {
        PollDataChannel::new(self.clone_inner())
    }
}

impl fmt::Debug for PollDataChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PollDataChannel")
            .field("data_channel", &self.data_channel)
            .field("read_buf_cap", &self.read_buf_cap)
            .finish()
    }
}

impl AsRef<RtcDataChannel> for PollDataChannel {
    fn as_ref(&self) -> &RtcDataChannel {
        &self.data_channel
    }
}
