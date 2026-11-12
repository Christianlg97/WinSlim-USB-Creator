//! Descarga segmentada y multifuente de la ISO.
//!
//! SourceForge sirve este proyecto desde un único servidor (`master`) que limita
//! cada conexión a unos cientos de KB/s. Para aprovechar la conexión del usuario
//! la ISO se reparte en tramos que descargan a la vez varias conexiones de varias
//! fuentes: los espejos de SourceForge que respondan y la descarga alternativa
//! publicada en GitHub (el mismo archivo, normalmente mucho más rápida).
//!
//! - Cada tramo se escribe directamente en su posición del archivo `.part`.
//! - Las conexiones rápidas piden tramos mayores y, cuando ya no queda trabajo
//!   pendiente, se quedan con la mitad restante de los tramos más lentos.
//! - Un hilo calcula el MD5 en orden a medida que se completa el principio del
//!   archivo; la ISO solo se publica si coincide con el MD5 del RSS, sea cual sea
//!   la fuente de cada byte.
//! - El progreso se guarda en `<iso>.part.ranges` para reanudar tras un corte.
use super::*;
use serde::Serialize;
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU32, AtomicUsize};

/// Conexiones simultáneas con la descarga alternativa (Google Drive u otra).
const ALTERNATIVE_CONNECTIONS: usize = 12;
/// Conexiones con SourceForge cuando es la única fuente. Es el máximo habitual
/// de los gestores de descargas por servidor; más conexiones solo cargarían un
/// servidor compartido que limita cada una a unos cientos de KB/s.
const SOURCEFORGE_CONNECTIONS: usize = 16;
/// Conexiones con SourceForge cuando también hay descarga alternativa.
const SOURCEFORGE_CONNECTIONS_WITH_ALTERNATIVE: usize = 4;
const READ_BUFFER_BYTES: usize = 256 * 1024;
const HASH_BUFFER_BYTES: usize = 4 * 1024 * 1024;
const STATE_SAVE_INTERVAL: Duration = Duration::from_secs(2);
const SPEED_WINDOW: Duration = Duration::from_secs(5);
/// Una fuente que falla sin entregar datos durante este tiempo se descarta.
const SOURCE_GRACE: Duration = Duration::from_secs(45);
const USER_AGENT: &str = "WinSlimUsbCreator/0.1";

/// Si una descarga con la fuente alternativa no supera el MD5, no se vuelve a
/// usar en esta sesión: el archivo publicado allí no es el del RSS.
static ALTERNATIVE_DISTRUSTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy)]
pub(super) struct SegmentConfig {
    /// Tramo mínimo que se divide al repartir trabajo entre conexiones.
    pub min_split: u64,
    /// Primer tramo de una conexión, antes de conocer su velocidad.
    pub initial_chunk: u64,
    pub max_chunk: u64,
    /// Cada conexión pide trabajo para unos segundos según su velocidad.
    pub target_chunk_seconds: f64,
    pub max_source_failures: u32,
    pub retry_base: Duration,
    pub source_grace: Duration,
    /// Antigüedad mínima de un tramo para que otra conexión se lo quede entero.
    pub steal_after: Duration,
}

impl Default for SegmentConfig {
    fn default() -> Self {
        Self {
            min_split: 1024 * 1024,
            initial_chunk: 4 * 1024 * 1024,
            max_chunk: 256 * 1024 * 1024,
            target_chunk_seconds: 20.0,
            max_source_failures: 8,
            retry_base: Duration::from_secs(1),
            source_grace: SOURCE_GRACE,
            steal_after: Duration::from_secs(3),
        }
    }
}

type Refresh = Box<dyn Fn() -> Result<String, String> + Send + Sync>;

pub(super) struct Source {
    label: String,
    url: Mutex<String>,
    /// Vuelve a resolver la URL (los enlaces de SourceForge caducan).
    refresh: Option<Refresh>,
    connections: usize,
    /// Conexiones permitidas si otra fuente deja de responder.
    max_connections: usize,
    alternative: bool,
    failures: AtomicU32,
    disabled: AtomicBool,
    delivered: AtomicU64,
    live: AtomicUsize,
    /// Milisegundos desde el inicio de la descarga en que entregó datos por última vez.
    last_success_ms: AtomicU64,
    last_error: Mutex<String>,
}

impl Source {
    pub(super) fn new(
        label: impl Into<String>,
        url: impl Into<String>,
        connections: usize,
        max_connections: usize,
        alternative: bool,
        refresh: Option<Refresh>,
    ) -> Self {
        Self {
            label: label.into(),
            url: Mutex::new(url.into()),
            refresh,
            connections: connections.max(1),
            max_connections: max_connections.max(connections).max(1),
            alternative,
            failures: AtomicU32::new(0),
            disabled: AtomicBool::new(false),
            delivered: AtomicU64::new(0),
            live: AtomicUsize::new(0),
            last_success_ms: AtomicU64::new(0),
            last_error: Mutex::new(String::new()),
        }
    }

    fn enabled(&self) -> bool {
        !self.disabled.load(Ordering::Acquire)
    }

