use std::cell::RefCell;
use std::rc::Rc;
use std::net::SocketAddr;
use std::time::Duration;
use std::thread;
use std::io::{Error, ErrorKind};

use futures::{future, Future, Sink};
use futures::stream::{self, Stream, SplitStream};
use futures::sync::mpsc::{Sender, Receiver};
use tokio_core::reactor::Core;
use tokio_core::net::TcpStream;
use tokio_timer::Timer;
use tokio_io::AsyncRead;
use tokio_io::codec::Framed;

use mqtt3::Packet;


use error::*;
use mqttopts::{MqttOptions, ReconnectOptions};
use client::Request;
use client::state::MqttState;
use codec::MqttCodec;
use crossbeam_channel;

pub struct Connection {
    notifier_tx: crossbeam_channel::Sender<Packet>,
    commands_tx: Sender<Request>,

    mqtt_state: Rc<RefCell<MqttState>>,
    opts: MqttOptions,
    reactor: Core,
}

impl Connection {
    pub fn new(opts: MqttOptions, commands_tx: Sender<Request>, notifier_tx: crossbeam_channel::Sender<Packet>) -> Self {
        Connection {
            notifier_tx: notifier_tx,
            commands_tx: commands_tx,
            mqtt_state: Rc::new(RefCell::new(MqttState::new(opts.clone()))),
            opts: opts,
            reactor: Core::new().unwrap()
        }
    }

