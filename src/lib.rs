#[macro_use]
extern crate log;

#[macro_use]
extern crate error_chain;

extern crate backtrace;
extern crate time;
extern crate url;

use std::thread;
use std::sync::mpsc::{channel, Sender, Receiver};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::fmt::{self, Debug};
use std::default::Default;
use std::time::Duration;
use std::io::Read;
use std::env;
use std::error::Error;
use std::str::FromStr;
// use std::io::Write;
mod errors;

pub use self::errors::*;

#[macro_use]
extern crate hyper;

use hyper::Client;
use hyper::net::HttpsConnector;
use hyper::header::{Headers, ContentType};

extern crate hyper_native_tls;

use hyper_native_tls::NativeTlsClient;

extern crate chrono;

use chrono::offset::Utc;

struct ThreadState<'a> {
    alive: &'a mut Arc<AtomicBool>,
}

impl<'a> ThreadState<'a> {
    fn set_alive(&self) {
        self.alive.store(true, Ordering::Relaxed);
    }
}

impl<'a> Drop for ThreadState<'a> {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
    }
}

pub trait WorkerClosure<T, P>: Fn(&P, T) -> () + Send + Sync {}

impl<T, F, P> WorkerClosure<T, P> for F where F: Fn(&P, T) -> () + Send + Sync {}


pub struct SingleWorker<T: 'static + Send, P: Clone + Send> {
    parameters: P,
    f: Arc<Box<WorkerClosure<T, P, Output=()>>>,
    receiver: Arc<Mutex<Receiver<T>>>,
    sender: Mutex<Sender<T>>,
    alive: Arc<AtomicBool>,
}

impl<T: 'static + Debug + Send, P: 'static + Clone + Send> SingleWorker<T, P> {
    pub fn new(parameters: P, f: Box<WorkerClosure<T, P, Output=()>>) -> SingleWorker<T, P> {
        let (sender, receiver) = channel::<T>();

        let worker = SingleWorker {
            parameters: parameters,
            f: Arc::new(f),
            receiver: Arc::new(Mutex::new(receiver)),
            sender: Mutex::new(sender),
            /* too bad sender is not sync -- suboptimal.... see https://github.com/rust-lang/rfcs/pull/1299/files */
            alive: Arc::new(AtomicBool::new(true)),
        };
        SingleWorker::spawn_thread(&worker);
        worker
    }

    fn is_alive(&self) -> bool {
        self.alive.clone().load(Ordering::Relaxed)
    }

    fn spawn_thread(worker: &SingleWorker<T, P>) {
        let mut alive = worker.alive.clone();
        let f = worker.f.clone();
        let receiver = worker.receiver.clone();
        let parameters = worker.parameters.clone();
        thread::spawn(move || {
            let state = ThreadState { alive: &mut alive };
            state.set_alive();

            let lock = match receiver.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            loop {
                match lock.recv() {
                    Ok(value) => f(&parameters, value),
                    Err(_) => {
                        thread::yield_now();
                    }
                };
            }
        });
        while !worker.is_alive() {
            thread::yield_now();
        }
    }

    pub fn work_with(&self, msg: T) {
        let alive = self.is_alive();
        if !alive {
            SingleWorker::spawn_thread(self);
        }

        let lock = match self.sender.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        let _ = lock.send(msg);
    }
}


pub trait ToJsonString {
    fn to_json_string(&self) -> String;
}

