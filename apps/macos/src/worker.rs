//! The connection to the daemon, on its own thread, so the UI never waits on
//! the socket. It reads the status every second, the settings and interfaces
//! on connecting and when asked, sends changes, and connects again after a
//! failure.

use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use ovsc_control::{
    Client, InterfaceInfo, Request, Response, Restart, Settings, SettingsChange, Status,
};

use crate::logic::Problem;

/// How often the status is read, and a lost connection tried again.
const POLL: Duration = Duration::from_secs(1);

/// What the UI asks of the worker.
#[derive(Debug)]
pub enum Command {
    /// Read the settings and the interfaces again.
    Reload,
    Apply(SettingsChange),
}

/// What the worker learned.
#[derive(Debug)]
pub enum Event {
    /// The daemon cannot be reached or used; sent when the problem changes.
    Problem(Problem),
    /// The daemon's status; sent when it changes.
    Status(Box<Status>),
    Settings(Settings),
    Interfaces(Vec<InterfaceInfo>),
    /// The answer to [`Command::Apply`]: what restarts, or why it failed.
    Applied(Result<Restart, String>),
}

/// The UI's end of the worker. The thread ends when this is dropped.
pub struct Worker {
    commands: Sender<Command>,
    events: Receiver<Event>,
}

impl Worker {
    /// Starts the worker on the daemon's socket at `socket`. It calls `wake`
    /// after each event.
    pub fn start(socket: PathBuf, wake: impl Fn() + Send + 'static) -> Worker {
        let (commands, command_rx) = mpsc::channel();
        let (event_tx, events) = mpsc::channel();
        let link = Link {
            socket,
            commands: command_rx,
            events: event_tx,
            wake: Box::new(wake),
            status: None,
            problem: None,
        };
        thread::Builder::new()
            .name("control".into())
            .spawn(move || link.run())
            .expect("cannot start the control thread");
        Worker { commands, events }
    }

    pub fn send(&self, command: Command) {
        // The thread only ends when this end is dropped.
        let _ = self.commands.send(command);
    }

    /// The events that arrived since the last call.
    pub fn events(&self) -> impl Iterator<Item = Event> + '_ {
        self.events.try_iter()
    }
}

/// Why a connection ended.
enum End {
    /// The UI is gone.
    Quit,
    Failed(io::Error),
}

impl From<io::Error> for End {
    fn from(e: io::Error) -> End {
        End::Failed(e)
    }
}

/// An answer the request does not take.
fn unexpected(response: &Response) -> End {
    let message = match response {
        Response::Error { message } => message.clone(),
        other => format!("unexpected answer {other:?}"),
    };
    End::Failed(io::Error::new(io::ErrorKind::InvalidData, message))
}

struct Link {
    socket: PathBuf,
    commands: Receiver<Command>,
    events: Sender<Event>,
    wake: Box<dyn Fn() + Send>,
    /// The last status sent, to skip repeats.
    status: Option<Box<Status>>,
    /// The last problem sent, to skip repeats.
    problem: Option<Problem>,
}

impl Link {
    fn run(mut self) {
        loop {
            let end = match Client::connect_to(&self.socket) {
                Ok(mut client) => {
                    let Err(end) = self.serve(&mut client);
                    end
                }
                Err(e) => End::Failed(e),
            };
            match end {
                End::Quit => return,
                End::Failed(e) => self.problem(Problem::from_io(&e)),
            }
            if !self.wait(POLL) {
                return;
            }
        }
    }

    /// Serves one connection until it fails or the UI is gone.
    fn serve(&mut self, client: &mut Client) -> Result<std::convert::Infallible, End> {
        let mut usable = self.poll(client)?;
        if usable {
            self.reload(client)?;
        }
        let mut next = Instant::now() + POLL;
        loop {
            match self.commands.recv_timeout(next.saturating_duration_since(Instant::now())) {
                Ok(Command::Reload) if usable => self.reload(client)?,
                Ok(Command::Reload) => {}
                Ok(Command::Apply(change)) if usable => self.apply(client, change)?,
                Ok(Command::Apply(_)) => self.applied(Err(self.not_usable())),
                Err(RecvTimeoutError::Timeout) => {
                    let was = usable;
                    usable = self.poll(client)?;
                    if usable && !was {
                        self.reload(client)?;
                    }
                    next = Instant::now() + POLL;
                }
                Err(RecvTimeoutError::Disconnected) => return Err(End::Quit),
            }
        }
    }

