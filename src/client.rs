use std::time::{Duration, Instant};
use rand::{self, Rng};
use std::net::{SocketAddr, ToSocketAddrs};
use error::{Error, Result};
use message::Message;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::str;
use mioco::tcp::TcpStream;
use mqtt::{Encodable, Decodable, QualityOfService, TopicFilter};
use mqtt::packet::*;
use mqtt::control::variable_header::{ConnectReturnCode, PacketIdentifier};
use mioco::timer::Timer;
use mioco;
use mioco::sync::mpsc::{Sender, Receiver};

#[derive(Clone)]
pub struct ClientOptions {
    keep_alive: Option<u16>,
    clean_session: bool,
    client_id: Option<String>,
    username: Option<String>,
    password: Option<String>,
    reconnect: ReconnectMethod,
}


impl ClientOptions {
    pub fn new() -> ClientOptions {
        ClientOptions {
            keep_alive: Some(5),
            clean_session: true,
            client_id: None,
            username: None,
            password: None,
            reconnect: ReconnectMethod::ForeverDisconnect,
        }
    }

    pub fn set_keep_alive(&mut self, secs: u16) -> &mut ClientOptions {
        self.keep_alive = Some(secs);
        self
    }

    pub fn set_client_id(&mut self, client_id: String) -> &mut ClientOptions {
        self.client_id = Some(client_id);
        self
    }

    pub fn set_clean_session(&mut self, clean_session: bool) -> &mut ClientOptions {
        self.clean_session = clean_session;
        self
    }


    pub fn generate_client_id(&mut self) -> &mut ClientOptions {
        let mut rng = rand::thread_rng();
        let id = rng.gen::<u32>();
        self.client_id = Some(format!("mqttc_{}", id));
        self
    }

    pub fn set_username(&mut self, username: String) -> &mut ClientOptions {
        self.username = Some(username);
        self
    }

    pub fn set_password(&mut self, password: String) -> &mut ClientOptions {
        self.password = Some(password);
        self
    }

    pub fn set_reconnect(&mut self, reconnect: ReconnectMethod) -> &mut ClientOptions {
        self.reconnect = reconnect;
        self
    }