impl ToJsonString for String {
    // adapted from rustc_serialize json.rs
    fn to_json_string(&self) -> String {
        let mut s = String::new();
        s.push_str("\"");

        let mut start = 0;

        for (i, byte) in self.bytes().enumerate() {
            let escaped = match byte {
                b'"' => "\\\"",
                b'\\' => "\\\\",
                b'\x00' => "\\u0000",
                b'\x01' => "\\u0001",
                b'\x02' => "\\u0002",
                b'\x03' => "\\u0003",
                b'\x04' => "\\u0004",
                b'\x05' => "\\u0005",
                b'\x06' => "\\u0006",
                b'\x07' => "\\u0007",
                b'\x08' => "\\b",
                b'\t' => "\\t",
                b'\n' => "\\n",
                b'\x0b' => "\\u000b",
                b'\x0c' => "\\f",
                b'\r' => "\\r",
                b'\x0e' => "\\u000e",
                b'\x0f' => "\\u000f",
                b'\x10' => "\\u0010",
                b'\x11' => "\\u0011",
                b'\x12' => "\\u0012",
                b'\x13' => "\\u0013",
                b'\x14' => "\\u0014",
                b'\x15' => "\\u0015",
                b'\x16' => "\\u0016",
                b'\x17' => "\\u0017",
                b'\x18' => "\\u0018",
                b'\x19' => "\\u0019",
                b'\x1a' => "\\u001a",
                b'\x1b' => "\\u001b",
                b'\x1c' => "\\u001c",
                b'\x1d' => "\\u001d",
                b'\x1e' => "\\u001e",
                b'\x1f' => "\\u001f",
                b'\x7f' => "\\u007f",
                _ => {
                    continue;
                }
            };


            if start < i {
                s.push_str(&self[start..i]);
            }

            s.push_str(escaped);

            start = i + 1;
        }

        if start != self.len() {
            s.push_str(&self[start..]);
        }

        s.push_str("\"");
        s
    }
}

#[derive(Debug, Clone)]
pub struct StackFrame {
    filename: String,
    function: String,
    lineno: u32,
}

// see https://docs.getsentry.com/hosted/clientdev/attributes/
#[derive(Debug, Clone)]
pub struct Event {
    // required
    event_id: String,
    // uuid4 exactly 32 characters (no dashes!)
    message: String,
    // Maximum length is 1000 characters.
    timestamp: String,
    // ISO 8601 format, without a timezone ex: "2011-05-02T17:41:36"
    level: String,
    // fatal, error, warning, info, debug
    logger: String,
    // ex "my.logger.name"
    platform: String,
    // Acceptable values ..., other
    sdk: SDK,
    device: Device,
    // optional
    culprit: Option<String>,
    // the primary perpetrator of this event ex: "my.module.function_name"
    server_name: Option<String>,
    // host client from which the event was recorded
    stack_trace: Option<Vec<StackFrame>>,
    // stack trace
    release: Option<String>,
    // generally be something along the lines of the git SHA for the given project
    tags: Vec<(String, String)>,
    // WARNING! should be serialized as json object k->v
    environment: Option<String>,
    // ex: "production"
    modules: Vec<(String, String)>,
    // WARNING! should be serialized as json object k->v
    extra: Vec<(String, String)>,
    // WARNING! should be serialized as json object k->v
    fingerprint: Vec<String>,
    // An array of strings used to dictate the deduplicating for this event.
}

