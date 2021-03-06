/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! A thread that takes a URL and streams back the binary data.
use connector::{Connector, create_http_connector};
use content_blocker::BLOCKED_CONTENT_RULES;
use cookie;
use cookie_rs;
use cookie_storage::CookieStorage;
use devtools_traits::DevtoolsControlMsg;
use fetch::methods::{FetchContext, fetch};
use filemanager_thread::{FileManager, TFDProvider};
use hsts::HstsList;
use http_loader::HttpState;
use hyper::client::pool::Pool;
use hyper::header::{ContentType, Header, SetCookie};
use hyper::mime::{Mime, SubLevel, TopLevel};
use hyper_serde::Serde;
use ipc_channel::ipc::{self, IpcReceiver, IpcReceiverSet, IpcSender};
use mime_classifier::{ApacheBugFlag, MimeClassifier, NoSniffFlag};
use net_traits::{CookieSource, CoreResourceThread, Metadata, ProgressMsg};
use net_traits::{CoreResourceMsg, FetchResponseMsg, FetchTaskTarget, LoadConsumer};
use net_traits::{CustomResponseMediator, LoadResponse, NetworkError, ResourceId};
use net_traits::{ResourceThreads, WebSocketCommunicate, WebSocketConnectData};
use net_traits::LoadContext;
use net_traits::ProgressMsg::Done;
use net_traits::request::{Request, RequestInit};
use net_traits::storage_thread::StorageThreadMsg;
use profile_traits::time::ProfilerChan;
use rustc_serialize::{Decodable, Encodable};
use rustc_serialize::json;
use servo_url::ServoUrl;
use std::borrow::{Cow, ToOwned};
use std::collections::HashMap;
use std::error::Error;
use std::fs::File;
use std::io::prelude::*;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, RwLock};
use std::sync::mpsc::Sender;
use storage_thread::StorageThreadFactory;
use util::prefs::PREFS;
use util::thread::spawn_named;
use websocket_loader;

const TFD_PROVIDER: &'static TFDProvider = &TFDProvider;

pub enum ProgressSender {
    Channel(IpcSender<ProgressMsg>),
}

#[derive(Clone)]
pub struct ResourceGroup {
    cookie_jar: Arc<RwLock<CookieStorage>>,
    auth_cache: Arc<RwLock<AuthCache>>,
    hsts_list: Arc<RwLock<HstsList>>,
    connector: Arc<Pool<Connector>>,
}

impl ProgressSender {
    //XXXjdm return actual error
    pub fn send(&self, msg: ProgressMsg) -> Result<(), ()> {
        match *self {
            ProgressSender::Channel(ref c) => c.send(msg).map_err(|_| ()),
        }
    }
}

pub fn send_error(url: ServoUrl, err: NetworkError, start_chan: LoadConsumer) {
    let mut metadata: Metadata = Metadata::default(url);
    metadata.status = None;

    if let Ok(p) = start_sending_opt(start_chan, metadata) {
        p.send(Done(Err(err))).unwrap();
    }
}

/// For use by loaders in responding to a Load message that allows content sniffing.
pub fn start_sending_sniffed_opt(start_chan: LoadConsumer, mut metadata: Metadata,
                                 classifier: Arc<MimeClassifier>, partial_body: &[u8],
                                 context: LoadContext)
                                 -> Result<ProgressSender, ()> {
    if PREFS.get("network.mime.sniff").as_boolean().unwrap_or(false) {
        // TODO: should be calculated in the resource loader, from pull requeset #4094
        let mut no_sniff = NoSniffFlag::Off;
        let mut check_for_apache_bug = ApacheBugFlag::Off;

        if let Some(ref headers) = metadata.headers {
            if let Some(ref content_type) = headers.get_raw("content-type").and_then(|c| c.last()) {
                check_for_apache_bug = ApacheBugFlag::from_content_type(content_type)
            }
            if let Some(ref raw_content_type_options) = headers.get_raw("X-content-type-options") {
                if raw_content_type_options.iter().any(|ref opt| *opt == b"nosniff") {
                    no_sniff = NoSniffFlag::On
                }
            }
        }

        let supplied_type =
            metadata.content_type.as_ref().map(|&Serde(ContentType(Mime(ref toplevel, ref sublevel, _)))| {
            (toplevel.to_owned(), format!("{}", sublevel))
        });
        let (toplevel, sublevel) = classifier.classify(context,
                                                       no_sniff,
                                                       check_for_apache_bug,
                                                       &supplied_type,
                                                       &partial_body);
        let mime_tp: TopLevel = toplevel.into();
        let mime_sb: SubLevel = sublevel.parse().unwrap();
        metadata.content_type =
            Some(Serde(ContentType(Mime(mime_tp, mime_sb, vec![]))));
    }

    start_sending_opt(start_chan, metadata)
}

