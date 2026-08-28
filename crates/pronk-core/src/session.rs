//! Tokio integration for authoritative logind session activity.

use std::ffi::{c_char, c_int, CStr, CString};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr::NonNull;

use nix::errno::Errno;
use nix::libc;
use thiserror::Error;
use tokio::io::unix::AsyncFd;

// Bound strings read from logind and procfs before retaining them in the
// caller-identity model.
pub const MAX_LOGIN_SESSION_ID_BYTES: usize = 64;
pub const MAX_SEAT_ID_BYTES: usize = 128;
const MAX_ACTIVE_LOGIN_SESSIONS: usize = 64;
const MAX_PROC_STAT_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicalSessionType {
    X11,
    Wayland,
    Mir,
}

#[derive(Debug)]
pub struct PinnedCallerSession {
    pid: u32,
    uid: u32,
    process_start_time_ticks: u64,
    session_id: String,
    seat: String,
    session_type: GraphicalSessionType,
    pidfd: AsyncFd<OwnedFd>,
}

impl PinnedCallerSession {
    /// Pin one same-user process and resolve its active local graphical login.
    ///
    /// Desktop applications commonly run in the shared user manager rather
    /// than the original session cgroup. We first accept a directly associated
    /// graphical session, then fall back to the caller UID's unique active
    /// local graphical session. Ambiguity fails closed.
    pub fn pin(pid: u32, uid: u32, expected_uid: u32) -> Result<Self, CallerSessionError> {
        let (pinned, session) = pin_caller_identity(pid, uid, expected_uid)?;
        Self::from_identity(pinned, session)
    }

    /// Pin a caller without performing procfs and sd-login reads on an async
    /// runtime worker.
    pub async fn pin_async(
        pid: u32,
        uid: u32,
        expected_uid: u32,
    ) -> Result<Self, CallerSessionError> {
        let (pinned, session) =
            tokio::task::spawn_blocking(move || pin_caller_identity(pid, uid, expected_uid))
                .await
                .map_err(|error| CallerSessionError::PinTask(error.to_string()))??;
        Self::from_identity(pinned, session)
    }