    /// Reads the status. False if the daemon cannot be used (another
    /// protocol, or an error instead of a status).
    fn poll(&mut self, client: &mut Client) -> Result<bool, End> {
        match client.request(&Request::Status)? {
            Response::Status(status) => match Problem::check(&status) {
                None => {
                    self.status(status);
                    Ok(true)
                }
                Some(problem) => {
                    self.problem(problem);
                    Ok(false)
                }
            },
            Response::Error { message } => {
                self.problem(Problem::Daemon(message));
                Ok(false)
            }
            other => Err(unexpected(&other)),
        }
    }

    fn reload(&mut self, client: &mut Client) -> Result<(), End> {
        match client.request(&Request::Settings)? {
            Response::Settings(settings) => self.send(Event::Settings(settings)),
            other => return Err(unexpected(&other)),
        }
        match client.request(&Request::Interfaces)? {
            Response::Interfaces { interfaces } => self.send(Event::Interfaces(interfaces)),
            other => return Err(unexpected(&other)),
        }
        Ok(())
    }

    fn apply(&mut self, client: &mut Client, change: SettingsChange) -> Result<(), End> {
        match client.request(&Request::Apply { change }) {
            Ok(Response::Applied { restart }) => {
                self.applied(Ok(restart));
                // A restarting daemon may be gone already; reconnecting
                // reloads then.
                self.reload(client)?;
                self.poll(client)?;
                Ok(())
            }
            Ok(Response::Error { message }) => {
                self.applied(Err(message));
                Ok(())
            }
            Ok(other) => {
                self.applied(Err("OpenVirtualSoundcard gave an answer this app does not understand.".into()));
                Err(unexpected(&other))
            }
            Err(e) => {
                self.applied(Err(format!(
                    "{} The change may or may not have been saved.",
                    Problem::from_io(&e).message()
                )));
                Err(e.into())
            }
        }
    }