/// For use by loaders in responding to a Load message.
/// It takes an optional NetworkError, so that we can extract the SSL Validation errors
/// and take it to the HTML parser
fn start_sending_opt(start_chan: LoadConsumer, metadata: Metadata) -> Result<ProgressSender, ()> {
    match start_chan {
        LoadConsumer::Channel(start_chan) => {
            let (progress_chan, progress_port) = ipc::channel().unwrap();
            let result = start_chan.send(LoadResponse {
                metadata: metadata,
                progress_port: progress_port,
            });
            match result {
                Ok(_) => Ok(ProgressSender::Channel(progress_chan)),
                Err(_) => Err(())
            }
        }
    }
}

/// Returns a tuple of (public, private) senders to the new threads.
pub fn new_resource_threads(user_agent: Cow<'static, str>,
                            devtools_chan: Option<Sender<DevtoolsControlMsg>>,
                            profiler_chan: ProfilerChan,
                            config_dir: Option<PathBuf>)
                            -> (ResourceThreads, ResourceThreads) {
    let (public_core, private_core) = new_core_resource_thread(
        user_agent,
        devtools_chan,
        profiler_chan,
        config_dir.clone());
    let storage: IpcSender<StorageThreadMsg> = StorageThreadFactory::new(config_dir);
    (ResourceThreads::new(public_core, storage.clone()),
     ResourceThreads::new(private_core, storage))
}


/// Create a CoreResourceThread
pub fn new_core_resource_thread(user_agent: Cow<'static, str>,
                                devtools_chan: Option<Sender<DevtoolsControlMsg>>,
                                profiler_chan: ProfilerChan,
                                config_dir: Option<PathBuf>)
                                -> (CoreResourceThread, CoreResourceThread) {
    let (public_setup_chan, public_setup_port) = ipc::channel().unwrap();
    let (private_setup_chan, private_setup_port) = ipc::channel().unwrap();
    spawn_named("ResourceManager".to_owned(), move || {
        let resource_manager = CoreResourceManager::new(
            user_agent, devtools_chan, profiler_chan
        );

        let mut channel_manager = ResourceChannelManager {
            resource_manager: resource_manager,
            config_dir: config_dir,
        };
        channel_manager.start(public_setup_port,
                              private_setup_port);
    });
    (public_setup_chan, private_setup_chan)
}

struct ResourceChannelManager {
    resource_manager: CoreResourceManager,
    config_dir: Option<PathBuf>,
}

fn create_resource_groups(config_dir: Option<&Path>)
                          -> (ResourceGroup, ResourceGroup) {
    let mut hsts_list = HstsList::from_servo_preload();
    let mut auth_cache = AuthCache::new();
    let mut cookie_jar = CookieStorage::new(150);
    if let Some(config_dir) = config_dir {
        read_json_from_file(&mut auth_cache, config_dir, "auth_cache.json");
        read_json_from_file(&mut hsts_list, config_dir, "hsts_list.json");
        read_json_from_file(&mut cookie_jar, config_dir, "cookie_jar.json");
    }
    let resource_group = ResourceGroup {
        cookie_jar: Arc::new(RwLock::new(cookie_jar)),
        auth_cache: Arc::new(RwLock::new(auth_cache)),
        hsts_list: Arc::new(RwLock::new(hsts_list.clone())),
        connector: create_http_connector(),
    };
    let private_resource_group = ResourceGroup {
        cookie_jar: Arc::new(RwLock::new(CookieStorage::new(150))),
        auth_cache: Arc::new(RwLock::new(AuthCache::new())),
        hsts_list: Arc::new(RwLock::new(HstsList::new())),
        connector: create_http_connector(),
    };
    (resource_group, private_resource_group)
}