impl Event {
    pub fn new(logger: &str,
               level: &str,
               message: &str,
               device: &Device,
               culprit: Option<&str>,
               fingerprint: Option<Vec<String>>,
               server_name: Option<&str>,
               stack_trace: Option<Vec<StackFrame>>,
               release: Option<&str>,
               environment: Option<&str>,
               tags: Option<Vec<(String, String)>>,
               extra: Option<Vec<(String, String)>>,
    )
               -> Event {
        Event {
            event_id: "".to_string(),
            message: message.to_owned(),
            timestamp: Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            /* ISO 8601 format, without a timezone ex: "2011-05-02T17:41:36" */
            level: level.to_owned(),
            logger: logger.to_owned(),
            platform: "other".to_string(),
            sdk: SDK {
                name: "rust-sentry".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            device: device.to_owned(),
            culprit: culprit.map(|c| c.to_owned()),
            server_name: server_name.map(|c| c.to_owned()),
            stack_trace: stack_trace,
            release: release.map(|c| c.to_owned()),
            tags: tags.unwrap_or(vec![]),
            environment: environment.map(|c| c.to_owned()),
            modules: vec![],
            extra: extra.unwrap_or(vec![]),
            fingerprint: fingerprint.unwrap_or(vec![]),
        }
    }

    pub fn push_tag(&mut self, key: String, value: String) {
        self.tags.push((key, value));
    }
}

impl ToJsonString for Event {
    fn to_json_string(&self) -> String {
        let mut s = String::new();
        s.push_str("{");
        s.push_str(&format!("\"event_id\":{},", self.event_id.to_json_string()));
        s.push_str(&format!("\"message\":{},", self.message.to_json_string()));
        s.push_str(&format!("\"timestamp\":\"{}\",", self.timestamp));
        s.push_str(&format!("\"level\":\"{}\",", self.level));
        s.push_str(&format!("\"logger\":\"{}\",", self.logger));
        s.push_str(&format!("\"platform\":\"{}\",", self.platform));
        s.push_str(&format!("\"sdk\": {},", self.sdk.to_json_string()));
        s.push_str(&format!("\"device\": {}", self.device.to_json_string()));

        if let Some(ref culprit) = self.culprit {
            s.push_str(&format!(",\"culprit\": {}", culprit.to_json_string()));
        }
        if let Some(ref server_name) = self.server_name {
            s.push_str(&format!(",\"server_name\":\"{}\"", server_name));
        }
        if let Some(ref release) = self.release {
            s.push_str(&format!(",\"release\":\"{}\"", release));
        }
        if self.tags.len() > 0 {
            let last_index = self.tags.len() - 1;
            s.push_str(",\"tags\":{");
            for (index, tag) in self.tags.iter().enumerate() {
                s.push_str(&format!("\"{}\":\"{}\"", tag.0, tag.1));
                if index != last_index {
                    s.push_str(",");
                }
            }
            s.push_str("}");
        }
        if let Some(ref environment) = self.environment {
            s.push_str(&format!(",\"environment\":\"{}\"", environment));
        }
        if self.modules.len() > 0 {
            s.push_str(",\"modules\":\"{");
            for module in self.modules.iter() {
                s.push_str(&format!("\"{}\":\"{}\"", module.0, module.1));
            }
            s.push_str("}");
        }
        if self.extra.len() > 0 {
            s.push_str(",\"extra\":\"{");
            for extra in self.extra.iter() {
                s.push_str(&format!("\"{}\":\"{}\"", extra.0, extra.1));
            }
            s.push_str("}");
        }
        if let Some(ref stack_trace) = self.stack_trace {
            s.push_str(",\"stacktrace\":{\"frames\":[");
            // push stack frames, starting with the oldest
            let mut is_first = true;
            for stack_frame in stack_trace.iter().rev() {
                if !is_first {
                    s.push_str(",");
                } else {
                    is_first = false;
                }
                s.push_str(&format!("{{\"filename\":\"{}\",\"function\":\"{}\",\"lineno\":{}}}",
                                    stack_frame.filename,
                                    stack_frame.function,
                                    stack_frame.lineno));
            }
            s.push_str("]}");
        }
        if self.fingerprint.len() > 0 {
            s.push_str(",\"fingerprint\":[");
            let mut comma = false;
            for fingerprint in self.fingerprint.iter() {
                if comma {
                    s.push_str(",");
                }
                s.push_str(&fingerprint.to_json_string());
                comma = true;
            }
            s.push_str("]");
        }

        s.push_str("}");
        s
    }
}

#[derive(Debug, Clone)]
pub struct SDK {
    name: String,
    version: String,
}

impl ToJsonString for SDK {
    fn to_json_string(&self) -> String {
        format!("{{\"name\":\"{}\",\"version\":\"{}\"}}",
                self.name,
                self.version)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    name: String,
    version: String,
    build: String,
}

impl Device {
    pub fn new(name: String, version: String, build: String) -> Device {
        Device {
            name: name,
            version: version,
            build: build
        }
    }
}

impl Default for Device {
    fn default() -> Device {
        Device {
            name: env::var_os("OSTYPE")
                .and_then(|cs| cs.into_string().ok())
                .unwrap_or("".to_string()),
            version: "".to_string(),
            build: "".to_string()
        }
    }
}

impl ToJsonString for Device {
    fn to_json_string(&self) -> String {
        format!("{{\"name\":\"{}\",\"version\":\"{}\",\"build\":\"{}\"}}",
                self.name,
                self.version,
                self.build)
    }
}


#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentryCredential {
    pub scheme: String,
    pub key: String,
    pub secret: String,
    pub host: String,
    pub port: String,
    pub project_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialParseError {}

impl fmt::Display for CredentialParseError {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str(self.description())
    }
}

impl Error for CredentialParseError {
    fn description(&self) -> &str {
        "Invalid Sentry DSN syntax. Expected the form `http[s]://{public key}:{private key}@{host}{[:port]}/{project id}`"
    }
}

impl FromStr for SentryCredential {
    type Err = CredentialParseError;
    fn from_str(s: &str) -> std::result::Result<SentryCredential, CredentialParseError> {
        url::Url::parse(s).ok()
            .and_then(|url| {
                let scheme = url.scheme().to_string();
                if !scheme.is_empty() { Some((url, scheme)) } else { None }
            })
            .and_then(|(url, scheme)| {
                let username = url.username().to_string();
                if !username.is_empty() { Some((url, scheme, username)) } else { None }
            })
            .and_then(|(url, scheme, username)| {
                let password = url.password().map(str::to_string);
                password.map(|pw| (url, scheme, username, pw))
            })
            .and_then(|(url, scheme, username, pw)| {
                let host = url.host_str().map(str::to_string);
                host.map(|host| (url, scheme, username, pw, host))
            })
            .and_then(|(url, scheme, username, pw, host)| {
                let port = url.port();
                let parsed_port: String;
                if port == None { parsed_port = "".to_string(); } else { parsed_port = port.unwrap().to_string(); }
                Some((url, scheme, username, pw, host, parsed_port))
            })
            .and_then(|(url, scheme, username, pw, host, port)| {
                url.path_segments()
                    .and_then(|paths| paths.last().map(str::to_string))
                    .and_then(|path| if !path.is_empty() { Some((url, scheme, username, pw, host, port, path)) } else { None })
            })
            .map(|(_, scheme, username, pw, host, port, path)| {
                SentryCredential {
                    scheme: scheme,
                    key: username,
                    secret: pw,
                    host: host,
                    port: port,
                    project_id: path
                }
            })
            .ok_or_else(|| CredentialParseError {})
    }
}

pub struct Sentry {
    settings: Settings,
    worker: Arc<SingleWorker<Event, SentryCredential>>,
}

#[derive(Debug, PartialEq, Default)]
pub struct Settings {
    pub server_name: String,
    pub release: String,
    pub environment: String,
    pub device: Device
}

impl Settings {
    pub fn new(server_name: String, release: String, environment: String, device: Device) -> Settings {
        Settings {
            server_name: server_name,
            release: release,
            environment: environment,
            device: device
        }
    }
}

header! { (XSentryAuth, "X-Sentry-Auth") => [String] }

impl Sentry {
    pub fn new(server_name: String,
               release: String,
               environment: String,
               credential: SentryCredential)
               -> Sentry {
        let settings = Settings {
            server_name: server_name,
            release: release,
            environment: environment,
            ..Settings::default()
        };

        Sentry::from_settings(settings, credential)
    }