    /// Waits for `time` while not connected, answering commands. False once
    /// the UI is gone.
    fn wait(&self, time: Duration) -> bool {
        let until = Instant::now() + time;
        loop {
            match self.commands.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(Command::Apply(_)) => self.applied(Err(self.not_usable())),
                Ok(Command::Reload) => {}
                Err(RecvTimeoutError::Timeout) => return true,
                Err(RecvTimeoutError::Disconnected) => return false,
            }
        }
    }

    fn not_usable(&self) -> String {
        match &self.problem {
            Some(problem) => problem.message(),
            None => "Not connected to OpenVirtualSoundcard.".into(),
        }
    }

    fn status(&mut self, status: Box<Status>) {
        self.problem = None;
        if self.status.as_ref() != Some(&status) {
            self.status = Some(status.clone());
            self.send(Event::Status(status));
        }
    }

    fn problem(&mut self, problem: Problem) {
        self.status = None;
        if self.problem.as_ref() != Some(&problem) {
            self.problem = Some(problem.clone());
            self.send(Event::Problem(problem));
        }
    }

    fn applied(&self, result: Result<Restart, String>) {
        self.send(Event::Applied(result));
    }

    fn send(&self, event: Event) {
        // A closed channel means the UI is gone; the command channel tells.
        if self.events.send(event).is_ok() {
            (self.wake)();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use ovsc_control::{ClockInfo, DriverInfo, PROTOCOL_VERSION, decode_line, encode_line};

    use super::*;

    impl Worker {
        /// The next event, waiting up to five seconds.
        fn next(&self) -> Event {
            self.events.recv_timeout(Duration::from_secs(5)).expect("no event from the worker")
        }
    }

    fn status(protocol: u32) -> Status {
        Status {
            protocol,
            version: "0.1.0".into(),
            engine_running: true,
            engine_error: None,
            device: None,
            clock: ClockInfo {
                source: "ptp".into(),
                state: "Locked".into(),
                locked: true,
                leader: Some("192.168.0.2".into()),
                offset_ns: 120,
                path_delay_ns: 40_000,
                freq_offset_ppm: 1.5,
            },
            driver: DriverInfo {
                connected: true,
                io_running: false,
                audio_flowing: true,
                detail: String::new(),
            },
            warnings: vec![],
        }
    }

    fn settings() -> Settings {
        Settings {
            name: "studio-mac".into(),
            interface: "en7".into(),
            sample_rate: 48_000,
            bits_per_sample: 24,
            rx_channels: 8,
            tx_channels: 8,
            latency_ms: 4.0,
        }
    }

    /// A daemon on a temporary socket that answers the status with
    /// `protocol` and a change with `applied`, and records the requests.
    fn daemon(test: &str, protocol: u32, applied: Response) -> (PathBuf, Arc<Mutex<Vec<Request>>>) {
        let dir = std::env::temp_dir().join(format!("ovapp-{}-{test}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("control.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let stream = stream.unwrap();
                let mut writer = stream.try_clone().unwrap();
                for line in BufReader::new(stream).lines() {
                    let request: Request = decode_line(&line.unwrap()).unwrap();
                    let response = match &request {
                        Request::Status => Response::Status(Box::new(status(protocol))),
                        Request::Settings => Response::Settings(settings()),
                        Request::Interfaces => Response::Interfaces { interfaces: vec![] },
                        Request::Apply { .. } => applied.clone(),
                    };
                    log.lock().unwrap().push(request);
                    if writer.write_all(encode_line(&response).as_bytes()).is_err() {
                        break;
                    }
                }
            }
        });
        (path, seen)
    }

    #[test]
    fn reads_everything_on_connecting() {
        let (path, _) = daemon("connect", PROTOCOL_VERSION, Response::Error { message: "".into() });
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = wakes.clone();
        let worker = Worker::start(path, move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        assert!(matches!(worker.next(), Event::Status(s) if *s == status(PROTOCOL_VERSION)));
        assert!(matches!(worker.next(), Event::Settings(s) if s == settings()));
        assert!(matches!(worker.next(), Event::Interfaces(i) if i.is_empty()));
        // The worker wakes the UI just after queueing each event.
        let deadline = Instant::now() + Duration::from_secs(1);
        while wakes.load(Ordering::SeqCst) < 3 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(wakes.load(Ordering::SeqCst), 3);
        // An unchanged status is not sent again.
        thread::sleep(POLL * 2);
        assert!(worker.events().next().is_none());
        assert_eq!(wakes.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn applies_a_change_and_reloads() {
        let (path, seen) =
            daemon("apply", PROTOCOL_VERSION, Response::Applied { restart: Restart::Device });
        let worker = Worker::start(path, || {});
        for _ in 0..3 {
            worker.next();
        }
        let change = SettingsChange { latency_ms: Some(2.0), ..Default::default() };
        worker.send(Command::Apply(change.clone()));
        assert!(matches!(worker.next(), Event::Applied(Ok(Restart::Device))));
        assert!(matches!(worker.next(), Event::Settings(_)));
        assert!(matches!(worker.next(), Event::Interfaces(_)));
        assert!(seen.lock().unwrap().contains(&Request::Apply { change }));
    }

    #[test]
    fn a_refused_change_reports_the_daemons_words() {
        let (path, _) =
            daemon("refuse", PROTOCOL_VERSION, Response::Error { message: "bad name".into() });
        let worker = Worker::start(path, || {});
        for _ in 0..3 {
            worker.next();
        }
        worker
            .send(Command::Apply(SettingsChange { name: Some("-".into()), ..Default::default() }));
        assert!(matches!(worker.next(), Event::Applied(Err(m)) if m == "bad name"));
        worker.send(Command::Reload);
        assert!(matches!(worker.next(), Event::Settings(_)));
    }

    #[test]
    fn without_a_socket_the_daemon_is_not_running() {
        let path = std::env::temp_dir().join(format!("ovapp-{}-absent.sock", std::process::id()));
        let worker = Worker::start(path, || {});
        assert!(matches!(worker.next(), Event::Problem(Problem::NotRunning)));
        worker
            .send(Command::Apply(SettingsChange { name: Some("x".into()), ..Default::default() }));
        assert!(
            matches!(worker.next(), Event::Applied(Err(m)) if m == Problem::NotRunning.message())
        );
    }

    #[test]
    fn another_protocol_is_reported_and_nothing_else_read() {
        let (path, seen) =
            daemon("protocol", PROTOCOL_VERSION + 1, Response::Applied { restart: Restart::None });
        let worker = Worker::start(path, || {});
        let Event::Problem(Problem::Version { daemon: Some((version, protocol)) }) = worker.next()
        else {
            panic!("expected a version problem");
        };
        assert_eq!((version.as_str(), protocol), ("0.1.0", PROTOCOL_VERSION + 1));
        worker.send(Command::Reload);
        worker
            .send(Command::Apply(SettingsChange { name: Some("x".into()), ..Default::default() }));
        assert!(matches!(worker.next(), Event::Applied(Err(_))));
        assert!(seen.lock().unwrap().iter().all(|r| *r == Request::Status));
    }
}