    pub fn connect<A: ToSocketAddrs>(mut self, addr: A) -> Result<(Proxy, Subscriber)> {
        if self.client_id == None {
            self.generate_client_id();
        }

        let addr = try!(addr.to_socket_addrs()).next().expect("Socket address is broken");
        let (sub_send, sub_recv) = mioco::sync::mpsc::channel::<Vec<(TopicFilter,
                                                                     QualityOfService)>>();
        let (msg_send, msg_recv) = mioco::sync::mpsc::channel::<Message>();

        let proxy = Proxy {
            addr: addr,
            opts: self,
            stream: None,
            session_present: false,
            subscribe_recv: sub_recv,
            message_send: msg_send,
        };

        let subscriber = Subscriber { subscribe_send: sub_send, message_recv: msg_recv };

        Ok((proxy, subscriber))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MqttClientState {
    Handshake,
    Connected,
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectMethod {
    ForeverDisconnect,
    ReconnectAfter(Duration),
}

pub struct Proxy {
    addr: SocketAddr,
    opts: ClientOptions,
    stream: Option<TcpStream>,
    session_present: bool,
    subscribe_recv: Receiver<Vec<(TopicFilter, QualityOfService)>>,
    message_send: Sender<Message>,
}

pub struct ProxyClient {
    addr: SocketAddr,
    state: MqttClientState,
    opts: ClientOptions,
    stream: Option<TcpStream>,
    session_present: bool,
    last_flush: Instant,
    await_ping: bool,

    // Queues
    incomming_pub: VecDeque<Box<Message>>, // QoS 1
    incomming_rec: VecDeque<Box<Message>>, // QoS 2
    incomming_rel: VecDeque<PacketIdentifier>, // QoS 2
    outgoing_ack: VecDeque<Box<Message>>, // QoS 1
    outgoing_rec: VecDeque<Box<Message>>, // QoS 2
    outgoing_comp: VecDeque<PacketIdentifier>, // QoS 2
}

pub struct Publisher {

}

pub struct Subscriber {
    subscribe_send: Sender<Vec<(TopicFilter, QualityOfService)>>,
    message_recv: Receiver<Message>,
}

impl Subscriber {
    pub fn subscribe(&self, topics: Vec<(TopicFilter, QualityOfService)>) {
        debug!("---> Subscribing");
        self.subscribe_send.send(topics);
    }

    pub fn receive(&self) -> Result<Message> {
        debug!("Receive message wait <---");
        let message = try!(self.message_recv.recv());
        Ok(message)
    }
}

impl Proxy {
    pub fn await(self) {
        let mut proxy_client = ProxyClient {
            addr: self.addr,
            state: MqttClientState::Disconnected,
            opts: self.opts.clone(),
            stream: None,
            session_present: self.session_present,
            last_flush: Instant::now(),
            await_ping: false,
            // Queues
            incomming_pub: VecDeque::new(),
            incomming_rec: VecDeque::new(),
            incomming_rel: VecDeque::new(),
            outgoing_ack: VecDeque::new(),
            outgoing_rec: VecDeque::new(),
            outgoing_comp: VecDeque::new(),
        };

        let subscribe_recv = self.subscribe_recv;
        let message_send = self.message_send;

        mioco::start(move || {
            let addr = proxy_client.addr;
            let mut stream = proxy_client._reconnect(addr).unwrap();
            proxy_client.stream = Some(stream.try_clone().unwrap());

            // Mqtt connect packet send + connack packet await
            match proxy_client._handshake() {
                Ok(_) => (),
                Err(e) => return Err(e),
            };

            let mut pingreq_timer = Timer::new();
            //let mut retry_timer = Timer::new();
            loop {
                pingreq_timer.set_timeout(proxy_client.opts.keep_alive.unwrap() as i64 * 1000);
                //retry_timer.set_timeout(10 * 1000); 
                select!(
                    r:pingreq_timer => {
                            info!("@PING REQ");
                            if !proxy_client.await_ping {
                                let _ = proxy_client.ping();
                            } else {
                                panic!("awaiting for previous ping resp");
                            }
                        },

                        r:stream => {
                            let packet = match VariablePacket::decode(&mut stream) {
                                Ok(pk) => pk,
                                Err(err) => {
                                    // maybe size=0 while reading indicating socket close at broker end
                                    error!("Error in receiving packet {:?}", err);
                                    continue;
                                }
                            };

                            trace!("PACKET {:?}", packet);
                            match proxy_client.handle_packet(&packet){
                                Ok(message) => {
                                    if let Some(m) = message {
                                        message_send.send(*m);
                                    }
                                },
                                Err(err) => panic!("error in handling packet. {:?}", err),         
                            };
                        },

                        // r:retry_timer => {  // TODO: Why isn't this working?
                        //     info!("@PUBLIST RETRY");
                        // },
                        
                        r:subscribe_recv => {
                            info!("@SUBSCRIBE REQUEST");
                            if let Ok(topics) = subscribe_recv.try_recv(){
                                info!("request = {:?}", topics);
                                proxy_client._subscribe(topics);
                            }
                        },
                );
            } //loop end
            Ok(())
        }); //mioco end
    }
}


impl ProxyClient {
    fn handle_packet(&mut self, packet: &VariablePacket) -> Result<Option<Box<Message>>> {
        match packet {
            &VariablePacket::ConnackPacket(ref pubrec) => {Ok(None)}

            &VariablePacket::SubackPacket(ref ack) => {
                if ack.packet_identifier() != 10 {
                    error!("SUBACK packet identifier not match");
                } else {
                    println!("Subscribed!");
                }

                Ok(None)
            }

            &VariablePacket::PingrespPacket(..) => {
                self.await_ping = false;
                Ok(None)
            }

            /// Receives disconnect packet
            &VariablePacket::DisconnectPacket(..) => {
                // TODO
                Ok(None)
            }

            /// Receives puback packet and verifies it with sub packet id
            &VariablePacket::PubackPacket(ref ack) => {
                let pkid = ack.packet_identifier();

                // let mut connection = self.connection.lock().unwrap();
                // let ref mut publish_queue = connection.queue;

                // let mut split_index: Option<usize> = None;
                // for (i, v) in publish_queue.iter().enumerate() {
                //     if v.pkid == pkid {
                //         split_index = Some(i);
                //     }
                // }

                // if split_index.is_some() {
                //     let split_index = split_index.unwrap();
                //     let mut list2 = publish_queue.split_off(split_index);
                //     list2.pop_front();
                //     publish_queue.append(&mut list2);
                // }
                // println!("pub ack for {}. queue --> {:?}",
                //         ack.packet_identifier(),
                //         publish_queue);

                Ok(None)
            }

            /// Receives publish packet
            &VariablePacket::PublishPacket(ref publ) => {
                // let msg = match str::from_utf8(&publ.payload()[..]) {
                //     Ok(msg) => msg,
                //     Err(err) => {
                //         error!("Failed to decode publish message {:?}", err);
                //         return;
                //     }
                // };
                let message = try!(Message::from_pub(publ));
                self._handle_message(message)
            }

            &VariablePacket::PubrecPacket(ref pubrec) => {Ok(None)}

            &VariablePacket::PubrelPacket(ref pubrel) => {Ok(None)}

            &VariablePacket::PubcompPacket(ref pubcomp) => {Ok(None)}

            &VariablePacket::UnsubackPacket(ref pubrec) => {Ok(None)}

            _ => {Ok(None)} //TODO: Replace this with panic later
        }
    }

    fn _handle_message(&mut self, message: Box<Message>) -> Result<Option<Box<Message>>> {
        debug!("       Publish {:?} {:?} < {:?} bytes",
               message.qos,
               message.topic.to_string(),
               message.payload.len());
        match message.qos {
            QoSWithPacketIdentifier::Level0 => Ok(Some(message)),
            QoSWithPacketIdentifier::Level1(_) => {
                Ok(Some(message))
            }
            QoSWithPacketIdentifier::Level2(_) => {
                Ok(None)
            }
        }
    }

    fn _reconnect(&mut self, addr: SocketAddr) -> Result<TcpStream> {
        // Raw tcp connect
        let stream = try!(TcpStream::connect(&addr));
        Ok(stream)
    }


    fn _handshake(&mut self) -> Result<()> {
        self.state = MqttClientState::Handshake;
        // send CONNECT
        try!(self._connect());

        // wait CONNACK
        let stream = match self.stream {
            Some(ref mut s) => s,
            None => return Err(Error::NoStreamError),
        };
        let connack = ConnackPacket::decode(stream).unwrap();
        trace!("CONNACK {:?}", connack);

        if connack.connect_return_code() != ConnectReturnCode::ConnectionAccepted {
            panic!("Failed to connect to server, return code {:?}",
                   connack.connect_return_code());
        } else {
            self.state = MqttClientState::Connected;
        }

        Ok(())
    }

    fn _connect(&mut self) -> Result<()> {
        let connect = try!(self._generate_connect_packet());
        try!(self._write_packet(connect));
        self._flush()
    }

    fn ping(&mut self) -> Result<()> {
        debug!("---> Pingreq");
        let ping = try!(self._generate_pingreq_packet());
        self.await_ping = true;
        try!(self._write_packet(ping));
        self._flush()
    }

    fn _subscribe(&mut self, topics: Vec<(TopicFilter, QualityOfService)>) -> Result<()> {
        debug!("---> Subscribe");
        let subscribe_packet = try!(self._generate_subscribe_packet(topics));
        try!(self._write_packet(subscribe_packet));
        self._flush()
        //TODO: sync wait for suback here
    }

    fn _flush(&mut self) -> Result<()> {
        // TODO: in case of disconnection, trying to reconnect
        let stream = match self.stream {
            Some(ref mut s) => s,
            None => return Err(Error::NoStreamError),
        };

        try!(stream.flush());
        self.last_flush = Instant::now();
        Ok(())
    }

    #[inline]
    fn _write_packet(&mut self, packet: Vec<u8>) -> Result<()> {
        trace!("{:?}", packet);
        let stream = match self.stream {
            Some(ref mut s) => s,
            None => return Err(Error::NoStreamError),
        };

        stream.write_all(&packet).unwrap();
        Ok(())
    }

    fn _generate_connect_packet(&self) -> Result<Vec<u8>> {
        let mut connect_packet = ConnectPacket::new("MQTT".to_owned(),
                                                    self.opts.client_id.clone().unwrap());
        connect_packet.set_clean_session(self.opts.clean_session);
        connect_packet.set_keep_alive(self.opts.keep_alive.unwrap());

        let mut buf = Vec::new();
        match connect_packet.encode(&mut buf) {
            Ok(result) => result,
            Err(_) => {
                return Err(Error::MqttEncodeError);
            }
        };
        Ok(buf)
    }

    fn _generate_pingreq_packet(&self) -> Result<Vec<u8>> {
        let pingreq_packet = PingreqPacket::new();
        let mut buf = Vec::new();

        pingreq_packet.encode(&mut buf).unwrap();
        match pingreq_packet.encode(&mut buf) {
            Ok(result) => result,
            Err(_) => {
                return Err(Error::MqttEncodeError);
            }
        };
        Ok(buf)
    }

    fn _generate_subscribe_packet(&self,
                                  topics: Vec<(TopicFilter, QualityOfService)>)
                                  -> Result<Vec<u8>> {
        let subscribe_packet = SubscribePacket::new(11, topics);
        let mut buf = Vec::new();

        subscribe_packet.encode(&mut buf).unwrap();
        match subscribe_packet.encode(&mut buf) {
            Ok(result) => result,
            Err(_) => {
                return Err(Error::MqttEncodeError);
            }
        };
        Ok(buf)
    }
}