impl ResourceChannelManager {
    #[allow(unsafe_code)]
    fn start(&mut self,
             public_receiver: IpcReceiver<CoreResourceMsg>,
             private_receiver: IpcReceiver<CoreResourceMsg>) {
        let (public_resource_group, private_resource_group) =
            create_resource_groups(self.config_dir.as_ref().map(Deref::deref));

        let mut rx_set = IpcReceiverSet::new().unwrap();
        let private_id = rx_set.add(private_receiver).unwrap();
        let public_id = rx_set.add(public_receiver).unwrap();

        loop {
            for (id, data) in rx_set.select().unwrap().into_iter().map(|m| m.unwrap()) {
                let group = if id == private_id {
                    &private_resource_group
                } else {
                    assert_eq!(id, public_id);
                    &public_resource_group
                };
                if let Ok(msg) = data.to() {
                    if !self.process_msg(msg, group) {
                        return;
                    }
                }
            }
        }
    }

    /// Returns false if the thread should exit.
    fn process_msg(&mut self,
                   msg: CoreResourceMsg,
                   group: &ResourceGroup) -> bool {
        match msg {
            CoreResourceMsg::Fetch(init, sender) =>
                self.resource_manager.fetch(init, sender, group),
            CoreResourceMsg::WebsocketConnect(connect, connect_data) =>
                self.resource_manager.websocket_connect(connect, connect_data, group),
            CoreResourceMsg::SetCookiesForUrl(request, cookie_list, source) =>
                self.resource_manager.set_cookies_for_url(request, cookie_list, source, group),
            CoreResourceMsg::SetCookiesForUrlWithData(request, cookie, source) =>
                self.resource_manager.set_cookies_for_url_with_data(request, cookie, source, group),
            CoreResourceMsg::GetCookiesForUrl(url, consumer, source) => {
                let mut cookie_jar = group.cookie_jar.write().unwrap();
                consumer.send(cookie_jar.cookies_for_url(&url, source)).unwrap();
            }
            CoreResourceMsg::NetworkMediator(mediator_chan) => {
                self.resource_manager.swmanager_chan = Some(mediator_chan)
            }
            CoreResourceMsg::GetCookiesDataForUrl(url, consumer, source) => {
                let mut cookie_jar = group.cookie_jar.write().unwrap();
                let cookies = cookie_jar.cookies_data_for_url(&url, source).map(Serde).collect();
                consumer.send(cookies).unwrap();
            }
            CoreResourceMsg::Cancel(res_id) => {
                if let Some(cancel_sender) = self.resource_manager.cancel_load_map.get(&res_id) {
                    let _ = cancel_sender.send(());
                }
                self.resource_manager.cancel_load_map.remove(&res_id);
            }
            CoreResourceMsg::Synchronize(sender) => {
                let _ = sender.send(());
            }
            CoreResourceMsg::ToFileManager(msg) => self.resource_manager.filemanager.handle(msg, TFD_PROVIDER),
            CoreResourceMsg::Exit(sender) => {
                if let Some(ref config_dir) = self.config_dir {
                    match group.auth_cache.read() {
                        Ok(auth_cache) => write_json_to_file(&*auth_cache, config_dir, "auth_cache.json"),
                        Err(_) => warn!("Error writing auth cache to disk"),
                    }
                    match group.cookie_jar.read() {
                        Ok(jar) => write_json_to_file(&*jar, config_dir, "cookie_jar.json"),
                        Err(_) => warn!("Error writing cookie jar to disk"),
                    }
                    match group.hsts_list.read() {
                        Ok(hsts) => write_json_to_file(&*hsts, config_dir, "hsts_list.json"),
                        Err(_) => warn!("Error writing hsts list to disk"),
                    }
                }
                let _ = sender.send(());
                return false;
            }
        }
        true
    }
}

pub fn read_json_from_file<T>(data: &mut T, config_dir: &Path, filename: &str)
    where T: Decodable
{
    let path = config_dir.join(filename);
    let display = path.display();

    let mut file = match File::open(&path) {
        Err(why) => {
            warn!("couldn't open {}: {}", display, Error::description(&why));
            return;
        },
        Ok(file) => file,
    };

    let mut string_buffer: String = String::new();
    match file.read_to_string(&mut string_buffer) {
        Err(why) => {
            panic!("couldn't read from {}: {}", display,
                                                Error::description(&why))
        },
        Ok(_) => println!("successfully read from {}", display),
    }

    match json::decode(&string_buffer) {
        Ok(decoded_buffer) => *data = decoded_buffer,
        Err(why) => warn!("Could not decode buffer{}", why),
    }
}

