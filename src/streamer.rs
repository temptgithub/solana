//! The `streamer` module defines a set of services for efficiently pulling data from UDP sockets.
//!
use influx_db_client as influxdb;
use metrics;
use packet::{Blob, BlobRecycler, PacketRecycler, SharedBlobs, SharedPackets};
use result::{Error, Result};
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{Builder, JoinHandle};
use std::time::{Duration, Instant};
use timing::duration_as_ms;

pub type PacketReceiver = Receiver<SharedPackets>;
pub type PacketSender = Sender<SharedPackets>;
pub type BlobSender = Sender<SharedBlobs>;
pub type BlobReceiver = Receiver<SharedBlobs>;

fn recv_loop(
    sock: &UdpSocket,
    exit: &Arc<AtomicBool>,
    re: &PacketRecycler,
    channel: &PacketSender,
    channel_tag: &'static str,
) -> Result<()> {
    loop {
        let msgs = re.allocate();
        loop {
            // Check for exit signal, even if socket is busy
            // (for instance the leader trasaction socket)
            if exit.load(Ordering::Relaxed) {
                return Ok(());
            }
            let result = msgs.write().recv_from(sock);
            match result {
                Ok(()) => {
                    let len = msgs.read().packets.len();
                    metrics::submit(
                        influxdb::Point::new(channel_tag)
                            .add_field("count", influxdb::Value::Integer(len as i64))
                            .to_owned(),
                    );
                    channel.send(msgs)?;
                    break;
                }
                Err(_) => (),
            }
        }
    }
}

pub fn receiver(
    sock: Arc<UdpSocket>,
    exit: Arc<AtomicBool>,
    packet_sender: PacketSender,
    sender_tag: &'static str,
) -> JoinHandle<()> {
    let res = sock.set_read_timeout(Some(Duration::new(1, 0)));
    let recycler = PacketRecycler::default();
    if res.is_err() {
        panic!("streamer::receiver set_read_timeout error");
    }
    Builder::new()
        .name("solana-receiver".to_string())
        .spawn(move || {
            let _ = recv_loop(&sock, &exit, &recycler, &packet_sender, sender_tag);
            ()
        }).unwrap()
}

fn recv_send(sock: &UdpSocket, r: &BlobReceiver) -> Result<()> {
    let timer = Duration::new(1, 0);
    let msgs = r.recv_timeout(timer)?;
    Blob::send_to(sock, msgs)?;
    Ok(())
}

pub fn recv_batch(recvr: &PacketReceiver) -> Result<(Vec<SharedPackets>, usize, u64)> {
    let timer = Duration::new(1, 0);
    let msgs = recvr.recv_timeout(timer)?;
    let recv_start = Instant::now();
    trace!("got msgs");
    let mut len = msgs.read().packets.len();
    let mut batch = vec![msgs];
    while let Ok(more) = recvr.try_recv() {
        trace!("got more msgs");
        len += more.read().packets.len();
        batch.push(more);

        if len > 100_000 {
            break;
        }
    }
    trace!("batch len {}", batch.len());
    Ok((batch, len, duration_as_ms(&recv_start.elapsed())))
}

pub fn responder(name: &'static str, sock: Arc<UdpSocket>, r: BlobReceiver) -> JoinHandle<()> {
    Builder::new()
        .name(format!("solana-responder-{}", name))
        .spawn(move || loop {
            if let Err(e) = recv_send(&sock, &r) {
                match e {
                    Error::RecvTimeoutError(RecvTimeoutError::Disconnected) => break,
                    Error::RecvTimeoutError(RecvTimeoutError::Timeout) => (),
                    _ => warn!("{} responder error: {:?}", name, e),
                }
            }
        }).unwrap()
}

//TODO, we would need to stick block authentication before we create the
//window.
fn recv_blobs(recycler: &BlobRecycler, sock: &UdpSocket, s: &BlobSender) -> Result<()> {
    trace!("recv_blobs: receiving on {}", sock.local_addr().unwrap());
    let dq = Blob::recv_from(recycler, sock)?;
    if !dq.is_empty() {
        s.send(dq)?;
    }
    Ok(())
}

pub fn blob_receiver(sock: Arc<UdpSocket>, exit: Arc<AtomicBool>, s: BlobSender) -> JoinHandle<()> {
    //DOCUMENTED SIDE-EFFECT
    //1 second timeout on socket read
    let timer = Duration::new(1, 0);
    sock.set_read_timeout(Some(timer))
        .expect("set socket timeout");
    let recycler = BlobRecycler::default();
    Builder::new()
        .name("solana-blob_receiver".to_string())
        .spawn(move || loop {
            if exit.load(Ordering::Relaxed) {
                break;
            }
            let _ = recv_blobs(&recycler, &sock, &s);
        }).unwrap()
}

#[cfg(test)]
mod test {
    use packet::{Blob, BlobRecycler, Packet, Packets, PACKET_DATA_SIZE};
    use std::io;
    use std::io::Write;
    use std::net::UdpSocket;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::channel;
    use std::sync::Arc;
    use std::time::Duration;
    use streamer::PacketReceiver;
    use streamer::{receiver, responder};

    fn get_msgs(r: PacketReceiver, num: &mut usize) {
        for _t in 0..5 {
            let timer = Duration::new(1, 0);
            match r.recv_timeout(timer) {
                Ok(m) => *num += m.read().packets.len(),
                _ => info!("get_msgs error"),
            }
            if *num == 10 {
                break;
            }
        }
    }
    #[test]
    pub fn streamer_debug() {
        write!(io::sink(), "{:?}", Packet::default()).unwrap();
        write!(io::sink(), "{:?}", Packets::default()).unwrap();
        write!(io::sink(), "{:?}", Blob::default()).unwrap();
    }
    #[test]
    pub fn streamer_send_test() {
        let read = UdpSocket::bind("127.0.0.1:0").expect("bind");
        read.set_read_timeout(Some(Duration::new(1, 0))).unwrap();

        let addr = read.local_addr().unwrap();
        let send = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let exit = Arc::new(AtomicBool::new(false));
        let resp_recycler = BlobRecycler::default();
        let (s_reader, r_reader) = channel();
        let t_receiver = receiver(Arc::new(read), exit.clone(), s_reader, "streamer-test");
        let t_responder = {
            let (s_responder, r_responder) = channel();
            let t_responder = responder("streamer_send_test", Arc::new(send), r_responder);
            let mut msgs = Vec::new();
            for i in 0..10 {
                let mut b = resp_recycler.allocate();
                {
                    let mut w = b.write();
                    w.data[0] = i as u8;
                    w.meta.size = PACKET_DATA_SIZE;
                    w.meta.set_addr(&addr);
                }
                msgs.push(b);
            }
            s_responder.send(msgs).expect("send");
            t_responder
        };

        let mut num = 0;
        get_msgs(r_reader, &mut num);
        assert_eq!(num, 10);
        exit.store(true, Ordering::Relaxed);
        t_receiver.join().expect("join");
        t_responder.join().expect("join");
    }
}