    fn current_url(&self) -> String {
        self.url
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Intervalos `[inicio, fin)` ordenados y fusionados.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub(super) struct Ranges(Vec<(u64, u64)>);

impl Ranges {
    pub(super) fn insert(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        let (mut start, mut end) = (start, end);
        let mut merged = Vec::with_capacity(self.0.len() + 1);
        let mut placed = false;
        for &(a, b) in &self.0 {
            if b < start {
                merged.push((a, b));
            } else if a > end {
                if !placed {
                    merged.push((start, end));
                    placed = true;
                }
                merged.push((a, b));
            } else {
                start = start.min(a);
                end = end.max(b);
            }
        }
        if !placed {
            merged.push((start, end));
        }
        self.0 = merged;
    }

    pub(super) fn prefix_end(&self) -> u64 {
        match self.0.first() {
            Some(&(0, end)) => end,
            _ => 0,
        }
    }

    pub(super) fn total(&self) -> u64 {
        self.0.iter().map(|(a, b)| b - a).sum()
    }

    pub(super) fn missing(&self, size: u64) -> Vec<(u64, u64)> {
        let mut gaps = Vec::new();
        let mut cursor = 0;
        for &(a, b) in &self.0 {
            if a > cursor {
                gaps.push((cursor, a.min(size)));
            }
            cursor = cursor.max(b);
        }
        if cursor < size {
            gaps.push((cursor, size));
        }
        gaps.retain(|(a, b)| a < b);
        gaps
    }
}

#[derive(Serialize, Deserialize)]
struct SavedProgress {
    size: u64,
    md5: String,
    done: Vec<(u64, u64)>,
}

pub(super) fn state_path(part: &Path) -> PathBuf {
    let mut name = part.as_os_str().to_owned();
    name.push(".ranges");
    PathBuf::from(name)
}

fn read_saved(path: &Path) -> Option<SavedProgress> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn save_progress(part: &Path, iso: &Iso, done: &Ranges) -> Result<(), String> {
    let state = state_path(part);
    let pending = state.with_extension("ranges.tmp");
    let bytes = serde_json::to_vec(&SavedProgress {
        size: iso.size,
        md5: iso.md5.clone().unwrap_or_default(),
        done: done.0.clone(),
    })
    .map_err(|error| error.to_string())?;
    fs::write(&pending, bytes).map_err(|error| file_error("guardar progreso", &pending, error))?;
    fs::rename(&pending, &state).map_err(|error| file_error("guardar progreso", &state, error))
}

/// Deja `.part` en el formato contiguo de la descarga de una conexión: conserva
/// el tramo inicial completo y descarta la tabla de tramos.
pub(super) fn convert_to_contiguous(part: &Path) -> Result<(), String> {
    let state = state_path(part);
    if !state.exists() {
        return Ok(());
    }
    let prefix = read_saved(&state).map_or(0, |saved| {
        let mut done = Ranges::default();
        for (a, b) in saved.done {
            done.insert(a, b);
        }
        done.prefix_end()
    });
    if part.exists() {
        OpenOptions::new()
            .write(true)
            .open(part)
            .and_then(|file| file.set_len(prefix))
            .map_err(|error| file_error("ajustar descarga parcial", part, error))?;
    }
    fs::remove_file(&state).map_err(|error| file_error("limpiar progreso", &state, error))
}

/// Recupera el progreso guardado y reserva el tamaño completo de la ISO.
fn prepare_part(part: &Path, iso: &Iso) -> Result<Ranges, String> {
    let state = state_path(part);
    let length = fs::metadata(part).map(|metadata| metadata.len()).ok();
    let mut done = Ranges::default();
    match read_saved(&state) {
        Some(saved)
            if saved.size == iso.size
                && iso.md5.as_deref() == Some(saved.md5.as_str())
                && length == Some(iso.size) =>
        {
            for (a, b) in saved.done {
                if b <= iso.size {
                    done.insert(a, b);
                }
            }
        }
        Some(_) => {
            log_event("INFO etapa=descarga_segmentada progreso_obsoleto descartado".into());
            let _ = fs::remove_file(part);
            let _ = fs::remove_file(&state);
        }
        None => match length {
            // Descarga anterior de una conexión: el archivo es un prefijo continuo.
            Some(length) if length <= iso.size => done.insert(0, length),
            Some(_) => {
                fs::remove_file(part)
                    .map_err(|error| file_error("descartar descarga parcial", part, error))?;
            }
            None => {}
        },
    }
    let allocated = fs::metadata(part)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let dir = part.parent().unwrap_or(Path::new("."));
    let needed = iso.size.saturating_sub(allocated);
    if fs_free_bytes(dir)? < needed {
        return Err(format!(
            "No hay espacio suficiente para descargar {}",
            human(needed)
        ));
    }
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(part)
        .and_then(|file| file.set_len(iso.size))
        .map_err(|error| file_error("reservar descarga temporal", part, error))?;
    save_progress(part, iso, &done)?;
    Ok(done)
}

#[cfg(windows)]
fn write_all_at(file: &File, mut buffer: &[u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buffer.is_empty() {
        let written = file.seek_write(buffer, offset)?;
        if written == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        buffer = &buffer[written..];
        offset += written as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn write_all_at(file: &File, buffer: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buffer, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buffer.is_empty() {
        let read = file.seek_read(buffer, offset)?;
        if read == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buffer = &mut buffer[read..];
        offset += read as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn read_exact_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buffer, offset)
}

/// Tramo asignado a una conexión. `end` puede reducirse si otra conexión se
/// queda con la parte final; `pos` es lo ya escrito.
struct Job {
    start: u64,
    end: AtomicU64,
    pos: AtomicU64,
    created: Instant,
    /// Tramo quitado entero a una conexión lenta. No se vuelve a quitar: si su
    /// conexión se atasca, el tiempo de espera de lectura lo devuelve a pendientes.
    rescue: bool,
}

impl Job {
    fn new(start: u64, end: u64, rescue: bool) -> Self {
        Self {
            start,
            end: AtomicU64::new(end),
            pos: AtomicU64::new(start),
            created: Instant::now(),
            rescue,
        }
    }

    fn remaining(&self) -> u64 {
        self.end
            .load(Ordering::Acquire)
            .saturating_sub(self.pos.load(Ordering::Acquire))
    }

    /// Velocidad media desde que se asignó, contando la espera inicial: un
    /// tramo atascado o que aún no recibió nada vale 0.
    fn effective_speed(&self) -> u64 {
        let received = self.pos.load(Ordering::Acquire).saturating_sub(self.start);
        (received as f64 / self.created.elapsed().as_secs_f64().max(0.001)) as u64
    }
}

#[derive(Default)]
struct Plan {
    pending: Vec<(u64, u64)>,
    active: Vec<Arc<Job>>,
    done: Ranges,
}

struct Engine<'a> {
    size: u64,
    part: &'a Path,
    config: SegmentConfig,
    plan: Mutex<Plan>,
    stop: AtomicBool,
    fatal: Mutex<Option<String>>,
    hashed: AtomicU64,
    rescues: AtomicU32,
    enabled_sources: AtomicUsize,
    started: Instant,
    download_state: &'a AtomicU8,
}

enum FetchError {
    Stopped,
    Fatal(String),
    /// La fuente no sirve rangos de este archivo: no tiene sentido reintentar.
    Unsupported(String),
    Retry {
        message: String,
        refresh: bool,
    },
}

impl<'a> Engine<'a> {
    fn new(
        size: u64,
        part: &'a Path,
        config: SegmentConfig,
        done: Ranges,
        download_state: &'a AtomicU8,
    ) -> Self {
        let pending = done.missing(size);
        Self {
            size,
            part,
            config,
            plan: Mutex::new(Plan {
                pending,
                active: Vec::new(),
                done,
            }),
            stop: AtomicBool::new(false),
            fatal: Mutex::new(None),
            hashed: AtomicU64::new(0),
            rescues: AtomicU32::new(0),
            enabled_sources: AtomicUsize::new(0),
            started: Instant::now(),
            download_state,
        }
    }

    fn plan(&self) -> MutexGuard<'_, Plan> {
        self.plan.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
            || self.download_state.load(Ordering::Acquire) == DOWNLOAD_CANCELLED
    }

    fn fail(&self, message: String) {
        self.fatal
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(message);
        self.stop.store(true, Ordering::Release);
    }

    fn complete(&self) -> bool {
        self.plan().done.total() >= self.size
    }

    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Entrega el siguiente tramo pendiente (de menor posición, para que el MD5
    /// avance a la vez) o, si no queda ninguno, la mitad final del tramo activo
    /// con más trabajo restante.
    fn next_job(&self, chunk: u64, my_speed: u64) -> Option<Arc<Job>> {
        let mut plan = self.plan();
        if let Some(&(start, end)) = plan.pending.first() {
            let job_end = end.min(start.saturating_add(chunk.max(1)));
            if job_end >= end {
                plan.pending.remove(0);
            } else {
                plan.pending[0].0 = job_end;
            }
            let job = Arc::new(Job::new(start, job_end, false));
            plan.active.push(job.clone());
            return Some(job);
        }
        let largest = plan
            .active
            .iter()
            .max_by_key(|job| job.remaining())?
            .clone();
        let (victim, start, rescue) = if largest.remaining() >= 2 * self.config.min_split {
            let pos = largest.pos.load(Ordering::Acquire);
            let remaining = largest.end.load(Ordering::Acquire).saturating_sub(pos);
            (largest, pos + remaining / 2, false)
        } else {
            // Final de la descarga: una conexión mucho más rápida se queda entero
            // un tramo lento o atascado. Solo tramos con unos segundos de vida y
            // velocidad medida; si no, las conexiones ociosas se robarían el
            // último tramo unas a otras antes de recibir un solo byte.
            if my_speed == 0 {
                return None;
            }
            let slow = plan
                .active
                .iter()
                .filter(|job| {
                    !job.rescue
                        && job.remaining() > 0
                        && job.created.elapsed() >= self.config.steal_after
                        && job.effective_speed().saturating_mul(4) < my_speed
                })
                .max_by_key(|job| job.remaining())?
                .clone();
            let pos = slow.pos.load(Ordering::Acquire);
            (slow, pos, true)
        };
        let end = victim.end.load(Ordering::Acquire);
        if start >= end {
            return None;
        }
        victim.end.store(start, Ordering::Release);
        if rescue {
            self.rescues.fetch_add(1, Ordering::Relaxed);
        }
        let job = Arc::new(Job::new(start, end, rescue));
        plan.active.push(job.clone());
        Some(job)
    }

    /// Registra lo escrito por un tramo y devuelve a pendientes lo que faltó.
    fn finish_job(&self, job: &Arc<Job>) {
        let mut plan = self.plan();
        let reached = job.pos.load(Ordering::Acquire);
        let end = job.end.load(Ordering::Acquire);
        plan.done.insert(job.start, reached);
        if reached < end {
            plan.pending.push((reached, end));
            plan.pending.sort_unstable();
        }
        plan.active.retain(|active| !Arc::ptr_eq(active, job));
    }

    fn progress(&self) -> u64 {
        let plan = self.plan();
        let in_flight: u64 = plan
            .active
            .iter()
            .map(|job| job.pos.load(Ordering::Acquire).saturating_sub(job.start))
            .sum();
        (plan.done.total() + in_flight).min(self.size)
    }
}

fn describe_http_error(error: ureq::Error) -> (String, bool) {
    match error {
        ureq::Error::Status(code, _) => (
            format!("HTTP {code}"),
            matches!(code, 401 | 403 | 404 | 410),
        ),
        ureq::Error::Transport(transport) => (transport.to_string(), false),
    }
}

fn fetch_range(
    engine: &Engine,
    source: &Source,
    agent: &ureq::Agent,
    file: &File,
    job: &Job,
    buffer: &mut [u8],
) -> Result<(), FetchError> {
    let start = job.start;
    let request_end = job.end.load(Ordering::Acquire);
    if start >= request_end {
        return Ok(());
    }
    let response = agent
        .get(&source.current_url())
        .set("User-Agent", USER_AGENT)
        .set("Accept-Encoding", "identity")
        .set("Range", &format!("bytes={start}-{}", request_end - 1))
        .call()
        .map_err(|error| {
            let (message, refresh) = describe_http_error(error);
            FetchError::Retry { message, refresh }
        })?;
    if response
        .header("Content-Type")
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/html"))
    {
        // Página de cuota agotada, inicio de sesión o aviso intermedio.
        return Err(FetchError::Retry {
            message: "el servidor devolvió una página HTML".into(),
            refresh: true,
        });
    }
    let expected_range = format!("bytes {start}-{}/{}", request_end - 1, engine.size);
    if response.status() != 206 || response.header("Content-Range") != Some(&expected_range) {
        let message = format!(
            "respuesta de rango inesperada (HTTP {} {:?})",
            response.status(),
            response.header("Content-Range")
        );
        // Una fuente que ya sirvió rangos solo tuvo una respuesta anómala
        // (p. ej. por limitación temporal): se reintenta en lugar de descartarla.
        return Err(if source.delivered.load(Ordering::Acquire) > 0 {
            FetchError::Retry {
                message,
                refresh: false,
            }
        } else {
            FetchError::Unsupported(message)
        });
    }
    if response
        .header("Content-Encoding")
        .is_some_and(|value| !value.eq_ignore_ascii_case("identity"))
    {
        return Err(FetchError::Unsupported(
            "el servidor comprimió la respuesta".into(),
        ));
    }
    let mut reader = response.into_reader();
    let mut pos = start;
    loop {
        if engine.stopped() {
            return Err(FetchError::Stopped);
        }
        let limit = job.end.load(Ordering::Acquire).min(request_end);
        if pos >= limit {
            break;
        }
        let want = ((limit - pos) as usize).min(buffer.len());
        let read = reader
            .read(&mut buffer[..want])
            .map_err(|error| FetchError::Retry {
                message: format!("lectura interrumpida: {error}"),
                refresh: false,
            })?;
        if read == 0 {
            return Err(FetchError::Retry {
                message: "el servidor cerró la conexión antes de terminar el tramo".into(),
                refresh: false,
            });
        }
        write_all_at(file, &buffer[..read], pos).map_err(|error| {
            FetchError::Fatal(file_error("escribir descarga temporal", engine.part, error))
        })?;
        pos += read as u64;
        job.pos.store(pos, Ordering::Release);
        source.delivered.fetch_add(read as u64, Ordering::Relaxed);
        source
            .last_success_ms
            .store(engine.elapsed_ms(), Ordering::Release);
    }
    if pos >= request_end {
        // Agotar el cuerpo devuelve la conexión al grupo para reutilizarla
        // (evita repetir el saludo TLS, que con SourceForge tarda ~1 s).
        let _ = reader.read(&mut buffer[..1]);
    }
    Ok(())
}

fn wait_unless_stopped(engine: &Engine, duration: Duration) {
    let until = Instant::now() + duration;
    while Instant::now() < until && !engine.stopped() {
        thread::sleep(Duration::from_millis(100));
    }
}

fn run_worker(engine: &Engine, source: &Source) {
    struct Live<'a>(&'a AtomicUsize);
    impl Drop for Live<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }
    let _live = Live(&source.live);
    let file = match OpenOptions::new().write(true).open(engine.part) {
        Ok(file) => file,
        Err(error) => {
            engine.fail(file_error("abrir descarga temporal", engine.part, error));
            return;
        }
    };
    // Sin datos durante 12 s la conexión se reintenta; también acota lo que
    // tarda en atenderse una cancelación con una conexión atascada.
    let agent = ureq::builder()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(12))
        .build();
    let mut buffer = vec![0u8; READ_BUFFER_BYTES];
    let mut speed = 0.0f64;
    let mut consecutive = 0u32;
    loop {
        if engine.stopped() || !source.enabled() || engine.complete() {
            return;
        }
        let chunk = if speed > 0.0 {
            ((speed * engine.config.target_chunk_seconds) as u64)
                .clamp(engine.config.min_split, engine.config.max_chunk)
        } else {
            engine.config.initial_chunk
        };
        let Some(job) = engine.next_job(chunk, speed as u64) else {
            wait_unless_stopped(engine, Duration::from_millis(150));
            continue;
        };
        let begun = Instant::now();
        let outcome = fetch_range(engine, source, &agent, &file, &job, &mut buffer);
        engine.finish_job(&job);
        let received = job.pos.load(Ordering::Acquire).saturating_sub(job.start);
        if received > 0 {
            consecutive = 0;
            source.failures.store(0, Ordering::Release);
            let measured = received as f64 / begun.elapsed().as_secs_f64().max(0.05);
            speed = if speed == 0.0 {
                measured
            } else {
                0.6 * speed + 0.4 * measured
            };
        }
        match outcome {
            Ok(()) | Err(FetchError::Stopped) => {}
            Err(FetchError::Fatal(message)) => {
                engine.fail(message);
                return;
            }
            Err(FetchError::Unsupported(message)) => {
                if !source.disabled.swap(true, Ordering::AcqRel) {
                    engine.enabled_sources.fetch_sub(1, Ordering::AcqRel);
                    log_event(format!(
                        "WARNING etapa=descarga_segmentada fuente={} descartada motivo={message}",
                        source.label
                    ));
                }
                *source
                    .last_error
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = message;
                return;
            }
            Err(FetchError::Retry { message, refresh }) => {
                consecutive += 1;
                let failures = source.failures.fetch_add(1, Ordering::AcqRel) + 1;
                log_event(format!(
                    "WARNING etapa=descarga_segmentada fuente={} tramo={}-{} fallo={failures} motivo={message}",
                    source.label,
                    job.start,
                    job.end.load(Ordering::Acquire)
                ));
                *source
                    .last_error
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = message.clone();
                let silent_ms = engine
                    .elapsed_ms()
                    .saturating_sub(source.last_success_ms.load(Ordering::Acquire));
                // Descartar una fuente solo reparte su trabajo entre las demás; si es
                // la última, se espera más antes de dar la descarga por interrumpida
                // (cortes breves de la conexión del usuario).
                let grace = if engine.enabled_sources.load(Ordering::Acquire) > 1 {
                    engine.config.source_grace
                } else {
                    engine.config.source_grace * 4
                };
                if failures >= engine.config.max_source_failures
                    && silent_ms >= grace.as_millis() as u64
                {
                    if !source.disabled.swap(true, Ordering::AcqRel) {
                        engine.enabled_sources.fetch_sub(1, Ordering::AcqRel);
                        log_event(format!(
                            "WARNING etapa=descarga_segmentada fuente={} descartada tras {failures} fallos",
                            source.label
                        ));
                    }
                    return;
                }
                if refresh {
                    if let Some(resolve) = &source.refresh {
                        match resolve() {
                            Ok(url) => {
                                *source.url.lock().unwrap_or_else(PoisonError::into_inner) = url
                            }
                            Err(error) => log_event(format!(
                                "WARNING etapa=descarga_segmentada fuente={} renovar_enlace={error}",
                                source.label
                            )),
                        }
                    }
                }
                let delay = engine
                    .config
                    .retry_base
                    .saturating_mul(1 << consecutive.min(4).saturating_sub(1))
                    .min(Duration::from_secs(16));
                wait_unless_stopped(engine, delay);
            }
        }
    }
}

/// Calcula el MD5 en orden a medida que se completa el principio del archivo.
fn run_hasher(engine: &Engine) -> Result<md5::Digest, String> {
    let file =
        File::open(engine.part).map_err(|error| file_error("leer descarga", engine.part, error))?;
    let mut hash = md5::Context::new();
    let mut hashed = 0u64;
    let mut buffer = vec![0u8; HASH_BUFFER_BYTES];
    loop {
        if hashed >= engine.size {
            return Ok(hash.compute());
        }
        if engine.stopped() {
            return Err(DOWNLOAD_CANCELLED_MESSAGE.into());
        }
        let prefix = engine.plan().done.prefix_end();
        if hashed >= prefix {
            thread::sleep(Duration::from_millis(50));
            continue;
        }
        let length = ((prefix - hashed) as usize).min(buffer.len());
        read_exact_at(&file, &mut buffer[..length], hashed)
            .map_err(|error| file_error("leer descarga", engine.part, error))?;
        hash.consume(&buffer[..length]);
        hashed += length as u64;
        engine.hashed.store(hashed, Ordering::Release);
    }
}

fn report_progress(
    weak: &slint::Weak<MainWindow>,
    size: u64,
    received: u64,
    speed: f64,
    connections: usize,
) {
    let remaining = (size.saturating_sub(received) as f64 / speed.max(1.0)) as u64;
    let message = format!(
        "Descargando · {} / {} · {}/s · {} restantes · {} conexiones",
        human(received),
        human(size),
        human(speed as u64),
        format_duration(remaining),
        connections
    );
    let fraction = (received as f64 / size as f64).clamp(0.0, 1.0) as f32;
    ui(weak.clone(), move |window| {
        show_status(&window, message.into());
        window.set_progress(fraction);
    });
}

/// Descarga `iso` en `part` desde `sources`. Devuelve `Ok` solo si el archivo
/// está completo y su MD5 coincide con el del RSS.
pub(super) fn download_segmented(
    iso: &Iso,
    sources: &[Arc<Source>],
    part: &Path,
    weak: &slint::Weak<MainWindow>,
    download_state: &AtomicU8,
    config: SegmentConfig,
) -> Result<(), String> {
    let expected = iso
        .md5
        .clone()
        .ok_or("La descarga segmentada necesita el MD5 publicado")?;
    let done = prepare_part(part, iso)?;
    let resumed = done.total();
    let engine = Engine::new(iso.size, part, config, done, download_state);
    engine
        .enabled_sources
        .store(sources.len(), Ordering::Release);
    log_event(format!(
        "INFO etapa=descarga_segmentada inicio bytes={} reanudado={resumed} fuentes={}",
        iso.size,
        sources
            .iter()
            .map(|source| format!("{}x{}", source.label, source.connections))
            .collect::<Vec<_>>()
            .join(",")
    ));
    let outcome = thread::scope(|scope| {
        let engine = &engine;
        let spawn_worker = |source: &Arc<Source>| {
            source.live.fetch_add(1, Ordering::AcqRel);
            let source = source.clone();
            scope.spawn(move || run_worker(engine, &source));
        };
        let hasher = scope.spawn(move || run_hasher(engine));
        for source in sources {
            for _ in 0..source.connections {
                spawn_worker(source);
            }
        }
        let mut samples = VecDeque::from([(Instant::now(), engine.progress())]);
        let mut last_save = Instant::now();
        let result = loop {
            thread::sleep(Duration::from_millis(250));
            if download_state.load(Ordering::Acquire) == DOWNLOAD_CANCELLED {
                break Err(DOWNLOAD_CANCELLED_MESSAGE.to_owned());
            }
            if let Some(error) = engine
                .fatal
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
            {
                break Err(error);
            }
            if engine.complete() {
                break Ok(());
            }
            if sources.iter().all(|source| !source.enabled()) {
                let last_error = sources
                    .iter()
                    .map(|source| {
                        source
                            .last_error
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .clone()
                    })
                    .find(|error| !error.is_empty())
                    .unwrap_or_default();
                break Err(format!(
                    "Descarga interrumpida tras {}. Se ha guardado el progreso; vuelve a iniciar la descarga para reanudarla. Último error: {last_error}",
                    human(engine.plan().done.total())
                ));
            }
            // Si una fuente deja de responder, las demás abren más conexiones.
            if sources.iter().any(|source| !source.enabled()) {
                for source in sources.iter().filter(|source| source.enabled()) {
                    while source.live.load(Ordering::Acquire) < source.max_connections {
                        spawn_worker(source);
                    }
                }
            }
            let now = Instant::now();
            let received = engine.progress();
            samples.push_back((now, received));
            while samples.len() > 2 && now.duration_since(samples[0].0) > SPEED_WINDOW {
                samples.pop_front();
            }
            let (since, from) = samples[0];
            let speed = received.saturating_sub(from) as f64
                / now.duration_since(since).as_secs_f64().max(0.25);
            let connections = sources
                .iter()
                .map(|source| source.live.load(Ordering::Acquire))
                .sum();
            report_progress(weak, iso.size, received, speed, connections);
            if last_save.elapsed() >= STATE_SAVE_INTERVAL {
                if let Err(error) = save_progress(part, iso, &engine.plan().done) {
                    log_event(format!("WARNING etapa=descarga_segmentada {error}"));
                }
                last_save = Instant::now();
            }
        };
        if let Err(error) = result {
            engine.stop.store(true, Ordering::Release);
            return Err(error);
        }
        status(weak.clone(), "Verificando la integridad de la ISO…");
        while !hasher.is_finished() {
            if download_state.load(Ordering::Acquire) == DOWNLOAD_CANCELLED {
                engine.stop.store(true, Ordering::Release);
                break;
            }
            let hashed = engine.hashed.load(Ordering::Acquire);
            let fraction = (hashed as f64 / iso.size as f64).clamp(0.0, 1.0) as f32;
            let message = format!(
                "Verificando la integridad de la ISO · {} / {}",
                human(hashed),
                human(iso.size)
            );
            ui(weak.clone(), move |window| {
                show_status(&window, message.into());
                window.set_progress(fraction);
            });
            thread::sleep(Duration::from_millis(250));
        }
        hasher
            .join()
            .map_err(|_| "Falló el cálculo del MD5 de la descarga".to_owned())?
    });
    let seconds = engine.started.elapsed().as_secs_f64();
    log_event(format!(
        "INFO etapa=descarga_segmentada tramos_rescatados={}",
        engine.rescues.load(Ordering::Relaxed)
    ));
    for source in sources {
        log_event(format!(
            "INFO etapa=descarga_segmentada fuente={} bytes={} activa={}",
            source.label,
            source.delivered.load(Ordering::Acquire),
            source.enabled()
        ));
    }
    let digest = match outcome {
        Ok(digest) => digest,
        Err(error) => {
            if error != DOWNLOAD_CANCELLED_MESSAGE {
                if let Err(save) = save_progress(part, iso, &engine.plan().done) {
                    log_event(format!("WARNING etapa=descarga_segmentada {save}"));
                }
            }
            return Err(error);
        }
    };
    let actual = format!("{digest:x}");
    if !actual.eq_ignore_ascii_case(&expected) {
        log_event(format!(
            "ERROR etapa=verificar_iso md5_esperado={expected} md5_obtenido={actual}"
        ));
        if sources
            .iter()
            .any(|source| source.alternative && source.delivered.load(Ordering::Acquire) > 0)
        {
            ALTERNATIVE_DISTRUSTED.store(true, Ordering::Release);
            log_event("WARNING etapa=descarga_segmentada alternativa_desactivada_por_md5".into());
        }
        let _ = fs::remove_file(part);
        let _ = fs::remove_file(state_path(part));
        return Err(
            "La suma MD5 no coincide con la publicada por SourceForge. Se ha descartado la descarga; vuelve a iniciarla para repetirla."
                .into(),
        );
    }
    OpenOptions::new()
        .write(true)
        .open(part)
        .and_then(|file| file.sync_all())
        .map_err(|error| file_error("guardar descarga", part, error))?;
    let _ = fs::remove_file(state_path(part));
    let downloaded = iso.size.saturating_sub(resumed);
    log_event(format!(
        "INFO etapa=descarga_segmentada completada bytes={downloaded} segundos={seconds:.1} velocidad_media={}/s",
        human((downloaded as f64 / seconds.max(0.001)) as u64)
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// Fuentes de descarga
// ---------------------------------------------------------------------------

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Nombre del archivo en `Content-Disposition` (forma `filename="..."`).
fn disposition_filename(header: &str) -> Option<String> {
    header.split(';').find_map(|part| {
        let value = part.trim().strip_prefix("filename=")?;
        Some(value.trim_matches('"').to_owned())
    })
}

/// Convierte un enlace de Google Drive en su descarga directa. `confirm=t`
/// evita la página de aviso de archivos que Drive no puede analizar.
pub(super) fn direct_download_url(link: &url::Url) -> String {
    let host = link.host_str().unwrap_or("");
    if matches!(
        host,
        "drive.google.com" | "docs.google.com" | "drive.usercontent.google.com"
    ) {
        let id = link
            .query_pairs()
            .find(|(key, _)| key == "id")
            .map(|(_, value)| value.into_owned())
            .or_else(|| {
                let segments = link.path_segments()?.collect::<Vec<_>>();
                let position = segments.iter().position(|segment| *segment == "d")?;
                segments.get(position + 1).map(|id| (*id).to_owned())
            })
            .filter(|id| {
                !id.is_empty()
                    && id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            });
        if let Some(id) = id {
            return format!(
                "https://drive.usercontent.google.com/download?id={id}&export=download&confirm=t"
            );
        }
    }
    link.to_string()
}

/// Comprueba que `url` sirve rangos de esta misma ISO y devuelve la URL final
/// tras las redirecciones.
fn probe_range_source(url: &str, iso: &Iso) -> Result<String, String> {
    let response = ureq::builder()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(15))
        .build()
        .get(url)
        .set("User-Agent", USER_AGENT)
        .set("Accept-Encoding", "identity")
        .set("Range", "bytes=0-0")
        .call()
        .map_err(|error| describe_http_error(error).0)?;
    let final_url = response.get_url().to_owned();
    if host_of(&final_url) == "accounts.google.com" {
        return Err("requiere iniciar sesión".into());
    }
    if response
        .header("Content-Type")
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/html"))
    {
        return Err("devolvió una página HTML".into());
    }
    if response.status() != 206 {
        return Err(format!(
            "sin soporte de rangos (HTTP {})",
            response.status()
        ));
    }
    if response.header("Content-Range") != Some(format!("bytes 0-0/{}", iso.size).as_str()) {
        return Err(format!(
            "tamaño distinto al publicado ({:?})",
            response.header("Content-Range")
        ));
    }
    if let Some(name) = response
        .header("Content-Disposition")
        .and_then(disposition_filename)
    {
        if !name.eq_ignore_ascii_case(&iso.name) {
            return Err(format!("contiene otra ISO ({name})"));
        }
    }
    let mut byte = [0u8; 1];
    if response
        .into_reader()
        .read(&mut byte)
        .map_err(|error| error.to_string())?
        != 1
    {
        return Err("no entregó datos".into());
    }
    Ok(final_url)
}

fn mirror_url(iso: &Iso, mirror: Option<&str>) -> String {
    match mirror {
        Some(mirror) => format!("{}?use_mirror={mirror}", iso.url),
        None => iso.url.clone(),
    }
}

fn is_sourceforge_download(url: &str, iso: &Iso) -> bool {
    let host = host_of(url);
    (host == "sourceforge.net" || host.ends_with(".sourceforge.net"))
        && url.contains(&format!("/project/winslim11-isos/{}", iso.name))
}

/// Espejos de SourceForge que sirven la ISO, sin repetir servidor: varios
/// espejos pueden redirigir al mismo (`master`).
fn sourceforge_endpoints(iso: &Iso) -> Vec<(Option<String>, String, String)> {
    let mut candidates = match available_mirrors(iso) {
        Ok(mirrors) => mirrors
            .into_iter()
            .take(MAX_MIRROR_PROBES)
            .map(Some)
            .collect::<Vec<_>>(),
        Err(error) => {
            log_event(format!("WARNING etapa=fuentes_descarga espejos={error}"));
            Vec::new()
        }
    };
    if candidates.is_empty() {
        candidates.push(None);
    }
    let probed = thread::scope(|scope| {
        candidates
            .iter()
            .map(|mirror| {
                scope.spawn(move || {
                    (
                        mirror.clone(),
                        probe_range_source(&mirror_url(iso, mirror.as_deref()), iso),
                    )
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|probe| probe.join().ok())
            .collect::<Vec<_>>()
    });
    let mut hosts = HashSet::new();
    let mut endpoints = Vec::new();
    for (mirror, outcome) in probed {
        match outcome {
            Ok(url) if is_sourceforge_download(&url, iso) => {
                let host = host_of(&url);
                if hosts.insert(host.clone()) {
                    endpoints.push((mirror, host, url));
                }
            }
            Ok(url) => log_event(format!(
                "WARNING etapa=fuentes_descarga espejo={mirror:?} destino_inesperado={}",
                url.split('?').next().unwrap_or("")
            )),
            Err(error) => log_event(format!(
                "INFO etapa=fuentes_descarga espejo={mirror:?} no_disponible={error}"
            )),
        }
    }
    if endpoints.is_empty() {
        // Último recurso: el enlace resuelto como en la descarga de una conexión.
        if let Ok(url) = resolve_download(iso).and_then(|url| probe_range_source(&url, iso)) {
            if is_sourceforge_download(&url, iso) {
                endpoints.push((None, host_of(&url), url));
            }
        }
    }
    endpoints
}

fn alternative_endpoint(iso: &Iso) -> Option<String> {
    if ALTERNATIVE_DISTRUSTED.load(Ordering::Acquire) {
        log_event("INFO etapa=fuentes_descarga alternativa=descartada_por_md5".into());
        return None;
    }
    let link = match fetch_alternative_link() {
        Ok(link) => link,
        Err(error) => {
            log_event(format!(
                "INFO etapa=fuentes_descarga alternativa_no_disponible={error}"
            ));
            return None;
        }
    };
    match probe_range_source(&direct_download_url(&link), iso) {
        Ok(url) => Some(url),
        Err(error) => {
            log_event(format!(
                "INFO etapa=fuentes_descarga alternativa={} no_valida={error}",
                link.host_str().unwrap_or("")
            ));
            None
        }
    }
}

/// Reúne en paralelo las fuentes que sirven rangos de esta ISO.
pub(super) fn discover_sources(iso: &Iso) -> Vec<Arc<Source>> {
    let (sourceforge, alternative) = thread::scope(|scope| {
        let sourceforge = scope.spawn(|| sourceforge_endpoints(iso));
        let alternative = scope.spawn(|| alternative_endpoint(iso));
        (
            sourceforge.join().unwrap_or_default(),
            alternative.join().ok().flatten(),
        )
    });
    let solo = (SOURCEFORGE_CONNECTIONS / sourceforge.len().max(1)).max(4);
    let shared = if alternative.is_some() {
        SOURCEFORGE_CONNECTIONS_WITH_ALTERNATIVE.min(solo)
    } else {
        solo
    };
    let mut sources = Vec::new();
    for (mirror, host, url) in sourceforge {
        let iso_for_refresh = iso.clone();
        let refresh: Refresh = Box::new(move || {
            probe_range_source(
                &mirror_url(&iso_for_refresh, mirror.as_deref()),
                &iso_for_refresh,
            )
        });
        sources.push(Arc::new(Source::new(
            format!("sourceforge:{host}"),
            url,
            shared,
            solo,
            false,
            Some(refresh),
        )));
    }
    if let Some(url) = alternative {
        sources.push(Arc::new(Source::new(
            format!("alternativa:{}", host_of(&url)),
            url,
            ALTERNATIVE_CONNECTIONS,
            ALTERNATIVE_CONNECTIONS,
            true,
            None,
        )));
    }
    log_event(format!(
        "INFO etapa=fuentes_descarga encontradas={}",
        sources
            .iter()
            .map(|source| source.label.as_str())
            .collect::<Vec<_>>()
            .join(",")
    ));
    sources
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    #[derive(Clone, Copy, Default)]
    struct Behaviour {
        /// Pausa tras cada bloque de 16 KiB (servidor lento).
        delay: Option<Duration>,
        /// Espera antes de responder (latencia del servidor).
        header_delay: Option<Duration>,
        ignore_range: bool,
    }

    struct MockServer {
        url: String,
        stop: Arc<AtomicBool>,
        served: Arc<Mutex<Vec<(u64, u64)>>>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn handle_connection(
        mut stream: TcpStream,
        data: &[u8],
        behaviour: Behaviour,
        served: &Mutex<Vec<(u64, u64)>>,
        stop: &AtomicBool,
    ) {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0u8; 1024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(length) => request.extend_from_slice(&buffer[..length]),
            }
        }
        let request = String::from_utf8_lossy(&request).into_owned();
        let range = request.lines().find_map(|line| {
            let value = line.strip_prefix("Range: bytes=")?;
            let (start, end) = value.trim().split_once('-')?;
            Some((start.parse::<u64>().ok()?, end.parse::<u64>().ok()?))
        });
        let (status, start, end) = match range {
            Some((start, end)) if !behaviour.ignore_range => ("206 Partial Content", start, end),
            _ => ("200 OK", 0, data.len() as u64 - 1),
        };
        served.lock().unwrap().push((start, end + 1));
        if let Some(delay) = behaviour.header_delay {
            thread::sleep(delay);
        }
        let mut header = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n",
            end - start + 1
        );
        if status.starts_with("206") {
            header.push_str(&format!(
                "Content-Range: bytes {start}-{end}/{}\r\n",
                data.len()
            ));
        }
        header.push_str("\r\n");
        if stream.write_all(header.as_bytes()).is_err() {
            return;
        }
        for block in data[start as usize..=end as usize].chunks(16 * 1024) {
            if stop.load(Ordering::Acquire) || stream.write_all(block).is_err() {
                return;
            }
            if let Some(delay) = behaviour.delay {
                thread::sleep(delay);
            }
        }
    }

    fn mock_server(data: Arc<Vec<u8>>, behaviour: Behaviour) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/test.iso", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let served = Arc::new(Mutex::new(Vec::new()));
        let handle = {
            let stop = stop.clone();
            let served = served.clone();
            thread::spawn(move || {
                let mut connections = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            stream.set_nonblocking(false).unwrap();
                            let data = data.clone();
                            let served = served.clone();
                            let stop = stop.clone();
                            connections.push(thread::spawn(move || {
                                handle_connection(stream, &data, behaviour, &served, &stop)
                            }));
                        }
                        Err(_) => thread::sleep(Duration::from_millis(5)),
                    }
                }
                for connection in connections {
                    let _ = connection.join();
                }
            })
        };
        MockServer {
            url,
            stop,
            served,
            handle: Some(handle),
        }
    }

