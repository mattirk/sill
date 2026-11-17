//! Pictures of a test desktop for the live view: a minimal Wayland client of the desktop's own
//! compositor that copies its output into shared memory with Weston's capture protocol
//! (`weston_capture_v1`, offered with `--debug`). Raw pixels, no encoding: the caller hashes
//! them to tell an unchanged desktop apart and encodes a PNG only for a picture someone reads.
//! Every wait has a deadline; the compositor is trusted, the programs drawing on it are not, and
//! they never talk to this client.
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};

#[allow(dead_code, non_camel_case_types, non_upper_case_globals, clippy::all)]
mod protocol {
    use wayland_client;
    use wayland_client::protocol::*;
    pub mod __interfaces {
        use wayland_client::backend as wayland_backend;
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/weston-output-capture.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/weston-output-capture.xml");
}
use protocol::weston_capture_source_v1::{self, WestonCaptureSourceV1};
use protocol::weston_capture_v1::{self, WestonCaptureV1};

/// DRM fourcc codes of the 32-bit formats read here (little-endian B, G, R, X/A in memory).
const DRM_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
const DRM_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
/// How long one capture may take, connection to pixels.
const DEADLINE: Duration = Duration::from_secs(5);
/// The largest output read: 4× a 1280×800 desktop.
const MAX_PIXELS: i64 = 5120 * 3200;

/// An output's pixels as the compositor gave them: 32-bit B, G, R, X/A in memory, row after row.
pub struct Raw {
    pub width: u32,
    pub height: u32,
    pub bgrx: Vec<u8>,
}

#[derive(Default)]
struct State {
    formats: Vec<u32>,
    size: Option<(i32, i32)>,
    outcome: Option<Result<(), String>>,
    retry: bool,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(_: &mut Self, _: &wl_output::WlOutput, _: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<wl_shm::WlShm, ()> for State {
    fn event(_: &mut Self, _: &wl_shm::WlShm, _: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<wl_shm_pool::WlShmPool, ()> for State {
    fn event(_: &mut Self, _: &wl_shm_pool::WlShmPool, _: wl_shm_pool::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(_: &mut Self, _: &wl_buffer::WlBuffer, _: wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WestonCaptureV1, ()> for State {
    fn event(_: &mut Self, _: &WestonCaptureV1, _: weston_capture_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WestonCaptureSourceV1, ()> for State {
    fn event(state: &mut Self, _: &WestonCaptureSourceV1, event: weston_capture_source_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use weston_capture_source_v1::Event;
        match event {
            Event::Format { drm_format } => state.formats.push(drm_format),
            Event::Size { width, height } => state.size = Some((width, height)),
            Event::Complete => state.outcome = Some(Ok(())),
            Event::Retry => state.retry = true,
            Event::Failed { msg } => state.outcome = Some(Err(msg.unwrap_or_else(|| "capture failed".into()))),
            _ => {}
        }
    }
}

/// Dispatch until `done`, reading from the socket with what is left of `deadline`.
fn dispatch_until(queue: &mut EventQueue<State>, state: &mut State, deadline: Instant, done: impl Fn(&State) -> bool) -> Result<()> {
    loop {
        queue.dispatch_pending(state).context("the test desktop's compositor")?;
        if done(state) {
            return Ok(());
        }
        queue.flush().context("the test desktop's compositor")?;
        let Some(guard) = queue.prepare_read() else { continue };
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("the test desktop did not give a picture within {} s", DEADLINE.as_secs());
        }
        let fd = guard.connection_fd();
        let mut fds = [rustix::event::PollFd::new(&fd, rustix::event::PollFlags::IN)];
        let timeout = rustix::event::Timespec::try_from(left).unwrap_or(rustix::event::Timespec { tv_sec: 5, tv_nsec: 0 });
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(0) => bail!("the test desktop did not give a picture within {} s", DEADLINE.as_secs()),
            Ok(_) => {
                guard.read().context("reading from the test desktop's compositor")?;
            }
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

/// The pixels of the first output of the compositor listening on `socket`.
pub fn capture(socket: &Path) -> Result<Raw> {
    let deadline = Instant::now() + DEADLINE;
    let stream = UnixStream::connect(socket).with_context(|| format!("connecting to {}", socket.display()))?;
    let conn = Connection::from_socket(stream).context("the test desktop's Wayland socket")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).context("the test desktop's globals")?;
    let qh = queue.handle();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).context("no wl_shm")?;
    let output: wl_output::WlOutput = globals.bind(&qh, 1..=4, ()).context("the test desktop has no output")?;
    let factory: WestonCaptureV1 = globals.bind(&qh, 1..=2, ()).context("the test desktop offers no capture (Weston started without --debug?)")?;
    let source = factory.create(&output, weston_capture_v1::Source::Framebuffer, &qh, ());
    let mut state = State::default();
    let result = (|| {
        for _attempt in 0..3 {
            dispatch_until(&mut queue, &mut state, deadline, |s| s.size.is_some() && !s.formats.is_empty())?;
            let (width, height) = state.size.expect("size");
            if width <= 0 || height <= 0 || i64::from(width) * i64::from(height) > MAX_PIXELS {
                bail!("the test desktop's output has an unexpected size {width}×{height}");
            }
            let format = if state.formats.contains(&DRM_XRGB8888) {
                wl_shm::Format::Xrgb8888
            } else if state.formats.contains(&DRM_ARGB8888) {
                wl_shm::Format::Argb8888
            } else {
                bail!("the test desktop offers no 32-bit RGB picture format");
            };
            let stride = width * 4;
            let len = stride as usize * height as usize;
            let memory = rustix::fs::memfd_create("sill-desktop-capture", rustix::fs::MemfdFlags::CLOEXEC)?;
            rustix::fs::ftruncate(&memory, len as u64)?;
            let pool = shm.create_pool(memory.as_fd(), len as i32, &qh, ());
            let buffer = pool.create_buffer(0, width, height, stride, format, &qh, ());
            state.outcome = None;
            state.retry = false;
            source.capture(&buffer);
            dispatch_until(&mut queue, &mut state, deadline, |s| s.outcome.is_some() || s.retry)?;
            let outcome = state.outcome.take();
            buffer.destroy();
            pool.destroy();
            if state.retry {
                // new parameters were sent before the retry: take them on the next round
                state.formats.clear();
                state.size = None;
                continue;
            }
            match outcome {
                Some(Ok(())) => {
                    let mut bgrx = vec![0u8; len];
                    let mut read = 0;
                    while read < len {
                        let n = rustix::io::pread(&memory, &mut bgrx[read..], read as u64)?;
                        if n == 0 {
                            bail!("the picture ended early");
                        }
                        read += n;
                    }
                    return Ok(Raw { width: width as u32, height: height as u32, bgrx });
                }
                Some(Err(why)) => bail!("the test desktop could not be captured: {why}"),
                None => unreachable!("dispatched until an outcome"),
            }
        }
        bail!("the test desktop kept changing its picture size")
    })();
    source.destroy();
    factory.destroy();
    let _ = conn.flush();
    result
}

/// The SHA-256 of the pixels, hex: an unchanged picture has the same revision.
pub fn revision(raw: &Raw) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(&raw.bgrx).iter().map(|b| format!("{b:02x}")).collect()
}

/// `raw` as an RGB PNG, quickly (fdeflate): a few tens of milliseconds for a HiDPI desktop.
pub fn png(raw: &Raw) -> Result<Vec<u8>> {
    let mut rgb = Vec::with_capacity(raw.width as usize * raw.height as usize * 3);
    for px in raw.bgrx.chunks_exact(4) {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, raw.width, raw.height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        // fdeflate with the Up filter: the least work to encode and to decode (about 30 and 20 ms
        // for a 2560×1600 desktop, optimised); a plain background keeps it small
        encoder.set_compression(png::Compression::Fast);
        encoder.set_filter(png::Filter::Up);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&rgb)?;
    }
    Ok(out)
}
