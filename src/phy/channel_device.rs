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
  to the OS device. While the TX queue is full [`Device::transmit`] returns
  `None` — the exact "device exhausted" signal smoltcp understands: it stops
  emitting for this poll and leaves the data in the socket's send buffer, to be
  sent after the queue drains. Packets are never dropped inside the device. (They
  used to be: a burst larger than the queue was silently discarded *after* the
  TCP state machine had already counted the segments as sent, which turned a
  local queue overflow into loss recovery — dup-ACK storms, backed-off RTOs and
  stalled downloads on exactly the fastest links.)
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
        // The RX token comes with a TX token for the reply smoltcp may owe (RST, ACK
        // of a probe). With no room for it, leave the packet queued for the next
        // poll instead of consuming it and losing the reply.
        if self.tx.len() >= self.tx_cap {
            return None;
        }
        self.rx.pop_front().map(|pkt| {
            let tx_token = ChannelTxToken { tx: &mut self.tx };
            (ChannelRxToken(pkt), tx_token)
        })
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        // Device exhausted: smoltcp stops emitting and keeps the data queued.
        if self.tx.len() >= self.tx_cap {
            return None;
        }
        Some(ChannelTxToken { tx: &mut self.tx })
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
}

impl TxToken for ChannelTxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let result = f(&mut buf);
        // Tokens are only handed out while there is room (`transmit`/`receive`), so
        // this never grows the queue past its capacity in practice; the push is
        // unconditional because a packet smoltcp has already accounted for must
        // never be lost here.
        self.tx.push_back(buf);
        result
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(tx_cap: usize) -> ChannelDevice {
        ChannelDevice::new(ip_tun_capabilities(1450), 8, tx_cap)
    }

    #[test]
    fn transmit_reports_exhaustion_instead_of_dropping() {
        let mut d = dev(2);
        let now = Instant::from_millis(0);
        for i in 0..2u8 {
            d.transmit(now).expect("room").consume(1, |b| b[0] = i);
        }
        assert!(d.transmit(now).is_none(), "full queue must refuse new tokens");
        // Nothing was lost, and order is preserved.
        assert_eq!(d.pop_tx(), Some(vec![0]));
        assert_eq!(d.pop_tx(), Some(vec![1]));
        // Draining frees the room again.
        assert!(d.transmit(now).is_some());
    }

    #[test]
    fn receive_waits_while_replies_would_not_fit() {
        let mut d = dev(1);
        let now = Instant::from_millis(0);
        d.push_rx(vec![9]);
        d.transmit(now).unwrap().consume(1, |_| {});
        assert!(d.receive(now).is_none(), "no TX room: keep the packet queued");
        assert_eq!(d.rx_len(), 1);
        d.pop_tx();
        let (rx, _tx) = d.receive(now).expect("room again");
        rx.consume(|p| assert_eq!(p, &[9]));
    }
}

/// Helper: returns a [`DeviceCapabilities`] suitable for an IP-mode TUN device.
pub fn ip_tun_capabilities(mtu: usize) -> DeviceCapabilities {
    let mut caps = DeviceCapabilities::default();
    caps.medium = Medium::Ip;
    caps.max_transmission_unit = mtu;
    caps
}