    fn from_identity(
        pinned: PinnedProcess,
        session: GraphicalSession,
    ) -> Result<Self, CallerSessionError> {
        let pid = pinned.pid;
        let process_start_time_ticks = pinned.start_time_ticks;
        let pidfd = AsyncFd::new(pinned.pidfd).map_err(CallerSessionError::RegisterPidFd)?;
        Ok(Self {
            pid,
            uid: session.uid,
            process_start_time_ticks,
            session_id: session.id,
            seat: session.seat,
            session_type: session.session_type,
            pidfd,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn uid(&self) -> u32 {
        self.uid
    }

    pub fn process_start_time_ticks(&self) -> u64 {
        self.process_start_time_ticks
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn seat(&self) -> &str {
        &self.seat
    }

    pub fn session_type(&self) -> GraphicalSessionType {
        self.session_type
    }

    /// Complete when the exact pidfd-pinned caller exits.
    pub async fn wait_for_exit(&self) -> Result<(), io::Error> {
        let mut readiness = self.pidfd.readable().await?;
        readiness.clear_ready();
        Ok(())
    }
}

#[derive(Debug)]
struct PinnedProcess {
    pid: u32,
    start_time_ticks: u64,
    pidfd: OwnedFd,
}

impl PinnedProcess {
    fn pin(pid: u32, uid: u32, expected_uid: u32) -> Result<Self, CallerSessionError> {
        if uid != expected_uid {
            return Err(CallerSessionError::WrongUid {
                expected: expected_uid,
                actual: uid,
            });
        }
        let native_pid = i32::try_from(pid).map_err(|_| CallerSessionError::InvalidPid(pid))?;
        if native_pid <= 0 {
            return Err(CallerSessionError::InvalidPid(pid));
        }
        let start_time_ticks = process_start_time_ticks(pid)?;
        // SAFETY: pidfd_open takes only a numeric PID and zero flags and
        // returns a new descriptor owned by the caller.
        let raw_fd = unsafe { libc::syscall(libc::SYS_pidfd_open, native_pid, 0_u32) };
        if raw_fd < 0 {
            return Err(CallerSessionError::OpenPidFd(io::Error::last_os_error()));
        }
        // SAFETY: a successful pidfd_open returns one new owned descriptor.
        let pidfd = unsafe { OwnedFd::from_raw_fd(raw_fd as RawFd) };
        let pinned = Self {
            pid,
            start_time_ticks,
            pidfd,
        };
        pinned.revalidate()?;
        Ok(pinned)
    }

    fn revalidate(&self) -> Result<(), CallerSessionError> {
        if self.has_exited()? {
            return Err(CallerSessionError::CallerExited);
        }
        let actual = process_start_time_ticks(self.pid)?;
        if actual != self.start_time_ticks {
            return Err(CallerSessionError::ProcessIdentityChanged {
                expected: self.start_time_ticks,
                actual,
            });
        }
        if self.has_exited()? {
            return Err(CallerSessionError::CallerExited);
        }
        Ok(())
    }

    fn has_exited(&self) -> Result<bool, CallerSessionError> {
        let mut descriptor = libc::pollfd {
            fd: self.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `descriptor` points to one initialized pollfd and a zero
        // timeout makes this a nonblocking liveness check.
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if result < 0 {
            return Err(CallerSessionError::PollPidFd(io::Error::last_os_error()));
        }
        Ok(result > 0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GraphicalSession {
    id: String,
    uid: u32,
    seat: String,
    session_type: GraphicalSessionType,
}

#[derive(Debug, Error)]
pub enum CallerSessionError {
    #[error("D-Bus caller UID {actual} differs from daemon UID {expected}")]
    WrongUid { expected: u32, actual: u32 },
    #[error("D-Bus caller PID {0} is invalid")]
    InvalidPid(u32),
    #[error("read caller process identity: {0}")]
    ProcessIdentity(#[source] io::Error),
    #[error("caller process start time changed from {expected} to {actual}")]
    ProcessIdentityChanged { expected: u64, actual: u64 },
    #[error("open caller pidfd: {0}")]
    OpenPidFd(#[source] io::Error),
    #[error("poll caller pidfd: {0}")]
    PollPidFd(#[source] io::Error),
    #[error("D-Bus caller exited before setup began")]
    CallerExited,
    #[error("register caller pidfd with Tokio: {0}")]
    RegisterPidFd(#[source] io::Error),
    #[error("caller identity task failed: {0}")]
    PinTask(String),
    #[error("resolve caller login session: {0}")]
    ResolveSession(Errno),
    #[error("enumerate active sessions for caller UID: {0}")]
    EnumerateSessions(Errno),
    #[error("caller UID has more than {maximum} active login sessions")]
    TooManySessions { maximum: usize },
    #[error("login session metadata query failed: {0}")]
    SessionMetadata(Errno),
    #[error("login session metadata contains invalid UTF-8")]
    InvalidSessionText,
    #[error("login session {field} is empty, too long, or contains a control character")]
    InvalidSessionField { field: &'static str },
    #[error("caller UID has no active local graphical login session")]
    NoGraphicalSession,
    #[error("caller UID has multiple active local graphical login sessions: {0:?}")]
    AmbiguousGraphicalSessions(Vec<String>),
}

fn process_start_time_ticks(pid: u32) -> Result<u64, CallerSessionError> {
    let path = format!("/proc/{pid}/stat");
    let file = std::fs::File::open(path).map_err(CallerSessionError::ProcessIdentity)?;
    let mut contents = Vec::with_capacity(512);
    file.take(MAX_PROC_STAT_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(CallerSessionError::ProcessIdentity)?;
    if contents.len() as u64 > MAX_PROC_STAT_BYTES {
        return Err(invalid_process_identity("process stat record is too long"));
    }
    parse_process_start_time_ticks(&contents).map_err(CallerSessionError::ProcessIdentity)
}

fn parse_process_start_time_ticks(contents: &[u8]) -> Result<u64, io::Error> {
    // The comm field is parenthesized and may itself contain spaces or right
    // parentheses. All fields after its final ')' are numeric except state.
    let comm_end = contents
        .iter()
        .rposition(|byte| *byte == b')')
        .ok_or_else(|| invalid_data("process stat record has no comm terminator"))?;
    let mut fields = contents
        .get(comm_end + 1..)
        .ok_or_else(|| invalid_data("process stat record is truncated"))?
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty());
    // starttime is field 22; this iterator begins at field 3 (state).
    let start_time = fields
        .nth(19)
        .ok_or_else(|| invalid_data("process stat record has no start time"))?;
    let start_time = std::str::from_utf8(start_time)
        .map_err(|_| invalid_data("process start time is not ASCII"))?;
    start_time
        .parse()
        .map_err(|_| invalid_data("process start time is not an integer"))
}

fn invalid_process_identity(message: &'static str) -> CallerSessionError {
    CallerSessionError::ProcessIdentity(invalid_data(message))
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn pin_caller_identity(
    pid: u32,
    uid: u32,
    expected_uid: u32,
) -> Result<(PinnedProcess, GraphicalSession), CallerSessionError> {
    let pinned = PinnedProcess::pin(pid, uid, expected_uid)?;
    let session = resolve_graphical_session(pid, uid)?;
    pinned.revalidate()?;
    Ok((pinned, session))
}

fn resolve_graphical_session(pid: u32, uid: u32) -> Result<GraphicalSession, CallerSessionError> {
    if let Some(session_id) = session_for_pid(pid)? {
        if let Some(session) = inspect_graphical_session(&session_id, uid)? {
            return Ok(session);
        }
    }

    let mut session_ids = active_sessions_for_uid(uid)?;
    session_ids.sort_unstable();
    session_ids.dedup();
    let mut candidates = Vec::with_capacity(session_ids.len());
    for session_id in session_ids {
        if let Some(session) = inspect_graphical_session(&session_id, uid)? {
            candidates.push(session);
        }
    }
    select_unique_graphical_session(candidates)
}

fn session_for_pid(pid: u32) -> Result<Option<String>, CallerSessionError> {
    let native_pid = i32::try_from(pid).map_err(|_| CallerSessionError::InvalidPid(pid))?;
    let mut session = std::ptr::null_mut();
    // SAFETY: `session` points to writable storage. On success libsystemd
    // returns one malloc-allocated string owned by the caller.
    let result = unsafe { sd_pid_get_session(native_pid, &mut session) };
    if result < 0 {
        return match errno_from_negative(result) {
            Errno::ENODATA | Errno::ENXIO => Ok(None),
            error => Err(CallerSessionError::ResolveSession(error)),
        };
    }
    let session =
        LibcString::new(session).ok_or(CallerSessionError::SessionMetadata(Errno::EPROTO))?;
    validate_login_field(session.as_str()?, "session ID", MAX_LOGIN_SESSION_ID_BYTES).map(Some)
}

fn active_sessions_for_uid(uid: u32) -> Result<Vec<String>, CallerSessionError> {
    let mut sessions = std::ptr::null_mut();
    // SAFETY: `sessions` points to writable storage. The positive
    // `require_active` argument requests only foreground sessions.
    let result = unsafe { sd_uid_get_sessions(uid, 1, &mut sessions) };
    if result < 0 {
        return Err(CallerSessionError::EnumerateSessions(errno_from_negative(
            result,
        )));
    }
    let count = usize::try_from(result).expect("nonnegative c_int fits usize");
    if count > MAX_ACTIVE_LOGIN_SESSIONS {
        // The returned vector remains owned even when policy rejects its size.
        let _sessions = LibcStringArray::new(sessions, count);
        return Err(CallerSessionError::TooManySessions {
            maximum: MAX_ACTIVE_LOGIN_SESSIONS,
        });
    }
    if count == 0 {
        if !sessions.is_null() {
            // SAFETY: libsystemd returned an empty malloc-allocated vector.
            unsafe { libc::free(sessions.cast()) };
        }
        return Ok(Vec::new());
    }
    let sessions = LibcStringArray::new(sessions, count)
        .ok_or(CallerSessionError::SessionMetadata(Errno::EPROTO))?;
    let mut ids = Vec::with_capacity(count);
    for index in 0..count {
        let text = sessions
            .get(index)
            .ok_or(CallerSessionError::SessionMetadata(Errno::EPROTO))?;
        ids.push(validate_login_field(
            text,
            "session ID",
            MAX_LOGIN_SESSION_ID_BYTES,
        )?);
    }
    Ok(ids)
}

fn inspect_graphical_session(
    session_id: &str,
    expected_uid: u32,
) -> Result<Option<GraphicalSession>, CallerSessionError> {
    let session_id =
        CString::new(session_id).map_err(|_| CallerSessionError::InvalidSessionField {
            field: "session ID",
        })?;
    if !session_boolean(&session_id, sd_session_is_active)? {
        return Ok(None);
    }
    if session_boolean(&session_id, sd_session_is_remote)? {
        return Ok(None);
    }

    let mut uid = 0_u32;
    // SAFETY: `session_id` is NUL-terminated and `uid` is writable.
    let result = unsafe { sd_session_get_uid(session_id.as_ptr(), &mut uid) };
    if metadata_is_absent(result)? || uid != expected_uid {
        return Ok(None);
    }

    let Some(session_type) = query_session_string(&session_id, sd_session_get_type)? else {
        return Ok(None);
    };
    let session_type = match session_type.as_str() {
        "x11" => GraphicalSessionType::X11,
        "wayland" => GraphicalSessionType::Wayland,
        "mir" => GraphicalSessionType::Mir,
        _ => return Ok(None),
    };

    let Some(session_class) = query_session_string(&session_id, sd_session_get_class)? else {
        return Ok(None);
    };
    if !matches!(
        session_class.as_str(),
        "user" | "user-early" | "user-light" | "user-early-light"
    ) {
        return Ok(None);
    }

    let Some(seat) = query_session_string(&session_id, sd_session_get_seat)? else {
        return Ok(None);
    };
    let seat = validate_login_field(seat, "seat", MAX_SEAT_ID_BYTES)?;

    // Recheck activity after the metadata reads so a transition during the
    // query cannot yield a newly inactive candidate.
    if !session_boolean(&session_id, sd_session_is_active)? {
        return Ok(None);
    }

    Ok(Some(GraphicalSession {
        id: session_id
            .to_str()
            .expect("session ID originated as UTF-8")
            .to_owned(),
        uid,
        seat,
        session_type,
    }))
}

type SessionBooleanQuery = unsafe extern "C" fn(*const c_char) -> c_int;
type SessionStringQuery = unsafe extern "C" fn(*const c_char, *mut *mut c_char) -> c_int;

fn session_boolean(
    session_id: &CStr,
    query: SessionBooleanQuery,
) -> Result<bool, CallerSessionError> {
    // SAFETY: `session_id` is NUL-terminated for the duration of this call.
    let result = unsafe { query(session_id.as_ptr()) };
    if result >= 0 {
        Ok(result > 0)
    } else {
        match errno_from_negative(result) {
            Errno::ENODATA | Errno::ENXIO => Ok(false),
            error => Err(CallerSessionError::SessionMetadata(error)),
        }
    }
}

fn query_session_string(
    session_id: &CStr,
    query: SessionStringQuery,
) -> Result<Option<String>, CallerSessionError> {
    let mut value = std::ptr::null_mut();
    // SAFETY: `session_id` is NUL-terminated and `value` points to writable
    // storage for the returned malloc-allocated string.
    let result = unsafe { query(session_id.as_ptr(), &mut value) };
    if result < 0 {
        return match errno_from_negative(result) {
            Errno::ENODATA | Errno::ENXIO => Ok(None),
            error => Err(CallerSessionError::SessionMetadata(error)),
        };
    }
    let value = LibcString::new(value).ok_or(CallerSessionError::SessionMetadata(Errno::EPROTO))?;
    value.as_str().map(str::to_owned).map(Some)
}

fn metadata_is_absent(result: c_int) -> Result<bool, CallerSessionError> {
    if result >= 0 {
        Ok(false)
    } else {
        match errno_from_negative(result) {
            Errno::ENODATA | Errno::ENXIO => Ok(true),
            error => Err(CallerSessionError::SessionMetadata(error)),
        }
    }
}

fn validate_login_field(
    value: impl AsRef<str>,
    field: &'static str,
    maximum_bytes: usize,
) -> Result<String, CallerSessionError> {
    let value = value.as_ref();
    if value.is_empty() || value.len() > maximum_bytes || value.chars().any(char::is_control) {
        return Err(CallerSessionError::InvalidSessionField { field });
    }
    Ok(value.to_owned())
}

fn select_unique_graphical_session(
    mut candidates: Vec<GraphicalSession>,
) -> Result<GraphicalSession, CallerSessionError> {
    candidates.sort_unstable_by(|left, right| left.id.cmp(&right.id));
    match candidates.len() {
        0 => Err(CallerSessionError::NoGraphicalSession),
        1 => Ok(candidates.remove(0)),
        _ => Err(CallerSessionError::AmbiguousGraphicalSessions(
            candidates.into_iter().map(|session| session.id).collect(),
        )),
    }
}

struct LibcString(NonNull<c_char>);

impl LibcString {
    fn new(value: *mut c_char) -> Option<Self> {
        NonNull::new(value).map(Self)
    }

    fn as_str(&self) -> Result<&str, CallerSessionError> {
        // SAFETY: libsystemd returned a live NUL-terminated string and this
        // wrapper owns it for the duration of the borrow.
        unsafe { CStr::from_ptr(self.0.as_ptr()) }
            .to_str()
            .map_err(|_| CallerSessionError::InvalidSessionText)
    }
}

impl Drop for LibcString {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns exactly one libc-allocated string.
        unsafe { libc::free(self.0.as_ptr().cast()) };
    }
}

struct LibcStringArray {
    values: NonNull<*mut c_char>,
    count: usize,
}

impl LibcStringArray {
    fn new(values: *mut *mut c_char, count: usize) -> Option<Self> {
        NonNull::new(values).map(|values| Self { values, count })
    }

    fn get(&self, index: usize) -> Option<&str> {
        if index >= self.count {
            return None;
        }
        // SAFETY: `index` is within the count returned by libsystemd.
        let value = unsafe { *self.values.as_ptr().add(index) };
        let value = NonNull::new(value)?;
        // SAFETY: each non-null vector member is a live NUL-terminated string.
        unsafe { CStr::from_ptr(value.as_ptr()) }.to_str().ok()
    }
}

impl Drop for LibcStringArray {
    fn drop(&mut self) {
        for index in 0..self.count {
            // SAFETY: all members within `count` belong to this returned
            // vector; free accepts null members as well.
            let value = unsafe { *self.values.as_ptr().add(index) };
            // SAFETY: each member is either null or malloc-allocated.
            unsafe { libc::free(value.cast()) };
        }
        // SAFETY: this wrapper owns the malloc-allocated pointer vector.
        unsafe { libc::free(self.values.as_ptr().cast()) };
    }
}

fn errno_from_negative(result: c_int) -> Errno {
    debug_assert!(result < 0);
    Errno::from_raw(-result)
}

#[link(name = "systemd")]
extern "C" {
    fn sd_pid_get_session(pid: libc::pid_t, session: *mut *mut c_char) -> c_int;
    fn sd_uid_get_sessions(
        uid: libc::uid_t,
        require_active: c_int,
        sessions: *mut *mut *mut c_char,
    ) -> c_int;
    fn sd_session_is_active(session: *const c_char) -> c_int;
    fn sd_session_is_remote(session: *const c_char) -> c_int;
    fn sd_session_get_uid(session: *const c_char, uid: *mut libc::uid_t) -> c_int;
    fn sd_session_get_seat(session: *const c_char, seat: *mut *mut c_char) -> c_int;
    fn sd_session_get_type(session: *const c_char, session_type: *mut *mut c_char) -> c_int;
    fn sd_session_get_class(session: *const c_char, session_class: *mut *mut c_char) -> c_int;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, seat: &str, session_type: GraphicalSessionType) -> GraphicalSession {
        GraphicalSession {
            id: id.to_owned(),
            uid: nix::unistd::getuid().as_raw(),
            seat: seat.to_owned(),
            session_type,
        }
    }

    #[test]
    fn parses_proc_stat_after_parentheses_in_comm() {
        let mut fields = vec!["S"; 19];
        fields.push("424242");
        fields.extend(["0", "0"]);
        let record = format!("99 (odd ) process name) {}\n", fields.join(" "));
        assert_eq!(
            parse_process_start_time_ticks(record.as_bytes()).unwrap(),
            424242
        );
    }

    #[test]
    fn rejects_truncated_proc_stat() {
        let error = parse_process_start_time_ticks(b"99 (short) S 1 2")
            .expect_err("a stat record without field 22 must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn pins_and_revalidates_the_current_process() {
        let uid = nix::unistd::getuid().as_raw();
        let process = PinnedProcess::pin(std::process::id(), uid, uid).unwrap();
        assert_eq!(
            process.start_time_ticks,
            process_start_time_ticks(process.pid).unwrap()
        );
        assert!(!process.has_exited().unwrap());
        process.revalidate().unwrap();
    }

    #[test]
    fn rejects_a_wrong_caller_uid_before_opening_proc() {
        let uid = nix::unistd::getuid().as_raw();
        let different_uid = if uid == 0 { 1 } else { 0 };
        assert!(matches!(
            PinnedProcess::pin(u32::MAX, uid, different_uid),
            Err(CallerSessionError::WrongUid { .. })
        ));
    }

    #[test]
    fn graphical_session_selection_is_exact_and_deterministic() {
        assert!(matches!(
            select_unique_graphical_session(Vec::new()),
            Err(CallerSessionError::NoGraphicalSession)
        ));
        assert_eq!(
            select_unique_graphical_session(vec![session(
                "3",
                "seat0",
                GraphicalSessionType::Wayland
            )])
            .unwrap(),
            session("3", "seat0", GraphicalSessionType::Wayland)
        );
        assert!(matches!(
            select_unique_graphical_session(vec![
                session("8", "seat1", GraphicalSessionType::X11),
                session("3", "seat0", GraphicalSessionType::Wayland),
            ]),
            Err(CallerSessionError::AmbiguousGraphicalSessions(ids))
                if ids == vec!["3".to_owned(), "8".to_owned()]
        ));
    }

    #[test]
    fn validates_bounded_login_fields() {
        assert_eq!(
            validate_login_field("seat0", "seat", MAX_SEAT_ID_BYTES).unwrap(),
            "seat0"
        );
        for invalid in ["", "seat\n0"] {
            assert!(matches!(
                validate_login_field(invalid, "seat", MAX_SEAT_ID_BYTES),
                Err(CallerSessionError::InvalidSessionField { field: "seat" })
            ));
        }
        let too_long = "s".repeat(MAX_LOGIN_SESSION_ID_BYTES + 1);
        assert!(matches!(
            validate_login_field(too_long, "session ID", MAX_LOGIN_SESSION_ID_BYTES),
            Err(CallerSessionError::InvalidSessionField {
                field: "session ID"
            })
        ));
    }
}
