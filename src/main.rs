use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::mem::ManuallyDrop;
use std::os::unix::io::FromRawFd;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::time::Instant;

use minivectordb::cgroup;
use minivectordb::db_err;
use minivectordb::engine::helper;
use minivectordb::engine::result::{Outcome, RowSink};
use minivectordb::engine::{Engine, EngineOptions};
use minivectordb::error::DbResult;
use minivectordb::protocol::{self, Op, Request, Response};
use minivectordb::sql;

fn main() {
    exit_on_termination_signals();
    // `--data-dir`, else MVDB_DATA_DIR, else the variable the organizers'
    // checker sets for a local run, else /data.
    let mut data_dir = std::env::var("MVDB_DATA_DIR")
        .or_else(|_| std::env::var("VECTORDB_DATA_DIR"))
        .unwrap_or_else(|_| "/data".to_string());
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--data-dir" {
            if let Some(d) = args.next() {
                data_dir = d;
            }
        }
    }

    // Recovery (log replay, cache rebuild) completes before we report ready.
    let mut engine = match Engine::open(Path::new(&data_dir), EngineOptions::default()) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("minivectordb: cannot open database in {data_dir}: {e}");
            std::process::exit(1);
        }
    };
    // The thread that takes half of each unfiltered TopK scan, if any,
    // starts before the first query.
    let _ = helper::get();
    let mut out = BufWriter::new(io::stdout().lock());
    let _ = protocol::write_ready(&mut out);
    let _ = out.flush();

    // Standard input through a reader of our own, so that `Polling` can tell
    // whether a request is already buffered.
    // SAFETY: descriptor 0 is open for the life of the process, and
    // `ManuallyDrop` keeps this handle from closing it.
    let stdin = ManuallyDrop::new(unsafe { File::from_raw_fd(0) });
    let mut input = BufReader::new(&*stdin);
    let mut polling = Polling::from_env();
    let mut after_query = false;
    let mut raw = Vec::new();
    loop {
        raw.clear();
        if polling.on && after_query && input.buffer().is_empty() {
            polling.wait();
        }
        after_query = false;
        match input.read_until(b'\n', &mut raw) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("minivectordb: stdin: {e}");
                break;
            }
        }
        // One request per line, one response per request.
        let line = match std::str::from_utf8(&raw) {
            Ok(line) => line.trim(),
            Err(_) => {
                let _ = protocol::write_error(&mut out, "null", "invalid_request", "request is not valid UTF-8");
                let _ = out.flush();
                continue;
            }
        };
        if line.is_empty() {
            continue;
        }
        let _ = match protocol::parse_request(line) {
            Ok(Request { id, op: Op::Execute { sql, .. } }) => {
                let mut response = Response::new(&mut out, id);
                let res = run(&mut engine, &sql, &mut response);
                after_query = matches!(res, Ok(Outcome::Rows));
                response.finish(res)
            }
            Ok(Request { id, op: Op::Shutdown }) => {
                // An open transaction is rolled back; every committed one is
                // already durable. Nothing may follow the acknowledgement.
                engine.abort_transaction();
                let _ = protocol::write_ok(&mut out, id);
                let _ = out.flush();
                helper::report();
                return;
            }
            Ok(Request { id, op: Op::Other(op) }) => {
                protocol::write_error(&mut out, id, "unsupported_op", &format!("unknown op '{op}'"))
            }
            Err(bad) => protocol::write_error(&mut out, bad.id, "invalid_request", &bad.msg),
        };
        let _ = out.flush();
    }
    // End of input without a shutdown request: dropping the engine discards
    // any open transaction; every committed one is already durable.
    helper::report();
}

/// After answering a query, the next request is waited for by polling
/// standard input for a moment instead of sleeping in `read` at once. A
/// client that sends its queries one after the other has the next one there
/// within that moment, and a process that did not sleep answers it sooner:
/// driven by the organizers' own transport with their container options,
/// the benchmark's 1,000 queries take 5.7% less with 2 CPUs and 6.1% less
/// with 1.
///
/// Polling uses a whole CPU while it waits, so it is on only where the
/// process has one: under a quota of half a CPU a first version without this
/// check made the query phase 43 to 61% slower. It follows queries only, so
/// during a load the process sleeps between statements as before.
struct Polling {
    on: bool,
    /// How long to poll before sleeping, in microseconds.
    micros: u64,
}

/// A stall this long between two turns of the polling loop means the process
/// was taken off the CPU while it polled, as a quota that cannot be read
/// from inside would do: polling then stays off.
const STALL_MICROS: u128 = 2_000;

impl Polling {
    /// One millisecond, or `MVDB_SPIN_US` microseconds (0 turns polling off);
    /// off under a CPU quota of less than one CPU. The organizers' client
    /// sends its next query about 180 us after an answer, so a window of
    /// 200 us missed most of them; 500, 1,000 and 2,000 us measured alike.
    fn from_env() -> Polling {
        let micros = std::env::var("MVDB_SPIN_US").ok().and_then(|v| v.parse().ok()).unwrap_or(1000);
        Polling { on: micros > 0 && cgroup::has_a_whole_cpu(cgroup::cpu_quota()), micros }
    }

    /// Returns once standard input is readable (or closed), or the time is up.
    fn wait(&mut self) {
        #[repr(C)]
        struct PollFd {
            fd: i32,
            events: i16,
            revents: i16,
        }
        extern "C" {
            fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
        }
        const POLLIN: i16 = 1;
        let mut fd = PollFd { fd: 0, events: POLLIN, revents: 0 };
        let start = Instant::now();
        let mut last = start;
        loop {
            // SAFETY: one valid pollfd; a zero timeout returns at once.
            // Anything but "nothing yet" (input, end of input, an error) is
            // left for `read` to report.
            if unsafe { poll(&mut fd, 1, 0) } != 0 {
                return;
            }
            let now = Instant::now();
            if (now - last).as_micros() > STALL_MICROS {
                self.on = false;
                eprintln!("minivectordb: stalled {} us while polling stdin; polling is off", (now - last).as_micros());
                return;
            }
            if (now - start).as_micros() >= self.micros as u128 {
                return;
            }
            last = now;
            std::hint::spin_loop();
        }
    }
}

/// SIGTERM and SIGINT end the session like end of input. Every acknowledged
/// commit is already durable and an open transaction exists only in memory,
/// so leaving at once is a normal shutdown that rolls it back; a frame cut
/// off by the exit is a torn tail, which recovery discards. The handler is
/// needed for `docker stop` to work at all: the database is process 1 in its
/// container, and the kernel delivers SIGTERM to process 1 only if it has
/// installed a handler.
fn exit_on_termination_signals() {
    extern "C" {
        fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
        fn _exit(code: i32) -> !;
    }
    extern "C" fn on_signal(_: i32) {
        // SAFETY: _exit is async-signal-safe.
        unsafe { _exit(0) }
    }
    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;
    // SAFETY: the handler only calls an async-signal-safe function.
    unsafe {
        signal(SIGTERM, on_signal);
        signal(SIGINT, on_signal);
    }
}

/// Parses and executes one statement. A panic is reported as an error and
/// rolls back the open transaction instead of taking the process down.
fn run(engine: &mut Engine, text: &str, sink: &mut dyn RowSink) -> DbResult<Outcome> {
    match panic::catch_unwind(AssertUnwindSafe(|| engine.execute_into(&sql::parse(text)?, sink))) {
        Ok(res) => res,
        Err(_) => {
            engine.abort_transaction();
            Err(db_err!(Internal, "internal error; the transaction was rolled back"))
        }
    }
}