    pub fn from_settings(settings: Settings, credential: SentryCredential) -> Sentry {
        let worker = SingleWorker::new(credential,
                                       Box::new(move |credential, e| {
                                           let _ = Sentry::post(credential, &e);
                                       }));
        Sentry {
            settings: settings,
            worker: Arc::new(worker)
        }
    }


    // POST /api/1/store/ HTTP/1.1
    // Content-Type: application/json
    //
    fn post(credential: &SentryCredential, e: &Event) -> Result<()> {
        // writeln!(&mut ::std::io::stderr(), "SENTRY: {}", e.to_json_string());

        let mut headers = Headers::new();

        // X-Sentry-Auth: Sentry sentry_version=7,
        // sentry_client=<client version, arbitrary>,
        // sentry_timestamp=<current timestamp>,
        // sentry_key=<public api key>,
        // sentry_secret=<secret api key>
        //
        let timestamp = time::get_time().sec.to_string();
        let xsentryauth = format!("Sentry sentry_version=7,sentry_client=rust-sentry/{},\
                                   sentry_timestamp={},sentry_key={},sentry_secret={}",
                                  env!("CARGO_PKG_VERSION"),
                                  timestamp,
                                  credential.key,
                                  credential.secret);
        headers.set(XSentryAuth(xsentryauth));


        headers.set(ContentType::json());

        let body = e.to_json_string();
        trace!("Sentry body {}", body);

        let ssl = NativeTlsClient::new().unwrap();
        let connector = HttpsConnector::new(ssl);
        let mut client = Client::with_connector(connector);
        client.set_read_timeout(Some(Duration::new(5, 0)));
        client.set_write_timeout(Some(Duration::new(5, 0)));

        // {PROTOCOL}://{PUBLIC_KEY}:{SECRET_KEY}@{HOST}/{PATH}{PROJECT_ID}/store/
        let url = format!("{}://{}:{}@{}{:}/api/{}/store/",
                          credential.scheme,
                          credential.key,
                          credential.secret,
                          credential.host,
                          credential.port,
                          credential.project_id);

        let mut res = client.post(&url)
            .headers(headers)
            .body(&body)
            .send()?;

        // Read the Response.
        let mut body = String::new();
        res.read_to_string(&mut body)?;
        trace!("Sentry Response {}", body);
        Ok(())
    }