    fn test_data(size: usize) -> Arc<Vec<u8>> {
        Arc::new(
            (0..size)
                .map(|index| ((index * 131 + index / 7) % 251) as u8)
                .collect(),
        )
    }

    fn test_iso(data: &[u8]) -> Iso {
        Iso {
            name: "test.iso".into(),
            url: String::new(),
            size: data.len() as u64,
            md5: Some(format!("{:x}", md5::compute(data))),
            published: 0,
        }
    }

    fn test_config() -> SegmentConfig {
        SegmentConfig {
            min_split: 64 * 1024,
            initial_chunk: 256 * 1024,
            max_chunk: 1024 * 1024,
            target_chunk_seconds: 1.0,
            max_source_failures: 3,
            retry_base: Duration::from_millis(50),
            source_grace: Duration::from_millis(200),
            steal_after: Duration::from_millis(300),
        }
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "winslim-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn source(server: &MockServer, connections: usize, alternative: bool) -> Arc<Source> {
        Arc::new(Source::new(
            server.url.clone(),
            server.url.clone(),
            connections,
            connections,
            alternative,
            None,
        ))
    }

    #[test]
    fn ranges_merge_and_report_gaps() {
        let mut ranges = Ranges::default();
        ranges.insert(10, 20);
        ranges.insert(30, 40);
        ranges.insert(20, 25);
        assert_eq!(ranges.0, vec![(10, 25), (30, 40)]);
        assert_eq!(ranges.missing(50), vec![(0, 10), (25, 30), (40, 50)]);
        assert_eq!(ranges.prefix_end(), 0);
        ranges.insert(0, 12);
        ranges.insert(22, 35);
        assert_eq!(ranges.0, vec![(0, 40)]);
        assert_eq!(ranges.prefix_end(), 40);
        assert_eq!(ranges.total(), 40);
        ranges.insert(5, 5);
        assert_eq!(ranges.0, vec![(0, 40)]);
    }