pub fn write_json_to_file<T>(data: &T, config_dir: &Path, filename: &str)
    where T: Encodable
{
    let json_encoded: String;
    match json::encode(&data) {
        Ok(d) => json_encoded = d,
        Err(_) => return,
    }
    let path = config_dir.join(filename);
    let display = path.display();

    let mut file = match File::create(&path) {
        Err(why) => panic!("couldn't create {}: {}",
                           display,
                           Error::description(&why)),
        Ok(file) => file,
    };

    match file.write_all(json_encoded.as_bytes()) {
        Err(why) => {
            panic!("couldn't write to {}: {}", display,
                                               Error::description(&why))
        },
        Ok(_) => println!("successfully wrote to {}", display),
    }
}

#[derive(RustcDecodable, RustcEncodable, Clone)]
pub struct AuthCacheEntry {
    pub user_name: String,
    pub password: String,
}

impl AuthCache {
    pub fn new() -> AuthCache {
        AuthCache {
            version: 1,
            entries: HashMap::new()
        }
    }
}

#[derive(RustcDecodable, RustcEncodable, Clone)]
pub struct AuthCache {
    pub version: u32,
    pub entries: HashMap<String, AuthCacheEntry>,
}

pub struct CoreResourceManager {
    user_agent: Cow<'static, str>,
    devtools_chan: Option<Sender<DevtoolsControlMsg>>,
    swmanager_chan: Option<IpcSender<CustomResponseMediator>>,
    filemanager: FileManager,
    cancel_load_map: HashMap<ResourceId, Sender<()>>,
}

impl CoreResourceManager {
    pub fn new(user_agent: Cow<'static, str>,
               devtools_channel: Option<Sender<DevtoolsControlMsg>>,
               _profiler_chan: ProfilerChan) -> CoreResourceManager {
        CoreResourceManager {
            user_agent: user_agent,
            devtools_chan: devtools_channel,
            swmanager_chan: None,
            filemanager: FileManager::new(),
            cancel_load_map: HashMap::new(),
        }
    }

    fn set_cookies_for_url(&mut self,
                           request: ServoUrl,
                           cookie_list: String,
                           source: CookieSource,
                           resource_group: &ResourceGroup) {
        let header = Header::parse_header(&[cookie_list.into_bytes()]);
        if let Ok(SetCookie(cookies)) = header {
            for bare_cookie in cookies {
                if let Some(cookie) = cookie::Cookie::new_wrapped(bare_cookie, &request, source) {
                    let mut cookie_jar = resource_group.cookie_jar.write().unwrap();
                    cookie_jar.push(cookie, source);
                }
            }
        }
    }

    fn set_cookies_for_url_with_data(&mut self, request: ServoUrl, cookie: cookie_rs::Cookie, source: CookieSource,
                                     resource_group: &ResourceGroup) {
        if let Some(cookie) = cookie::Cookie::new_wrapped(cookie, &request, source) {
            let mut cookie_jar = resource_group.cookie_jar.write().unwrap();
            cookie_jar.push(cookie, source)
        }
    }

    fn fetch(&self,
             init: RequestInit,
             sender: IpcSender<FetchResponseMsg>,
             group: &ResourceGroup) {
        let http_state = HttpState {
            hsts_list: group.hsts_list.clone(),
            cookie_jar: group.cookie_jar.clone(),
            auth_cache: group.auth_cache.clone(),
            blocked_content: BLOCKED_CONTENT_RULES.clone(),
        };
        let ua = self.user_agent.clone();
        let dc = self.devtools_chan.clone();
        let filemanager = self.filemanager.clone();
        spawn_named(format!("fetch thread for {}", init.url), move || {
            let request = Request::from_init(init);
            // XXXManishearth: Check origin against pipeline id (also ensure that the mode is allowed)
            // todo load context / mimesniff in fetch
            // todo referrer policy?
            // todo service worker stuff
            let mut target = Some(Box::new(sender) as Box<FetchTaskTarget + Send + 'static>);
            let context = FetchContext {
                state: http_state,
                user_agent: ua,
                devtools_chan: dc,
                filemanager: filemanager,
            };
            fetch(Rc::new(request), &mut target, &context);
        })
    }

    fn websocket_connect(&self,
                         connect: WebSocketCommunicate,
                         connect_data: WebSocketConnectData,
                         resource_grp: &ResourceGroup) {
        websocket_loader::init(connect, connect_data, resource_grp.cookie_jar.clone());
    }
}
