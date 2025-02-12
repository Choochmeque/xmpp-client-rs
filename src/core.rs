/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */
 use std::any::TypeId;
 use std::collections::{HashMap, VecDeque};
 use std::convert::{TryFrom, TryInto};
 use std::fmt::{self, Debug, Display};
 use std::future::Future;
 use std::str::FromStr;
 use std::sync::{Arc, Mutex};
 use std::time::Instant;
 
 use anyhow::Result;
 use chrono::{DateTime, FixedOffset};
 use futures::sink::SinkExt;
 use futures::stream::StreamExt;
 use rand::distr::Distribution;
 use rand::Rng;
 use secrecy::ExposeSecret;
 use tokio::runtime::Runtime as TokioRuntime;
 use tokio::sync::{mpsc, RwLock, RwLockMappedWriteGuard, RwLockReadGuard, RwLockWriteGuard};
 use uuid::Uuid;
 
 use xmpp_parsers::caps::{self, Caps};
 use xmpp_parsers::delay::Delay;
 use xmpp_parsers::hashes as xmpp_hashes;
 use xmpp_parsers::iq::{Iq, IqType};
 use xmpp_parsers::legacy_omemo;
 use xmpp_parsers::message::Message as XmppParsersMessage;
 use xmpp_parsers::muc::Muc;
 use xmpp_parsers::presence::{Presence, Show as PresenceShow, Type as PresenceType};
 use xmpp_parsers::pubsub::event::PubSubEvent;
 use xmpp_parsers::stanza_error::StanzaError;
 use xmpp_parsers::{iq, presence};
 use jid::{BareJid, FullJid, Jid};
 use minidom::Element;

 use crate::account::{Account, ConnectionInfo, Password};
 use crate::async_iq::{IqFuture, PendingIqState};
 use crate::config::Config;
 use crate::crypto::CryptoEngine;
 use crate::mods;
 use crate::message::Message;
 
 const VERSION: &str = env!("CARGO_PKG_VERSION");
 
 #[derive(Debug, Clone)]
 pub enum Event {
     Start,
     Connect(ConnectionInfo, Password),
     Connected(Account, Jid),
     Disconnected(Account, String),
     AuthError(Account, String),
     Stanza(Account, Element),
     RawMessage {
         account: Account,
         message: XmppParsersMessage,
         delay: Option<Delay>,
         archive: bool,
     },
     SendMessage(Account, Message),
     Message(Option<Account>, Message),
     Chat {
         account: Account,
         contact: BareJid,
     },
     Join {
         account: FullJid,
         channel: Jid,
         user_request: bool,
     },
     Joined {
         account: FullJid,
         channel: FullJid,
         user_request: bool,
     },
     Iq(Account, iq::Iq),
     IqResult {
         account: Account,
         uuid: Uuid,
         from: Option<Jid>,
         payload: Option<Element>,
     },
     IqError {
         account: Account,
         uuid: Uuid,
         from: Option<Jid>,
         payload: StanzaError,
     },
     Disco(Account, Vec<String>),
     PubSub {
         account: Account,
         from: Option<Jid>,
         event: PubSubEvent,
     },
     Presence(Account, presence::Presence),
     Win(String),
     Close(String),
     WindowChange,
     LoadChannelHistory {
         account: Account,
         jid: BareJid,
         from: Option<DateTime<FixedOffset>>,
     },
     LoadChatHistory {
         account: Account,
         contact: BareJid,
         from: Option<DateTime<FixedOffset>>,
     },
     Quit,
     ResetCompletion,
     ChangeWindow(String),
     Subject(Account, Jid, HashMap<String, String>),
 }
 
 pub enum Mod {
     Disco(mods::disco::DiscoMod),
 }
 
 macro_rules! from_mod {
     ($enum:ident, $type:path) => {
         impl<'a> From<&'a Mod> for &'a $type {
             fn from(r#mod: &'a Mod) -> &'a $type {
                 match r#mod {
                     Mod::$enum(r#mod) => r#mod,
                     _ => unreachable!(),
                 }
             }
         }
 
         impl<'a> From<&'a mut Mod> for &'a mut $type {
             fn from(r#mod: &'a mut Mod) -> &'a mut $type {
                 match r#mod {
                     Mod::$enum(r#mod) => r#mod,
                     _ => unreachable!(),
                 }
             }
         }
     };
 }
 
 from_mod!(Disco, mods::disco::DiscoMod);
 
 pub trait ModTrait: Display {
     fn init(&mut self, aparte: &mut Aparte) -> Result<(), ()>;
     fn on_event(&mut self, aparte: &mut Aparte, event: &Event);
     /// Return weither this message can be handled
     /// 0 means no, 1 mean definitely yes
     fn can_handle_xmpp_message(
         &mut self,
         _aparte: &mut Aparte,
         _account: &Account,
         _message: &XmppParsersMessage,
         _delay: &Option<Delay>,
     ) -> f64 {
         0f64
     }
 
     /// Handle message
     fn handle_xmpp_message(
         &mut self,
         _aparte: &mut Aparte,
         _account: &Account,
         _message: &XmppParsersMessage,
         _delay: &Option<Delay>,
         _archive: bool,
     ) {
     }
 }
 
 impl ModTrait for Mod {
     fn init(&mut self, aparte: &mut Aparte) -> Result<(), ()> {
         match self {
             Mod::Disco(r#mod) => r#mod.init(aparte),
         }
     }
 
     fn on_event(&mut self, aparte: &mut Aparte, event: &Event) {
         match self {
             Mod::Disco(r#mod) => r#mod.on_event(aparte, event),
         }
     }
 
     fn can_handle_xmpp_message(
         &mut self,
         aparte: &mut Aparte,
         account: &Account,
         message: &XmppParsersMessage,
         delay: &Option<Delay>,
     ) -> f64 {
         match self {
             Mod::Disco(r#mod) => r#mod.can_handle_xmpp_message(aparte, account, message, delay),
         }
     }
 
     fn handle_xmpp_message(
         &mut self,
         aparte: &mut Aparte,
         account: &Account,
         message: &XmppParsersMessage,
         delay: &Option<Delay>,
         archive: bool,
     ) {
         match self {
             Mod::Disco(r#mod) => {
                 r#mod.handle_xmpp_message(aparte, account, message, delay, archive)
             }
         }
     }
 }
 
 impl fmt::Debug for Mod {
     fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
         match self {
             Mod::Disco(_) => f.write_str("Mod::Disco"),
         }
     }
 }
 
 impl Display for Mod {
     fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
         match self {
             Mod::Disco(r#mod) => r#mod.fmt(f),
         }
     }
 }
 
 pub struct Connection {
     pub sink: mpsc::UnboundedSender<Element>,
 }
 
 #[macro_export]
 macro_rules! info(
     ($aparte:ident, $msg:literal, $($args: tt)*) => ({
         ::log::info!($msg, $($args)*);
         $aparte.log(format!($msg, $($args)*))
     });
     ($aparte:ident, $msg:literal) => ({
         ::log::info!($msg);
         $aparte.log(format!($msg))
     });
 );
 
 #[macro_export]
 macro_rules! error(
     ($aparte:ident, $err:ident, $msg:literal, $($args: tt)*) => ({
         let context = format!($msg, $($args)*);
         ::log::error!("{:?}", $err.context(context.clone()));
         $aparte.log(context)
     });
     ($aparte:ident, $err:ident, $msg:literal) => ({
         let context = format!($msg);
         ::log::error!("{:?}", $err.context(context.clone()));
         $aparte.log(context)
     });
 );
 
 pub struct Aparte {
     mods: Arc<HashMap<TypeId, RwLock<Mod>>>,
     connections: HashMap<Account, Connection>,
     current_connection: Option<Account>,
     event_tx: mpsc::UnboundedSender<Event>,
     event_rx: Option<mpsc::UnboundedReceiver<Event>>,
     send_tx: mpsc::UnboundedSender<(Account, Element)>,
     send_rx: Option<mpsc::UnboundedReceiver<(Account, Element)>>,
     pending_iq: Arc<Mutex<HashMap<Uuid, PendingIqState>>>,
     crypto_engines: Arc<Mutex<HashMap<(Account, BareJid), CryptoEngine>>>,
     /// Aparté main configuration
     pub config: Config,
 }
 
 impl Aparte {
     pub fn new(config: Config) -> Result<Self> {
         log::debug!("Loading aparté");
 
         let (event_tx, event_rx) = mpsc::unbounded_channel();
         let (send_tx, send_rx) = mpsc::unbounded_channel();
 
         let aparte = Self {
             mods: Arc::new(HashMap::new()),
             connections: HashMap::new(),
             current_connection: None,
             event_tx,
             event_rx: Some(event_rx),
             send_tx,
             send_rx: Some(send_rx),
             config: config.clone(),
             pending_iq: Arc::new(Mutex::new(HashMap::new())),
             crypto_engines: Arc::new(Mutex::new(HashMap::new())),
         };
 
         Ok(aparte)
     }
 
     pub fn add_mod(&mut self, r#mod: Mod) {
         log::info!("Add mod `{}`", r#mod);
         let mods = Arc::get_mut(&mut self.mods).unwrap();
         // TODO ensure mod is not inserted twice
         match r#mod {
             Mod::Disco(r#mod) => {
                 mods.insert(
                     TypeId::of::<mods::disco::DiscoMod>(),
                     RwLock::new(Mod::Disco(r#mod)),
                 );
             }
         }
     }
 
     pub fn add_connection(&mut self, account: Account, sink: mpsc::UnboundedSender<Element>) {
         let connection = Connection { sink };
 
         self.connections.insert(account.clone(), connection);
         self.current_connection = Some(account);
     }
 
     pub fn init(&mut self) -> Result<(), ()> { 
         let mods = self.mods.clone();
         for (_, r#mod) in mods.iter() {
             r#mod.try_write().unwrap().init(self)?;
         }
 
         Ok(())
     }
 
     pub fn run(mut self) {
         let rt = TokioRuntime::new().unwrap();
 
        //  rt.spawn({
        //      let tx = self.event_tx.clone();
        //      async move {
        //          let mut sigwinch = unix::signal(unix::SignalKind::window_change()).unwrap();
        //          loop {
        //              sigwinch.recv().await;
        //              if let Err(err) = tx.send(Event::WindowChange) {
        //                  log::error!("Cannot send signal to internal channel: {}", err);
        //                  break;
        //              }
        //          }
        //      }
        //  });
 
         let local_set = tokio::task::LocalSet::new();
         local_set.block_on(&rt, async move {
             self.schedule(Event::Start);
             let mut event_rx = self.event_rx.take().unwrap();
             let mut send_rx = self.send_rx.take().unwrap();
 
             let mut last_events = VecDeque::new();
             'main: loop {
                 let mut events_buf = Vec::new();
                 tokio::select! {
                     count = event_rx.recv_many(&mut events_buf, 1000) => match count {
                         0 => {
                             log::error!("Broken event channel");
                             break
                         }
                         events_count => {
                             // Ensure all key events are handled first
                             let (priority_events, filtered_events): (VecDeque<_>, VecDeque<_>) = events_buf.drain(..).partition(|event| matches!(event,
                                                                                                  | Event::ChangeWindow(_)
                                                                                                  | Event::Quit));
 
                             log::trace!("Event loop got {} new events ({} priority); have {} last events", events_count, priority_events.len(), last_events.len());
 
                             last_events.extend(filtered_events);
                             // Handle priority events first
                             for event in priority_events {
                                 if self.handle_event(event).is_err() {
                                     break 'main
                                 }
                             }
                             let mut start = Instant::now();
                             while let Some(event) = last_events.pop_front() {
                                 if self.handle_event(event).is_err() {
                                     break 'main;
                                 }
                             }
                             log::trace!("Event loop, delayed handling of {} events", last_events.len());
                         },
                     },
                     account_and_stanza = send_rx.recv() => match account_and_stanza {
                         Some((account, stanza)) => self.send_stanza(account, stanza),
                         None => {
                             log::error!("Broken send channel");
                             break;
                         }
                     }
                 };
             }
         });
     }
 
    pub async fn run_async(&mut self) {
       self.schedule(Event::Start);
       let mut event_rx = self.event_rx.take().unwrap();
       let mut send_rx = self.send_rx.take().unwrap();

       let mut last_events = VecDeque::new();
       loop {
          let mut events_buf = Vec::new();
          tokio::select! {
             count = event_rx.recv_many(&mut events_buf, 1000) => match count {
                0 => {
                    log::error!("Broken event channel");
                    break
                }
                events_count => {
                    // Ensure all key events are handled first
                    let (priority_events, filtered_events): (VecDeque<_>, VecDeque<_>) = events_buf.drain(..).partition(|event| matches!(event,
                                                                          | Event::ChangeWindow(_)
                                                                          | Event::Quit));

                    log::trace!("Event loop got {} new events ({} priority); have {} last events", events_count, priority_events.len(), last_events.len());

                    last_events.extend(filtered_events);
                    // Handle priority events first
                    for event in priority_events {
                       if self.handle_event(event).is_err() {
                          break
                       }
                    }
                    while let Some(event) = last_events.pop_front() {
                       if self.handle_event(event).is_err() {
                          break;
                       }
                    }
                    log::trace!("Event loop, delayed handling of {} events", last_events.len());
                },
             },
             account_and_stanza = send_rx.recv() => match account_and_stanza {
                Some((account, stanza)) => self.send_stanza(account, stanza),
                None => {
                    log::error!("Broken send channel");
                    break;
                }
             }
          };
       }
    }

     pub fn start(&mut self) {
         for (name, account) in self.config.accounts.clone() {
             if account.autoconnect {
                //self.schedule(Event::Connect(account, account.password.expect("password not set").clone()));
                self.connect(&account, account.password.clone().expect("password not set"));
             }
         }
     }
 
     fn send_stanza(&mut self, account: Account, stanza: Element) {
         let mut raw = Vec::<u8>::new();
         stanza.write_to(&mut raw).unwrap();
         log::debug!("SEND: {}", String::from_utf8(raw).unwrap());
         match self.connections.get_mut(&account) {
             Some(connection) => {
                 if let Err(e) = connection.sink.send(stanza) {
                     log::warn!("Cannot send stanza: {}", e);
                 }
             }
             None => {
                 log::warn!("No connection found for {}", account);
             }
         }
     }
 
     pub fn connect(&mut self, connection_info: &ConnectionInfo, password: Password) {
         let account: Account = match Jid::from_str(&connection_info.jid).map(Jid::try_into_full) {
             Ok(Ok(full_jid)) => full_jid,
             Ok(Err(bare_jid)) => {
                 let rand_string: String = rand::rng()
                     .sample_iter(&rand::distr::Alphanumeric)
                     .take(5)
                     .map(char::from)
                     .collect();
                 bare_jid
                     .with_resource_str(&format!("aparte_{rand_string}"))
                     .unwrap()
             }
             Err(err) => {
                 self.log(format!(
                     "Cannot connect as {}: {}",
                     connection_info.jid, err
                 ));
                 return;
             }
         };
 
         self.log(format!("Connecting as {account}"));
         let config = tokio_xmpp::AsyncConfig {
             jid: Jid::from(account.clone()),
             password: password.expose_secret().to_string(),
             server: match (&connection_info.server, &connection_info.port) {
                 (Some(server), Some(port)) => tokio_xmpp::starttls::ServerConfig::Manual {
                     host: server.clone(),
                     port: *port,
                 },
                 (Some(server), None) => tokio_xmpp::starttls::ServerConfig::Manual {
                     host: server.clone(),
                     port: 5222,
                 },
                 (None, Some(port)) => tokio_xmpp::starttls::ServerConfig::Manual {
                     host: account.domain().to_string(),
                     port: *port,
                 },
                 (None, None) => tokio_xmpp::starttls::ServerConfig::UseSrv,
             },
         };
         log::debug!("Connect with config: {config:?}");
         let mut client = tokio_xmpp::AsyncClient::new_with_config(config);
 
         client.set_reconnect(true);
 
         let (connection_channel, mut rx) = mpsc::unbounded_channel();
 
         self.add_connection(account.clone(), connection_channel);
 
         let (mut writer, mut reader) = client.split();
         // XXX could use self.rt.spawn if client was impl Send
         tokio::spawn(async move {
             while let Some(element) = rx.recv().await {
                 if let Err(err) = writer.send(tokio_xmpp::Packet::Stanza(element)).await {
                     log::error!("cannot send Stanza to internal channel: {}", err);
                     break;
                 }
             }
         });
 
         let event_tx = self.event_tx.clone();
 
         let reconnect = true;
         tokio::spawn(async move {
             while let Some(event) = reader.next().await {
                 log::debug!("XMPP Event: {:?}", event);
                 match event {
                     tokio_xmpp::Event::Disconnected(tokio_xmpp::Error::Auth(e)) => {
                         if let Err(err) =
                             event_tx.send(Event::AuthError(account.clone(), format!("{e}")))
                         {
                             log::error!("Cannot send event to internal channel: {}", err);
                         };
                         break;
                     }
                     tokio_xmpp::Event::Disconnected(e) => {
                         if let Err(err) =
                             event_tx.send(Event::Disconnected(account.clone(), format!("{e}")))
                         {
                             log::error!("Cannot send event to internal channel: {}", err);
                         };
                         if !reconnect {
                             break;
                         }
                     }
                     tokio_xmpp::Event::Online {
                         bound_jid: jid,
                         resumed: true,
                     } => {
                         log::debug!("Reconnected to {}", jid);
                     }
                     tokio_xmpp::Event::Online {
                         bound_jid: jid,
                         resumed: false,
                     } => {
                         if let Err(err) = event_tx.send(Event::Connected(account.clone(), jid)) {
                             log::error!("Cannot send event to internal channel: {}", err);
                             break;
                         }
                     }
                     tokio_xmpp::Event::Stanza(stanza) => {
                         log::debug!("RECV: {}", String::from(&stanza));
                         if let Err(err) = event_tx.send(Event::Stanza(account.clone(), stanza)) {
                             log::error!("Cannot send stanza to internal channel: {}", err);
                             break;
                         }
                     }
                 }
             }
         });
     }
 
     pub fn handle_event(&mut self, event: Event) -> Result<(), ()> {
         log::trace!("Handle event: {:?}", event);

         let before = Instant::now();
 
         {
             let mods = self.mods.clone();
             for (_, r#mod) in mods.iter() {
                 let before = Instant::now();
                 r#mod.try_write().unwrap().on_event(self, &event);
                 log::trace!("{:?} handled event in {:.2?}", r#mod, before.elapsed());
             }
         }
 
         match event {
             Event::Start => {
                 self.start();
             }
             Event::SendMessage(account, message) => {
                 self.schedule(Event::Message(Some(account.clone()), message.clone()));
 
                 // Encrypt if required
                 let encryption = message.encryption_recipient().and_then(|recipient| {
                     let mut crypto_engines = self.crypto_engines.lock().unwrap();
                     crypto_engines
                         .get_mut(&(account.clone(), recipient))
                         .map(|crypto_engine| crypto_engine.encrypt(self, &account, &message))
                 });
 
                 match encryption {
                     Some(Ok(encrypted_message)) => self.send(&account, encrypted_message),
                     Some(Err(e)) => {
                         log::error!("Cannot encrypt message (TODO print error in UI): {e}")
                     }
                     None => self.send(&account, message),
                 }
             }
             Event::Connect(account, password) => {
                 self.connect(&account, password);
             }
             Event::Connected(account, _) => {
                 self.log(format!("Connected as {}", account));
                 let mut presence = Presence::new(PresenceType::None);
                 presence.show = Some(PresenceShow::Chat);
 
                 let disco = self.get_mod::<mods::disco::DiscoMod>().get_disco();
                 let disco = caps::compute_disco(&disco);
                 let verification_string =
                     caps::hash_caps(&disco, xmpp_hashes::Algo::Blake2b_512).unwrap();
                 let caps = Caps::new("aparté", verification_string);
                 presence.add_payload(caps);
 
                 self.send(&account, presence);
             }
             Event::Disconnected(account, err) => {
                 self.log(format!("Connection lost for {}: {}", account, err));
             }
             Event::AuthError(account, err) => {
                 self.log(format!("Authentication error for {}: {}", account, err));
             }
             Event::Stanza(account, stanza) => {
                 self.handle_stanza(account, stanza);
             }
             Event::RawMessage {
                 account,
                 message,
                 delay,
                 archive,
             } => {
                 self.handle_xmpp_message(account, message, delay, archive);
             }
             Event::Join {
                 account,
                 channel,
                 user_request,
             } => {
                 let to = match channel.try_as_full() {
                     Ok(full_jid) => full_jid.clone(),
                     Err(bare_jid) => bare_jid
                         .with_resource_str(account.node().as_ref().unwrap())
                         .unwrap(),
                 };
                 let from: Jid = account.clone().into();
 
                 let mut presence = Presence::new(PresenceType::None);
                 presence = presence.with_to(Jid::from(to.clone()));
                 presence = presence.with_from(from);
                 presence.add_payload(Muc::new());
                 self.send(&account, presence);
 
                 // Successful join
                 self.log(format!("Joined {}", channel));
                 self.schedule(Event::Joined {
                     account: account.clone(),
                     channel: to,
                     user_request,
                 });
             }
             Event::Quit => {
                 return Err(());
             }
             _ => {}
         }
 
         log::trace!("Fully handled event in {:.2?}", before.elapsed());
         Ok(())
     }
 
     fn handle_stanza(&mut self, account: Account, stanza: Element) {
         match stanza.name() {
             "iq" => match Iq::try_from(stanza.clone()) {
                 Ok(iq) => self.handle_iq(account, iq),
                 Err(err) => {
                     log::error!("{}", err);
                     if let Some(id) = stanza.attr("id") {
                         self.errored_iq(id, err.into());
                     }
                 }
             },
             "presence" => match Presence::try_from(stanza) {
                 Ok(presence) => self.schedule(Event::Presence(account, presence)),
                 Err(err) => log::error!("{}", err),
             },
             "message" => match XmppParsersMessage::try_from(stanza) {
                 Ok(message) => self.handle_xmpp_message(account, message, None, false),
                 Err(err) => log::error!("{}", err),
             },
             _ => log::error!("unknown stanza: {}", stanza.name()),
         }
     }
 
     fn handle_xmpp_message(
         &mut self,
         account: Account,
         message: XmppParsersMessage,
         delay: Option<Delay>,
         archive: bool,
     ) {
         let mut best_match = 0f64;
         let mut matched_mod = None;
         let mut message = message;
 
         let encryption_ns = message
             .payloads
             .iter()
             .find_map(|p| {
                 xmpp_parsers::eme::ExplicitMessageEncryption::try_from((*p).clone())
                     .ok()
                     .map(|eme| eme.namespace)
             })
             .or(message.payloads.iter().find_map(|p| {
                 legacy_omemo::Encrypted::try_from((*p).clone())
                     .ok()
                     .map(|_| xmpp_parsers::ns::LEGACY_OMEMO.to_string())
             }));
 
         // Decrypt if required
         // TODO EME can't be required
         if let (Some(encryption_ns), Some(from)) = (encryption_ns, message.from.clone()) {
             let mut crypto_engines = self.crypto_engines.lock().unwrap();
             if let Some(crypto_engine) = crypto_engines.get_mut(&(account.clone(), from.to_bare()))
             {
                 if encryption_ns == crypto_engine.ns() {
                     message = match crypto_engine.decrypt(self, &account, &message) {
                         Ok(message) => message,
                         Err(err) => {
                             log::error!(
                                 "Cannot decrypt message with {}: {}",
                                 crypto_engine.ns(),
                                 err
                             );
                             message
                         }
                     };
                 } else {
                     log::warn!(
                         "Incompatible crypto engine found for {:?} (found {} expecting {})",
                         message.from,
                         crypto_engine.ns(),
                         encryption_ns
                     );
                 }
             } else {
                 log::warn!(
                     "No crypto engine found for {:?} (encrypted with {})",
                     message.from,
                     encryption_ns
                 );
             }
         }
 
         let mods = self.mods.clone();
         for (_, r#mod) in mods.iter() {
             let message_match = r#mod
                 .try_write()
                 .unwrap()
                 .can_handle_xmpp_message(self, &account, &message, &delay);
             if message_match > best_match {
                 matched_mod = Some(r#mod);
                 best_match = message_match;
             }
         }
 
         if let Some(r#mod) = matched_mod {
             log::debug!("Handling xmpp message by {:?}", r#mod);
             r#mod
                 .try_write()
                 .unwrap()
                 .handle_xmpp_message(self, &account, &message, &delay, archive);
         } else {
             log::info!("Don't know how to handle message: {:?}", message);
         }
     }
 
     fn handle_iq(&mut self, account: Account, iq: Iq) {
         // Try to match to pending Iq
         if let Ok(uuid) = Uuid::from_str(&iq.id) {
             let state = self.pending_iq.lock().unwrap().remove(&uuid);
             if let Some(state) = state {
                 match state {
                     PendingIqState::Waiting(waker) => {
                         // XXX dead lock
                         self.pending_iq
                             .lock()
                             .unwrap()
                             .insert(uuid, PendingIqState::Finished(iq));
                         if let Some(waker) = waker {
                             waker.wake();
                         }
                     }
                     PendingIqState::Errored(_err) => {
                         log::info!("Received multiple response for Iq: {}", uuid);
                         // Insert valid iq instead
                         self.pending_iq
                             .lock()
                             .unwrap()
                             .insert(uuid, PendingIqState::Finished(iq));
                     }
                     PendingIqState::Finished(iq) => {
                         log::info!("Received multiple response for Iq: {}", uuid);
                         // Reinsert original result
                         self.pending_iq
                             .lock()
                             .unwrap()
                             .insert(uuid, PendingIqState::Finished(iq));
                     }
                 }
                 return;
             }
         }
 
         // Unknown iq
         match iq.payload {
             IqType::Error(payload) => {
                 if let Some(text) = payload.texts.get("en") {
                     let message = Message::log(text.to_string());
                     self.schedule(Event::Message(Some(account.clone()), message));
                 }
             }
             IqType::Result(payload) => {
                 log::info!("Received unexpected Iq result {:?}", payload);
             }
             _ => {
                 self.schedule(Event::Iq(account, iq));
             }
         }
     }
 
     fn errored_iq(&mut self, id: &str, err: anyhow::Error) {
         if let Ok(uuid) = Uuid::from_str(id) {
             let state = self.pending_iq.lock().unwrap().remove(&uuid);
             if let Some(state) = state {
                 match state {
                     PendingIqState::Waiting(waker) => {
                         // XXX dead lock
                         self.pending_iq
                             .lock()
                             .unwrap()
                             .insert(uuid, PendingIqState::Errored(err));
                         if let Some(waker) = waker {
                             waker.wake();
                         }
                     }
                     PendingIqState::Errored(err) => {
                         log::warn!("Received multiple response for Iq: {}", uuid);
                         // Reinsert original result
                         self.pending_iq
                             .lock()
                             .unwrap()
                             .insert(uuid, PendingIqState::Errored(err));
                     }
                     PendingIqState::Finished(iq) => {
                         log::warn!("Received multiple response for Iq: {}", uuid);
                         // Reinsert original result
                         self.pending_iq
                             .lock()
                             .unwrap()
                             .insert(uuid, PendingIqState::Finished(iq));
                     }
                 }
             }
         }
     }
 
     // TODO maybe use From<>
     pub fn proxy(&self) -> AparteAsync {
         AparteAsync {
             current_connection: self.current_connection.clone(),
             event_tx: self.event_tx.clone(),
             send_tx: self.send_tx.clone(),
             pending_iq: self.pending_iq.clone(),
             config: self.config.clone(),
             crypto_engines: self.crypto_engines.clone(),
         }
     }
 
     pub fn spawn<F>(future: F)
     where
         F: Future + Send + 'static,
         F::Output: Send + 'static,
     {
         tokio::spawn(future);
     }
 
     // Common function for AparteAsync and Aparte, maybe share it in Trait
     pub fn add_crypto_engine(
         &mut self,
         account: &Account,
         recipient: &BareJid,
         crypto_engine: CryptoEngine,
     ) {
         let mut crypto_engines = self.crypto_engines.lock().unwrap();
         crypto_engines.insert((account.clone(), recipient.clone()), crypto_engine);
     }
 
     pub fn send<T>(&mut self, account: &Account, element: T)
     where
         T: TryInto<Element> + Debug,
     {
         match element.try_into() {
             Ok(stanza) => self.send_tx.send((account.clone(), stanza)).unwrap(),
             Err(_e) => {
                 log::error!("Cannot convert to element");
             }
         };
     }
 
     pub fn schedule(&mut self, event: Event) {
         log::trace!("Schedule event {:?}", event);
         self.event_tx.send(event).unwrap();
     }
 
     pub fn log<T: ToString>(&mut self, message: T) {
         let message = Message::log(message.to_string());
         self.schedule(Event::Message(None, message));
     }
 
     pub fn error<T: Display>(&mut self, message: T, err: anyhow::Error) {
         let message = Message::log(format!("{}: {:#}", message, err));
         self.schedule(Event::Message(None, message));
     }
 
     pub fn get_mod<'a, T>(&'a self) -> RwLockReadGuard<'a, T>
     where
         T: 'static,
         for<'b> &'b T: From<&'b Mod>,
     {
         match self.mods.get(&TypeId::of::<T>()) {
             Some(r#mod) => RwLockReadGuard::map(r#mod.try_read().unwrap(), |m| m.into()),
             None => unreachable!(),
         }
     }
 
     #[allow(unused)]
     pub fn get_mod_mut<'a, T>(&'a self) -> RwLockMappedWriteGuard<'a, T>
     where
         T: 'static,
         for<'b> &'b mut T: From<&'b mut Mod>,
     {
         match self.mods.get(&TypeId::of::<T>()) {
             Some(r#mod) => RwLockWriteGuard::map(r#mod.try_write().unwrap(), |m| m.into()),
             None => unreachable!(),
         }
     }
 
     pub fn current_account(&self) -> Option<Account> {
         self.current_connection.clone()
     }
 }
 
 #[derive(Clone)]
 pub struct AparteAsync {
     current_connection: Option<Account>,
     event_tx: mpsc::UnboundedSender<Event>,
     send_tx: mpsc::UnboundedSender<(Account, Element)>,
     crypto_engines: Arc<Mutex<HashMap<(Account, BareJid), CryptoEngine>>>,
     pub(crate) pending_iq: Arc<Mutex<HashMap<Uuid, PendingIqState>>>,
     pub config: Config,
 }
 
 impl AparteAsync {
    pub fn connect(&mut self, connection_info: &ConnectionInfo) {
        let connection_info = connection_info.clone();
        let password = connection_info.password.clone();
        self.schedule(Event::Connect(connection_info, password.unwrap()));
    }

     pub fn send(&mut self, account: &Account, stanza: Element) {
         self.send_tx.send((account.clone(), stanza)).unwrap();
     }
 
     pub fn iq(&mut self, account: &Account, iq: Iq) -> IqFuture {
         IqFuture::new(self.clone(), account, iq)
     }
 
     pub fn schedule(&mut self, event: Event) {
         self.event_tx.send(event).unwrap();
     }
 
     pub fn log<T: ToString>(&mut self, message: T) {
         let message = Message::log(message.to_string());
         self.schedule(Event::Message(None, message));
     }
 
     pub fn error<T: Display>(&mut self, message: T, err: anyhow::Error) {
         let message = Message::log(format!("{}: {:#}", message, err));
         self.schedule(Event::Message(None, message));
     }
 
     pub fn current_account(&self) -> Option<Account> {
         self.current_connection.clone()
     }
 
     pub fn add_crypto_engine(
         &mut self,
         account: &Account,
         recipient: &BareJid,
         crypto_engine: CryptoEngine,
     ) {
         let mut crypto_engines = self.crypto_engines.lock().unwrap();
         crypto_engines.insert((account.clone(), recipient.clone()), crypto_engine);
     }
 }