    #[test]
    fn google_drive_links_become_direct_downloads() {
        let expected =
            "https://drive.usercontent.google.com/download?id=abc_123-X&export=download&confirm=t";
        for link in [
            "https://docs.google.com/uc?export=download&id=abc_123-X",
            "https://drive.google.com/file/d/abc_123-X/view?usp=sharing",
            "https://drive.google.com/uc?id=abc_123-X",
        ] {
            assert_eq!(
                direct_download_url(&url::Url::parse(link).unwrap()),
                expected
            );
        }
        let other = url::Url::parse("https://example.org/WinSlim.iso").unwrap();
        assert_eq!(
            direct_download_url(&other),
            "https://example.org/WinSlim.iso"
        );
        assert_eq!(
            disposition_filename("attachment; filename=\"WinSlim.iso\"").as_deref(),
            Some("WinSlim.iso")
        );
    }

    #[test]
    fn fast_and_slow_sources_share_the_download() {
        let data = test_data(3 * 1024 * 1024 + 4321);
        let fast = mock_server(data.clone(), Behaviour::default());
        let slow = mock_server(
            data.clone(),
            Behaviour {
                delay: Some(Duration::from_millis(20)),
                ..Behaviour::default()
            },
        );
        let dir = temp_dir("segmented");
        let part = dir.join("test.iso.part");
        let iso = test_iso(&data);
        let sources = [source(&slow, 3, false), source(&fast, 2, true)];
        let state = AtomicU8::new(DOWNLOAD_RUNNING);
        download_segmented(
            &iso,
            &sources,
            &part,
            &slint::Weak::default(),
            &state,
            test_config(),
        )
        .unwrap();
        assert_eq!(fs::read(&part).unwrap(), *data);
        assert!(!state_path(&part).exists());
        assert!(sources[1].delivered.load(Ordering::Acquire) > 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn last_chunk_is_rescued_once_and_only_when_stalled() {
        const MIB: u64 = 1024 * 1024;
        let mut done = Ranges::default();
        done.insert(2 * MIB, 10 * MIB);
        let state = AtomicU8::new(DOWNLOAD_RUNNING);
        let config = SegmentConfig {
            min_split: 4 * MIB,
            steal_after: Duration::from_millis(100),
            ..test_config()
        };
        let engine = Engine::new(10 * MIB, Path::new("unused"), config, done, &state);
        let fast = 50 * MIB;
        let first = engine.next_job(2 * MIB, 0).unwrap();
        assert_eq!(
            (first.start, first.end.load(Ordering::Acquire)),
            (0, 2 * MIB)
        );
        // Un tramo recién pedido aún espera respuesta: no se le quita a nadie.
        assert!(engine.next_job(2 * MIB, fast).is_none());
        thread::sleep(Duration::from_millis(150));
        // Sin datos tras el plazo: una conexión rápida lo rescata entero.
        let rescue = engine.next_job(2 * MIB, fast).unwrap();
        assert!(rescue.rescue);
        assert_eq!(first.end.load(Ordering::Acquire), 0);
        assert_eq!(rescue.end.load(Ordering::Acquire), 2 * MIB);
        // El rescate no se vuelve a rescatar aunque tarde en responder.
        thread::sleep(Duration::from_millis(150));
        assert!(engine.next_job(2 * MIB, fast).is_none());
        // Una conexión lenta tampoco rescata tramos de otra.
        assert!(engine.next_job(2 * MIB, 1).is_none());
        assert_eq!(engine.rescues.load(Ordering::Relaxed), 1);
        engine.finish_job(&first);
        rescue.pos.store(2 * MIB, Ordering::Release);
        engine.finish_job(&rescue);
        assert!(engine.complete());
    }

    #[test]
    fn idle_connections_do_not_pass_the_last_chunk_around() {
        // Muchas conexiones rápidas con latencia, como Drive, y una fuente lenta
        // con la cola: antes se robaban el último tramo entre sí sin descargarlo.
        let data = test_data(8 * 1024 * 1024);
        let fast = mock_server(
            data.clone(),
            Behaviour {
                header_delay: Some(Duration::from_millis(400)),
                ..Behaviour::default()
            },
        );
        let slow = mock_server(
            data.clone(),
            Behaviour {
                delay: Some(Duration::from_millis(60)),
                ..Behaviour::default()
            },
        );
        let dir = temp_dir("segmented-tail");
        let part = dir.join("test.iso.part");
        let iso = test_iso(&data);
        let sources = [source(&slow, 2, false), source(&fast, 12, true)];
        let state = Arc::new(AtomicU8::new(DOWNLOAD_RUNNING));
        let finished = Arc::new(AtomicBool::new(false));
        let watchdog = {
            let state = state.clone();
            let finished = finished.clone();
            thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(15);
                while !finished.load(Ordering::Acquire) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(50));
                }
                let _ = state.compare_exchange(
                    DOWNLOAD_RUNNING,
                    DOWNLOAD_CANCELLED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            })
        };
        // Tramos de 1-2 MiB: las conexiones rápidas miden varios MB/s, más de
        // cuatro veces la fuente lenta, como Drive frente a SourceForge.
        let config = SegmentConfig {
            initial_chunk: 1024 * 1024,
            max_chunk: 2 * 1024 * 1024,
            ..test_config()
        };
        let result = download_segmented(
            &iso,
            &sources,
            &part,
            &slint::Weak::default(),
            &state,
            config,
        );
        finished.store(true, Ordering::Release);
        watchdog.join().unwrap();
        let requests = fast.served.lock().unwrap().len();
        result.expect("la descarga se bloqueó en el último tramo");
        assert_eq!(fs::read(&part).unwrap(), *data);
        assert!(requests < 100, "demasiadas peticiones: {requests}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn source_ignoring_ranges_is_dropped() {
        let data = test_data(1024 * 1024);
        let good = mock_server(data.clone(), Behaviour::default());
        let bad = mock_server(
            data.clone(),
            Behaviour {
                ignore_range: true,
                ..Behaviour::default()
            },
        );
        let dir = temp_dir("segmented-bad");
        let part = dir.join("test.iso.part");
        let iso = test_iso(&data);
        let sources = [source(&bad, 2, false), source(&good, 2, false)];
        let state = AtomicU8::new(DOWNLOAD_RUNNING);
        download_segmented(
            &iso,
            &sources,
            &part,
            &slint::Weak::default(),
            &state,
            test_config(),
        )
        .unwrap();
        assert_eq!(fs::read(&part).unwrap(), *data);
        assert!(!sources[0].enabled());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn saved_ranges_are_not_downloaded_again() {
        let data = test_data(2 * 1024 * 1024);
        let server = mock_server(data.clone(), Behaviour::default());
        let dir = temp_dir("segmented-resume");
        let part = dir.join("test.iso.part");
        let iso = test_iso(&data);
        // Descarga anterior interrumpida con dos tramos completos.
        let file = File::create(&part).unwrap();
        file.set_len(iso.size).unwrap();
        write_all_at(&file, &data[..512 * 1024], 0).unwrap();
        write_all_at(&file, &data[1024 * 1024..1536 * 1024], 1024 * 1024).unwrap();
        drop(file);
        let mut done = Ranges::default();
        done.insert(0, 512 * 1024);
        done.insert(1024 * 1024, 1536 * 1024);
        save_progress(&part, &iso, &done).unwrap();
        let sources = [source(&server, 2, false)];
        let state = AtomicU8::new(DOWNLOAD_RUNNING);
        download_segmented(
            &iso,
            &sources,
            &part,
            &slint::Weak::default(),
            &state,
            test_config(),
        )
        .unwrap();
        assert_eq!(fs::read(&part).unwrap(), *data);
        let served = server.served.lock().unwrap().clone();
        let mut fetched = Ranges::default();
        for (start, end) in served {
            let overlaps = |a: u64, b: u64| start < b && end > a;
            assert!(!overlaps(0, 512 * 1024) && !overlaps(1024 * 1024, 1536 * 1024));
            fetched.insert(start, end);
        }
        assert_eq!(fetched.total(), 1024 * 1024);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_partial_download_is_adopted_and_converted_back() {
        let data = test_data(256 * 1024);
        let dir = temp_dir("segmented-legacy");
        let part = dir.join("test.iso.part");
        let iso = test_iso(&data);
        fs::write(&part, &data[..100_000]).unwrap();
        let done = prepare_part(&part, &iso).unwrap();
        assert_eq!(done.0, vec![(0, 100_000)]);
        assert_eq!(fs::metadata(&part).unwrap().len(), iso.size);
        convert_to_contiguous(&part).unwrap();
        assert_eq!(fs::read(&part).unwrap(), data[..100_000]);
        assert!(!state_path(&part).exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn wrong_md5_discards_the_download() {
        let data = test_data(512 * 1024);
        let server = mock_server(data.clone(), Behaviour::default());
        let dir = temp_dir("segmented-md5");
        let part = dir.join("test.iso.part");
        let mut iso = test_iso(&data);
        iso.md5 = Some("0".repeat(32));
        let sources = [source(&server, 2, false)];
        let state = AtomicU8::new(DOWNLOAD_RUNNING);
        assert!(download_segmented(
            &iso,
            &sources,
            &part,
            &slint::Weak::default(),
            &state,
            test_config(),
        )
        .is_err());
        assert!(!part.exists());
        assert!(!state_path(&part).exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cancelling_stops_every_connection() {
        let data = test_data(4 * 1024 * 1024);
        let server = mock_server(
            data.clone(),
            Behaviour {
                delay: Some(Duration::from_millis(30)),
                ..Behaviour::default()
            },
        );
        let dir = temp_dir("segmented-cancel");
        let part = dir.join("test.iso.part");
        let iso = test_iso(&data);
        let sources = [source(&server, 3, false)];
        let state = Arc::new(AtomicU8::new(DOWNLOAD_RUNNING));
        let canceller = {
            let state = state.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(400));
                state.store(DOWNLOAD_CANCELLED, Ordering::Release);
            })
        };
        let began = Instant::now();
        let result = download_segmented(
            &iso,
            &sources,
            &part,
            &slint::Weak::default(),
            &state,
            test_config(),
        );
        canceller.join().unwrap();
        assert_eq!(result.unwrap_err(), DOWNLOAD_CANCELLED_MESSAGE);
        assert!(began.elapsed() < Duration::from_secs(5));
        fs::remove_dir_all(dir).unwrap();
    }

    /// Descarga cualquier URL con rangos para diagnosticar el motor en red real:
    /// `WINSLIM_TEST_URL`, `WINSLIM_TEST_MD5` y `WINSLIM_TEST_CONNECTIONS`.
    #[test]
    #[ignore = "downloads WINSLIM_TEST_URL from the network"]
    fn real_url_download_completes_and_verifies() {
        let (Ok(url), Ok(md5)) = (
            std::env::var("WINSLIM_TEST_URL"),
            std::env::var("WINSLIM_TEST_MD5"),
        ) else {
            println!("Define WINSLIM_TEST_URL y WINSLIM_TEST_MD5");
            return;
        };
        let connections = std::env::var("WINSLIM_TEST_CONNECTIONS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(8);
        let response = ureq::get(&url).set("Range", "bytes=0-0").call().unwrap();
        let size = response
            .header("Content-Range")
            .and_then(|value| value.rsplit_once('/'))
            .and_then(|(_, total)| total.parse::<u64>().ok())
            .unwrap();
        let iso = Iso {
            name: "test.iso".into(),
            url: url.clone(),
            size,
            md5: Some(md5),
            published: 0,
        };
        let sources = [Arc::new(Source::new(
            "prueba",
            url,
            connections,
            connections,
            false,
            None,
        ))];
        let dir = temp_dir("real-url");
        let part = dir.join("test.iso.part");
        let state = AtomicU8::new(DOWNLOAD_RUNNING);
        let began = Instant::now();
        let result = download_segmented(
            &iso,
            &sources,
            &part,
            &slint::Weak::default(),
            &state,
            SegmentConfig::default(),
        );
        println!(
            "resultado={result:?} bytes={size} segundos={:.1} media={:.1} MB/s",
            began.elapsed().as_secs_f64(),
            size as f64 / 1e6 / began.elapsed().as_secs_f64()
        );
        fs::remove_dir_all(dir).unwrap();
        result.unwrap();
    }

    /// Escenario Drive + SourceForge en red real: `WINSLIM_TEST_URL` (servidor
    /// rápido) y `WINSLIM_TEST_FILE` (copia local del mismo archivo, que sirve un
    /// espejo local lento a ~250 KB/s por conexión). Mide cuánto tarda el final.
    #[test]
    #[ignore = "downloads WINSLIM_TEST_URL from the network"]
    fn real_fast_source_with_slow_mirror() {
        let (Ok(url), Ok(file)) = (
            std::env::var("WINSLIM_TEST_URL"),
            std::env::var("WINSLIM_TEST_FILE"),
        ) else {
            println!("Define WINSLIM_TEST_URL y WINSLIM_TEST_FILE");
            return;
        };
        let data = Arc::new(fs::read(file).unwrap());
        let slow = mock_server(
            data.clone(),
            Behaviour {
                delay: Some(Duration::from_millis(60)),
                ..Behaviour::default()
            },
        );
        let iso = test_iso(&data);
        let fast = Arc::new(Source::new("rapida", url, 12, 12, true, None));
        let sources = [source(&slow, 4, false), fast];
        let dir = temp_dir("real-hybrid");
        let part = dir.join("test.iso.part");
        let state = Arc::new(AtomicU8::new(DOWNLOAD_RUNNING));
        let sampler = {
            let state = state.clone();
            let sources = sources.clone();
            let size = iso.size;
            thread::spawn(move || {
                let start = Instant::now();
                while state.load(Ordering::Acquire) == DOWNLOAD_RUNNING {
                    thread::sleep(Duration::from_millis(500));
                    let total: u64 = sources
                        .iter()
                        .map(|source| source.delivered.load(Ordering::Acquire))
                        .sum();
                    println!(
                        "t={:>5.1}s {:>5.1}% lenta={:.1}MB rapida={:.1}MB",
                        start.elapsed().as_secs_f64(),
                        total as f64 * 100.0 / size as f64,
                        sources[0].delivered.load(Ordering::Acquire) as f64 / 1e6,
                        sources[1].delivered.load(Ordering::Acquire) as f64 / 1e6
                    );
                }
            })
        };
        let began = Instant::now();
        let result = download_segmented(
            &iso,
            &sources,
            &part,
            &slint::Weak::default(),
            &state,
            SegmentConfig::default(),
        );
        state.store(DOWNLOAD_IDLE, Ordering::Release);
        sampler.join().unwrap();
        println!(
            "resultado={result:?} segundos={:.1} media={:.1} MB/s",
            began.elapsed().as_secs_f64(),
            iso.size as f64 / 1e6 / began.elapsed().as_secs_f64()
        );
        fs::remove_dir_all(dir).unwrap();
        result.unwrap();
    }

    /// Mide la velocidad real con los servidores publicados. Duración en
    /// segundos con `WINSLIM_SPEED_SECONDS` (60 por defecto); 0 = hasta terminar.
    #[test]
    #[ignore = "downloads the real ISO from the public servers"]
    fn real_download_throughput() {
        let iso = fetch_latest().unwrap();
        let seconds = std::env::var("WINSLIM_SPEED_SECONDS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(60);
        let began = Instant::now();
        let sources = discover_sources(&iso);
        println!(
            "fuentes en {:.1} s: {:?}",
            began.elapsed().as_secs_f64(),
            sources
                .iter()
                .map(|source| format!("{} x{}", source.label, source.connections))
                .collect::<Vec<_>>()
        );
        assert!(!sources.is_empty());
        let dir = temp_dir("real-speed");
        let part = dir.join(format!("{}.part", iso.name));
        let state = Arc::new(AtomicU8::new(DOWNLOAD_RUNNING));
        let sampler = {
            let state = state.clone();
            let sources = sources.clone();
            thread::spawn(move || {
                let start = Instant::now();
                let mut previous = 0u64;
                let mut previous_at = Instant::now();
                while state.load(Ordering::Acquire) == DOWNLOAD_RUNNING {
                    thread::sleep(Duration::from_secs(5));
                    let total: u64 = sources
                        .iter()
                        .map(|source| source.delivered.load(Ordering::Acquire))
                        .sum();
                    let speed =
                        (total - previous) as f64 / previous_at.elapsed().as_secs_f64() / 1e6;
                    println!(
                        "t={:>3}s total={:>7.1} MB velocidad={speed:>5.1} MB/s {}",
                        start.elapsed().as_secs(),
                        total as f64 / 1e6,
                        sources
                            .iter()
                            .map(|source| format!(
                                "{}={:.1}MB/{}c",
                                source.label,
                                source.delivered.load(Ordering::Acquire) as f64 / 1e6,
                                source.live.load(Ordering::Acquire)
                            ))
                            .collect::<Vec<_>>()
                            .join(" ")
                    );
                    previous = total;
                    previous_at = Instant::now();
                    if seconds > 0 && start.elapsed() >= Duration::from_secs(seconds) {
                        let _ = state.compare_exchange(
                            DOWNLOAD_RUNNING,
                            DOWNLOAD_CANCELLED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                    }
                }
            })
        };
        let started = Instant::now();
        let result = download_segmented(
            &iso,
            &sources,
            &part,
            &slint::Weak::default(),
            &state,
            SegmentConfig::default(),
        );
        state.store(DOWNLOAD_IDLE, Ordering::Release);
        sampler.join().unwrap();
        let total: u64 = sources
            .iter()
            .map(|source| source.delivered.load(Ordering::Acquire))
            .sum();
        println!(
            "resultado={result:?} media={:.1} MB/s en {:.0} s",
            total as f64 / 1e6 / started.elapsed().as_secs_f64(),
            started.elapsed().as_secs_f64()
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