    pub fn log_event(&self, e: Event) {
        self.worker.work_with(e);
    }

    pub fn register_panic_handler<F>(&self, maybe_f: Option<F>)
        where F: Fn(&std::panic::PanicInfo) + 'static + Sync + Send
    {
        let device = self.settings.device.clone();
        let server_name = self.settings.server_name.clone();
        let release = self.settings.release.clone();
        let environment = self.settings.environment.clone();

        let worker = self.worker.clone();

        std::panic::set_hook(Box::new(move |info: &std::panic::PanicInfo| {
            let location = info.location()
                .map(|l| format!("{}: {}", l.file(), l.line()))
                .unwrap_or("NA".to_string());
            let msg = match info.payload().downcast_ref::<&'static str>() {
                Some(s) => *s,
                None => {
                    match info.payload().downcast_ref::<String>() {
                        Some(s) => &s[..],
                        None => "Box<Any>",
                    }
                }
            };

            let mut frames = vec![];
            backtrace::trace(|frame: &backtrace::Frame| {
                backtrace::resolve(frame.ip(), |symbol| {
                    let name = symbol.name()
                        .map_or("unresolved symbol".to_string(), |name| name.to_string());
                    let filename = symbol.filename()
                        .map_or("".to_string(), |sym| sym.to_string_lossy().into_owned());
                    let lineno = symbol.lineno().unwrap_or(0);
                    frames.push(StackFrame {
                        filename: filename,
                        function: name,
                        lineno: lineno,
                    });
                });

                true // keep going to the next frame
            });

            let e = Event::new("panic",
                               "fatal",
                               msg,
                               &device,
                               Some(&location),
                               None,
                               Some(&server_name),
                               Some(frames),
                               Some(&release),
                               Some(&environment),
                               None,
                               None);
            let _ = worker.work_with(e.clone());
            if let Some(ref f) = maybe_f {
                f(info);
            }
        }));
    }
    pub fn unregister_panic_handler(&self) {
        let _ = std::panic::take_hook();
    }

    // fatal, error, warning, info, debug
    pub fn fatal(&self, logger: &str, message: &str, culprit: Option<&str>) {
        self.log(logger, "fatal", message, culprit, None, None, None);
    }
    pub fn error(&self, logger: &str, message: &str, culprit: Option<&str>) {
        self.log(logger, "error", message, culprit, None, None, None);
    }
    pub fn warning(&self, logger: &str, message: &str, culprit: Option<&str>) {
        self.log(logger, "warning", message, culprit, None, None, None);
    }
    pub fn info(&self, logger: &str, message: &str, culprit: Option<&str>) {
        self.log(logger, "info", message, culprit, None, None, None);
    }
    pub fn debug(&self, logger: &str, message: &str, culprit: Option<&str>) {
        self.log(logger, "debug", message, culprit, None, None, None);
    }

    pub fn log(&self,
               logger: &str,
               level: &str,
               message: &str,
               culprit: Option<&str>,
               fingerprint: Option<Vec<String>>,
               tags: Option<Vec<(String, String)>>,
               extra: Option<Vec<(String, String)>>) {
        let fpr = match fingerprint {
            Some(f) => f,
            None => {
                vec![logger.to_string(),
                     level.to_string(),
                     culprit.map(|c| c.to_string()).unwrap_or("".to_string())]
            }
        };
        self.worker.work_with(Event::new(logger,
                                         level,
                                         message,
                                         &self.settings.device,
                                         culprit,
                                         Some(fpr),
                                         Some(&self.settings.server_name),
                                         None,
                                         Some(&self.settings.release),
                                         Some(&self.settings.environment),
                                         tags,
                                         extra));
    }
}

#[cfg(test)]
mod tests {
    use super::{Device, Sentry, SentryCredential, Settings, SingleWorker, ToJsonString};
    use std::sync::{Arc, Mutex};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::channel;
    use std::thread;
    use std::panic::PanicInfo;


