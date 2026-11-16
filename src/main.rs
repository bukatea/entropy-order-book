//! Command-line front end: replays one command log and writes its canonical
//! output to stdout.
//!
//! The log is read once, line by line, and never written. Exit status: 0 when
//! the log ran to its end (rejected rows included), 1 when the engine halted,
//! 2 for a usage error, 3 for an I/O failure.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use orderbook::canonical::{write_digest_trailer, write_event, write_events_header};
use orderbook::engine::Engine;
use orderbook::event::{Event, HaltReason};
use orderbook::input::{self, HeaderError, read_physical_line};

const USAGE: &str = "usage: orderbook [--state] <log.csv>";

const HELP: &str = "\
usage: orderbook [--state] <log.csv>

Replays a command log and writes its canonical output to stdout.

  --state      write the final engine state instead of the event stream
  -h, --help   show this help and exit
  --           end the options; later arguments are file names

Exit status: 0 success, 1 engine halted, 2 usage error, 3 I/O error.
";

const EXIT_HALTED: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_IO: u8 = 3;

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Request {
    Help,
    Run { state: bool, path: PathBuf },
}

/// Why a run stopped before the end of its output.
enum Failure {
    Read(io::Error),
    Write(io::Error),
}

impl Failure {
    fn report(self, path: &Path) -> ExitCode {
        match self {
            Self::Read(error) => diagnose(&format!("cannot read {}: {error}", path.display())),
            Self::Write(error) => diagnose(&format!("cannot write output: {error}")),
        }
        ExitCode::from(EXIT_IO)
    }
}

fn main() -> ExitCode {
    match parse_args(std::env::args_os().skip(1)) {
        Ok(Request::Help) => print_help(),
        Ok(Request::Run { state, path }) => run(&path, state),
        Err(message) => {
            diagnose(&format!("{message}\n{USAGE}"));
            ExitCode::from(EXIT_USAGE)
        }
    }
}

/// Parses the arguments that follow the program name.
fn parse_args<I: IntoIterator<Item = OsString>>(args: I) -> Result<Request, String> {
    let mut state = false;
    let mut paths = Vec::new();
    let mut options_done = false;
    for arg in args {
        if options_done {
            paths.push(arg);
        } else if arg == "--" {
            options_done = true;
        } else if arg == "--state" {
            state = true;
        } else if arg == "--help" || arg == "-h" {
            return Ok(Request::Help);
        } else if arg.as_encoded_bytes().starts_with(b"-") && arg != "-" {
            return Err(format!("unknown option: {}", arg.to_string_lossy()));
        } else {
            paths.push(arg);
        }
    }
    match <[OsString; 1]>::try_from(paths) {
        Ok([path]) => Ok(Request::Run {
            state,
            path: path.into(),
        }),
        Err(paths) if paths.is_empty() => Err("missing log file".to_owned()),
        Err(_) => Err("too many arguments".to_owned()),
    }
}

fn print_help() -> ExitCode {
    let mut out = io::stdout().lock();
    match out.write_all(HELP.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            diagnose(&format!("cannot write output: {error}"));
            ExitCode::from(EXIT_IO)
        }
    }
}

fn run(path: &Path, state: bool) -> ExitCode {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) => {
            diagnose(&format!("cannot open {}: {error}", path.display()));
            return ExitCode::from(EXIT_IO);
        }
    };
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    let halt = match replay(&mut BufReader::new(file), &mut out, state) {
        Ok(halt) => halt,
        Err(failure) => return failure.report(path),
    };
    match halt {
        None => ExitCode::SUCCESS,
        Some(reason) => {
            diagnose(&format!("engine halted: {reason:?}"));
            ExitCode::from(EXIT_HALTED)
        }
    }
}

/// Replays the log into `out` and returns the halt reason, if the engine halted.
///
/// Reading stops at the first halt, so no later line is applied. Events are
/// written as they are produced and never retained.
fn replay<R: BufRead, W: Write>(
    input: &mut R,
    out: &mut W,
    state: bool,
) -> Result<Option<HaltReason>, Failure> {
    let mut engine = Engine::new();
    let mut buf = Vec::new();
    if !state {
        write_events_header(out).map_err(Failure::Write)?;
    }
    let header = match read_physical_line(input, &mut buf).map_err(Failure::Read)? {
        None => Err(HeaderError::Empty),
        Some(line) => input::parse_header(line),
    };
    match header {
        // A new engine has not halted, so it accepts the header failure.
        Err(error) => emit(out, state, &engine.reject_header(error).unwrap_or_default())?,
        Ok(()) => {
            while engine.halt().is_none() {
                let Some(line) = read_physical_line(input, &mut buf).map_err(Failure::Read)? else {
                    break;
                };
                // The loop condition ensures the engine has not halted.
                let events = engine.apply(input::parse_line(line)).unwrap_or_default();
                emit(out, state, &events)?;
            }
        }
    }
    if state {
        engine.write_state(out).map_err(Failure::Write)?;
    } else {
        write_digest_trailer(out, &engine.digest()).map_err(Failure::Write)?;
    }
    out.flush().map_err(Failure::Write)?;
    Ok(engine.halt())
}

/// Writes one step's events, unless only the final state was requested.
fn emit<W: Write>(out: &mut W, state: bool, events: &[Event]) -> Result<(), Failure> {
    if !state {
        for event in events {
            write_event(out, event).map_err(Failure::Write)?;
        }
    }
    Ok(())
}

/// Writes one diagnostic line to stderr. A failed write is dropped, because
/// there is nowhere left to report it.
fn diagnose(message: &str) {
    let _ = writeln!(io::stderr(), "orderbook: {message}");
}
