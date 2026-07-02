/*! Virtual device backed by bounded in-process packet queues.

[`ChannelDevice`] bridges between asynchronous I/O (e.g. a TUN interface read
from a tokio task) and smoltcp's synchronous poll loop, without introducing
unbounded buffering.

# Backpressure model

* **RX (external → smoltcp):** caller pushes packets via [`ChannelDevice::push_rx`].
  When the RX queue is full the method returns `false`; the caller should stop
  reading from the OS device until space opens up. smoltcp drains the queue
  on every [`Interface::poll`] call.

* **TX (smoltcp → external):** smoltcp pushes outgoing packets via the standard
  [`TxToken`] mechanism. They accumulate in a bounded TX queue. The caller
  drains them via [`ChannelDevice::pop_tx`] after each poll and forwards them
  to the OS device. If the TX queue is full a packet is silently dropped; TCP
  will retransmit, and the queue fills only on extreme burst.
*/

use std::collections::VecDeque;

use crate::{
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    time::Instant,
};

/// A virtual smoltcp [`Device`] backed by two bounded in-memory packet queues.
///
/// Create it with [`ChannelDevice::new`], integrate with an async TUN reader by
/// calling [`push_rx`](Self::push_rx) and drain outgoing packets with
/// [`pop_tx`](Self::pop_tx).
pub struct ChannelDevice {
    caps: DeviceCapabilities,
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
    rx_cap: usize,
    tx_cap: usize,
}

impl ChannelDevice {
    /// Create a new device with the given capabilities.
    ///
    /// `rx_capacity` limits how many inbound packets can be queued before
    /// backpressure kicks in. `tx_capacity` limits outbound packets.
    /// Typical values: 32–128 packets each.
    pub fn new(caps: DeviceCapabilities, rx_capacity: usize, tx_capacity: usize) -> Self {
        Self {
            caps,
            rx: VecDeque::with_capacity(rx_capacity),
            tx: VecDeque::with_capacity(tx_capacity),
            rx_cap: rx_capacity,
            tx_cap: tx_capacity,
        }
    }

    /// Push a packet received from the OS (e.g. TUN) into the RX queue so
    /// smoltcp can process it on the next [`Interface::poll`].
    ///
    /// Returns `true` if the packet was accepted, `false` if the queue is full
    /// (backpressure: caller should pause reading from the OS device).
    pub fn push_rx(&mut self, pkt: Vec<u8>) -> bool {
        if self.rx.len() < self.rx_cap {
            self.rx.push_back(pkt);
            true
        } else {
            false
        }
    }

    /// Pop the next packet that smoltcp wants to send to the OS (e.g. TUN).
    /// Returns `None` when there are no outgoing packets.
    pub fn pop_tx(&mut self) -> Option<Vec<u8>> {
        self.tx.pop_front()
    }

    /// Returns `true` if there are inbound packets waiting for smoltcp.
    pub fn has_rx(&self) -> bool {
        !self.rx.is_empty()
    }

    /// Returns `true` if the RX queue is at its capacity limit.
    pub fn rx_full(&self) -> bool {
        self.rx.len() >= self.rx_cap
    }

    /// Returns `true` if smoltcp has produced outgoing packets.
    pub fn has_tx(&self) -> bool {
        !self.tx.is_empty()
    }

    /// Current number of packets in the RX queue.
    pub fn rx_len(&self) -> usize {
        self.rx.len()
    }
}

impl Device for ChannelDevice {
    type RxToken<'a> = ChannelRxToken;
    type TxToken<'a> = ChannelTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        self.rx.pop_front().map(|pkt| {
            let tx_token = ChannelTxToken {
                tx: &mut self.tx,
                cap: self.tx_cap,
            };
            (ChannelRxToken(pkt), tx_token)
        })
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(ChannelTxToken {
            tx: &mut self.tx,
            cap: self.tx_cap,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        self.caps.clone()
    }
}

// ---------------------------------------------------------------------------

pub struct ChannelRxToken(Vec<u8>);

impl RxToken for ChannelRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.0)
    }
}

// ---------------------------------------------------------------------------

pub struct ChannelTxToken<'a> {
    tx: &'a mut VecDeque<Vec<u8>>,
    cap: usize,
}

impl TxToken for ChannelTxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let result = f(&mut buf);
        if self.tx.len() < self.cap {
            self.tx.push_back(buf);
        }
        // If TX queue is full the packet is silently dropped.
        // TCP will retransmit; UDP is best-effort anyway.
        result
    }
}

// ---------------------------------------------------------------------------

/// Helper: returns a [`DeviceCapabilities`] suitable for an IP-mode TUN device.
pub fn ip_tun_capabilities(mtu: usize) -> DeviceCapabilities {
    let mut caps = DeviceCapabilities::default();
    caps.medium = Medium::Ip;
    caps.max_transmission_unit = mtu;
    caps
}