    // use std::time::Duration;

    #[test]
    fn test_to_json_string_for_string() {
        let uut1 = "".to_owned();
        assert_eq!(uut1.to_json_string(), "\"\"");

        let uut2 = "plain_string".to_owned();
        assert_eq!(uut2.to_json_string(), "\"plain_string\"");

        let uut3 = "string\"with escapes\"".to_owned();
        assert_eq!(uut3.to_json_string(), "\"string\\\"with escapes\\\"\"");

        let uut4 = "string with null\x00".to_owned();
        assert_eq!(uut4.to_json_string(), "\"string with null\\u0000\"");
    }

    #[test]
    fn it_should_pass_value_to_worker_thread() {
        let (sender, receiver) = channel();
        let s = Mutex::new(sender);
        let worker = SingleWorker::new("",
                                       Box::new(move |_, v| {
                                           let _ = s.lock().unwrap().send(v);
                                       }));
        let v = "Value";
        worker.work_with(v);

        let recv_v = receiver.recv().ok();
        assert!(recv_v == Some(v));
    }

    #[test]
    fn it_should_pass_value_event_after_thread_panic() {
        let (sender, receiver) = channel();
        let s = Mutex::new(sender);
        let i = AtomicUsize::new(0);
        let worker = SingleWorker::new("",
                                       Box::new(move |_, v| {
                                           let lock = match s.lock() {
                                               Ok(guard) => guard,
                                               Err(poisoned) => poisoned.into_inner(),
                                           };
                                           let _ = lock.send(v);

                                           i.fetch_add(1, Ordering::SeqCst);
                                           if i.load(Ordering::Relaxed) == 2 {
                                               panic!("PanicTesting");
                                           }
                                       }));
        let v0 = "Value0";
        let v1 = "Value1";
        let v2 = "Value2";
        let v3 = "Value3";
        worker.work_with(v0);
        worker.work_with(v1);
        let recv_v0 = receiver.recv().ok();
        let recv_v1 = receiver.recv().ok();

        while worker.is_alive() {
            thread::yield_now();
        }

        worker.work_with(v2);
        worker.work_with(v3);
        let recv_v2 = receiver.recv().ok();
        let recv_v3 = receiver.recv().ok();

        assert!(recv_v0 == Some(v0));
        assert!(recv_v1 == Some(v1));
        assert!(recv_v2 == Some(v2));
        assert!(recv_v3 == Some(v3));
    }

    #[test]
    fn it_registrer_panic_handler() {
        let sentry = Sentry::new("Server Name".to_string(),
                                 "release".to_string(),
                                 "test_env".to_string(),
                                 SentryCredential {
                                     scheme: "https".to_string(),
                                     key: "xx".to_string(),
                                     secret: "xx".to_string(),
                                     host: "app.getsentry.com".to_string(),
                                     port: "".to_string(),
                                     project_id: "xx".to_string(),
                                 });

        let (sender, receiver) = channel();
        let s = Mutex::new(sender);

        sentry.register_panic_handler(Some(move |_: &PanicInfo| -> () {
            let lock = match s.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let _ = lock.send(true);
        }));

        let t1 = thread::spawn(|| {
            panic!("Panic Handler Testing");
        });
        let _ = t1.join();


        assert_eq!(receiver.recv().unwrap(), true);
        sentry.unregister_panic_handler();
    }

    #[test]
    fn it_share_sentry_accross_threads() {
        let sentry = Arc::new(Sentry::new("Server Name".to_string(),
                                          "release".to_string(),
                                          "test_env".to_string(),
                                          SentryCredential {
                                              scheme: "https".to_string(),
                                              key: "xx".to_string(),
                                              secret: "xx".to_string(),
                                              host: "app.getsentry.com".to_string(),
                                              port: "".to_string(),
                                              project_id: "xx".to_string(),
                                          }));

        let sentry1 = sentry.clone();
        let t1 = thread::spawn(move || sentry1.settings.server_name.clone());
        let sentry2 = sentry.clone();
        let t2 = thread::spawn(move || sentry2.settings.server_name.clone());

        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();

        assert!(r1 == sentry.settings.server_name);
        assert!(r2 == sentry.settings.server_name);
    }