    pub fn start(&mut self, mut commands_rx: Receiver<Request>) {
        let initial_connect = self.mqtt_state.borrow().initial_connect();
        let reconnect_opts = self.opts.reconnect;

        'reconnect: loop {
            let framed = match self.mqtt_connect() {
                Ok(framed) => framed,
                Err(e) => {
                    error!("Connection error = {:?}", e);
                    match reconnect_opts {
                        ReconnectOptions::Never => break 'reconnect,
                        ReconnectOptions::AfterFirstSuccess(d) if !initial_connect => {
                            info!("Will retry connecting again in {} seconds", d);
                            thread::sleep(Duration::new(u64::from(d), 0));
                            continue 'reconnect;
                        }
                        ReconnectOptions::AfterFirstSuccess(_) => break 'reconnect,
                        ReconnectOptions::Always(d) => {
                            info!("Will retry connecting again in {} seconds", d);
                            thread::sleep(Duration::new(u64::from(d), 0));
                            continue 'reconnect;
                        }
                    }
                }
            };

            let (mut sender, receiver) = framed.split();
            let process_network_msg = self.mqtt_network_recv_future(receiver).then(|result| {
                // This returns when the stream is done or has errored out
                match result {
                    Ok(v) => {
                        error!("Network receiver done!!. Result = {:?}", v);
                        // its the end of stream or either an error, so we return an error
                        // to jump back to reconnect loop
                        return future::err(())
                    }
                    Err(e) => error!("N/w receiver failed. Error = {:?}", e),
                }
                future::ok(())
            });

            // spawn ping timer
            if let Some(keep_alive) = self.opts.keep_alive {
                self.spawn_ping_timer(keep_alive);
            }

            // republish last session unacked packets
            let last_session_publishes = self.mqtt_state.borrow_mut().handle_reconnection();
            if last_session_publishes.is_some() {
                for publish in last_session_publishes.unwrap() {
                    let packet = Packet::Publish(publish);
                    sender = sender.send(packet).wait().unwrap();
                }
            }

            // receive incoming user request and write to network
            let mqtt_state = self.mqtt_state.clone();
            let client_cmd_rx = commands_rx.by_ref();
            let process_client_msg = client_cmd_rx.map(|msg| mqtt_state.borrow_mut().handle_client_requests(msg).unwrap())
                                             .map_err(|_| Error::new(ErrorKind::Other, "Error receiving client msg"))
                                             .forward(sender)
                                             .map(|_| ())
                                             .or_else(|_| { error!("Client send error"); future::ok(())});

            let fused_future = process_network_msg.select(process_client_msg);
            if let Err(_) = self.reactor.run(fused_future) {
                error!("Reactor halted. Error");
            }
        }
    }

    fn mqtt_network_recv_future(&self, receiver: SplitStream<Framed<TcpStream, MqttCodec>>) -> Box<Future<Item=(), Error=Error>> {
        let mqtt_state = self.mqtt_state.clone();
        let mut commands_tx = self.commands_tx.clone();
        let notifier = self.notifier_tx.clone();
        let receiver = receiver.then(move |result| {
            let commands_tx = &mut commands_tx;
            let message = match result {
                Ok(m) => {
                    info!("Received {:?}", m);
                    m
                },
                Err(e) => {
                    error!("Network receiver error = {:?}", e);
                    commands_tx.send(Request::Disconnect).wait().unwrap();
                    return future::err(e)
                }
            };

            match message {
                Packet::Connack(connack) => {
                    if let Err(e) = mqtt_state.borrow_mut().handle_incoming_connack(connack) {
                        error!("Connack failed. Error = {:?}", e);
                    }
                }
                Packet::Puback(ack) => {
                    if let Err(e) = notifier.try_send(Packet::Puback(ack)) {
                        error!("Puback notification send failed. Error = {:?}", e);
                    }
                    // ignore unsolicited ack errors
                    let _ = mqtt_state.borrow_mut().handle_incoming_puback(ack);
                }
                Packet::Pingresp => mqtt_state.borrow_mut().handle_incoming_pingresp(),
                Packet::Publish(publish) => {
                    let (publish, ack) = mqtt_state.borrow_mut().handle_incoming_publish(publish);
                    if let Some(publish) = publish {
                        if let Err(e) = notifier.try_send(Packet::Publish(publish)) {
                            error!("Publish notification send failed. Error = {:?}", e);
                        }
                    }
                    if let Some(ack) = ack {
                        match ack {
                            Packet::Puback(pkid) => {
                                commands_tx.send(Request::Puback(pkid)).wait().unwrap();
                            }
                            _ => unimplemented!()
                        };
                    }
                }
                Packet::Suback(suback) => {
                    if let Err(e) = notifier.try_send(Packet::Suback(suback)) {
                        error!("Suback notification send failed. Error = {:?}", e);
                    }
                }
                Packet::Disconnect => {
                    warn!("Sending disconnect packet");
                    commands_tx.send(Request::Disconnect).wait().unwrap();
                }
                _ => unimplemented!()
            }

            future::ok(())
        }).for_each(|_| future::ok(()));
        Box::new(receiver)
    }

    fn mqtt_connect(&mut self) -> Result<Framed<TcpStream, MqttCodec>, ConnectError> {
        // NOTE: make sure that dns resolution happens during reconnection to handle changes in server ip
        let addr: SocketAddr = self.opts.broker_addr.as_str().parse().unwrap();
        let handle = self.reactor.handle();
        let mqtt_state = self.mqtt_state.clone();
        
        // TODO: Add TLS support with client authentication (ca = roots.pem for iotcore)

        let future_response = TcpStream::connect(&addr, &handle).and_then(|connection| {
            let framed = connection.framed(MqttCodec);
            let connect = mqtt_state.borrow_mut().handle_outgoing_connect();
            let future_mqtt_connect = framed.send(Packet::Connect(connect));

            future_mqtt_connect.and_then(|framed| {
                framed.into_future().and_then(|(res, stream)| Ok((res, stream))).map_err(|(err, _stream)| err)
            })
        });

        let response = self.reactor.run(future_response);
        let (packet, frame) = response?;

        // Return `Framed` and previous session packets that are to be republished
        match packet.unwrap() {
            Packet::Connack(connack) => {
                self.mqtt_state.borrow_mut().handle_incoming_connack(connack)?;
                Ok(frame)
            }
            _ => unimplemented!(),
        }
    }

    fn spawn_ping_timer(&self, keep_alive: u16) {
        let timer = Timer::default();
        let interval = timer.interval(Duration::new(u64::from(keep_alive), 0));
        let mqtt_state = self.mqtt_state.clone();
        let mut commands_tx = self.commands_tx.clone();
        let handle = self.reactor.handle();
        let timer_future = interval.for_each(move |_t| {
            let ref mut commands_tx = commands_tx;
            if mqtt_state.borrow().is_ping_required() {
                debug!("Ping timer fire");
                commands_tx.send(Request::Ping).wait().unwrap();
            }
            future::ok(())
        });

        handle.spawn(
            timer_future.then(move |result| {
                    match result {
                        Ok(_) => error!("Ping timer done"),
                        Err(e) => error!("Ping timer IO error {:?}", e),
                    }
                    future::ok(())
                }
            )
        )
    }
}