    #[test]
    fn test_parsing_dsn_when_valid() {
        let parsed_creds: SentryCredential = "https://mypublickey:myprivatekey@myhost/myprojectid".parse().unwrap();
        let manual_creds = SentryCredential {
            scheme: "https".to_string(),
            key: "mypublickey".to_string(),
            secret: "myprivatekey".to_string(),
            host: "myhost".to_string(),
            port: "".to_string(),
            project_id: "myprojectid".to_string()
        };
        assert_eq!(parsed_creds, manual_creds);
    }

    #[test]
    fn test_parsing_dsn_with_nested_project_id() {
        let parsed_creds: SentryCredential = "https://mypublickey:myprivatekey@myhost/foo/bar/myprojectid".parse().unwrap();
        let manual_creds = SentryCredential {
            scheme: "https".to_string(),
            key: "mypublickey".to_string(),
            secret: "myprivatekey".to_string(),
            host: "myhost".to_string(),
            port: "".to_string(),
            project_id: "myprojectid".to_string()
        };
        assert_eq!(parsed_creds, manual_creds);
    }

    #[test]
    fn test_parsing_dsn_when_with_http_unnormal_port() {
        let parsed_creds: SentryCredential = "http://mypublickey:myprivatekey@myhost:444/222".parse().unwrap();
        let manual_creds = SentryCredential {
            scheme: "http".to_string(),
            key: "mypublickey".to_string(),
            secret: "myprivatekey".to_string(),
            host: "myhost".to_string(),
            port: "444".to_string(),
            project_id: "222".to_string()
        };
        assert_eq!(parsed_creds, manual_creds);
    }

    #[test]
    fn test_parsing_dsn_when_lacking_project_id() {
        let parsed_creds = "https://mypublickey:myprivatekey@myhost/".parse::<SentryCredential>();
        assert!(parsed_creds.is_err());
    }

    #[test]
    fn test_parsing_dsn_when_lacking_private_key() {
        let parsed_creds = "https://mypublickey@myhost/myprojectid".parse::<SentryCredential>();
        assert!(parsed_creds.is_err());
    }

    #[test]
    fn test_parsing_dsn_when_lacking_protocol() {
        let parsed_creds = "mypublickey:myprivatekey@myhost/myprojectid".parse::<SentryCredential>();
        assert!(parsed_creds.is_err());
    }

    #[test]
    fn test_empty_settings_constructor_matches_empty_new_constructor() {
        let creds = "https://mypublickey:myprivatekey@myhost/myprojectid".parse::<SentryCredential>().unwrap();
        let from_settings = Sentry::from_settings(Settings::default(), creds.clone());
        let from_new = Sentry::new("".to_string(), "".to_string(), "".to_string(), creds);
        assert_eq!(from_settings.settings, from_new.settings);
    }

    #[test]
    fn test_full_settings_constructor_overrides_all_settings() {
        let creds = "https://mypublickey:myprivatekey@myhost/myprojectid".parse::<SentryCredential>().unwrap();
        let server_name = "server_name".to_string();
        let release = "release".to_string();
        let environment = "environment".to_string();
        let device = Device::new("device_name".to_string(), "version".to_string(), "build".to_string());
        let settings = Settings {
            server_name: server_name.clone(),
            release: release.clone(),
            environment: environment.clone(),
            device: device.clone()
        };
        let from_settings = Sentry::from_settings(settings, creds);
        assert_eq!(from_settings.settings.server_name, server_name);
        assert_eq!(from_settings.settings.release, release);
        assert_eq!(from_settings.settings.environment, environment);
        assert_eq!(from_settings.settings.device, device);
    }

    // #[test]
    // fn it_post_sentry_event() {
    //     let sentry = Sentry::new("Server Name".to_string(),
    //                              "release".to_string(),
    //                              "test_env".to_string(),
    //                              SentryCredential {
    //                                  key: "xx".to_string(),
    //                                  secret: "xx".to_string(),
    //                                  host: "app.getsentry.com".to_string(),
    //                                  project_id: "xx".to_string(),
    //                              });
    //
    //     sentry.info("test.logger",
    //                 "Test Message\nThis \"Message\" is nice\\cool!\nEnd",
    //                 None);
    //
    //     thread::sleep(Duration::new(5, 0));
    //
    // }
}
